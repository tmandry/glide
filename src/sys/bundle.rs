// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, bail};
use objc2::rc::Retained;
use objc2_foundation::{NSBundle, NSString, ns_string};
use tempfile::TempDir;

pub enum BundleError {
    NotInBundle,
    BundleNotGlide { identifier: Retained<NSString> },
}

pub fn glide_bundle() -> Result<Retained<NSBundle>, BundleError> {
    let mut bundle = NSBundle::mainBundle();
    if bundle.bundleIdentifier().is_none()
        && let Some(fallback) = bundle_fallback()
    {
        bundle = fallback;
    }
    match bundle.bundleIdentifier().or_else(|| bundle_fallback()?.bundleIdentifier()) {
        None => Err(BundleError::NotInBundle),
        Some(identifier) if !identifier.containsString(ns_string!("glidewm")) => {
            Err(BundleError::BundleNotGlide { identifier })
        }
        Some(_) => Ok(bundle),
    }
}

fn bundle_fallback() -> Option<Retained<NSBundle>> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let mut bundle = exe;
    bundle.pop();
    if !bundle.ends_with("Contents/MacOS") {
        return None;
    }
    bundle.pop();
    bundle.pop();
    NSBundle::bundleWithPath(&NSString::from_str(bundle.to_str()?))
}

pub fn launch(bundle: &NSBundle, args: &[OsString]) -> anyhow::Result<()> {
    launch_inner(bundle, false, args)
}

pub fn relaunch_current_bundle() -> anyhow::Result<MustExit> {
    let Ok(bundle) = glide_bundle() else {
        bail!("Skipping relaunch because the current application is not Glide");
    };
    launch_inner(&bundle, true, &[]).map(|()| MustExit)
}

fn launch_inner(bundle: &NSBundle, relaunch: bool, args: &[OsString]) -> anyhow::Result<()> {
    let path = bundle.bundlePath().to_string();
    let mut options = OpenLaunchOptions::default();
    if relaunch {
        options.new_instance = true;
    }
    let out = open_path(Path::new(&path), options, args)?;
    if out.status.success() {
        Ok(())
    } else {
        bail!(
            "Launch failed with code {status}. stderr:\n{stderr}\n\nstdout:\n{stdout}",
            status = out.status,
            stderr = String::from_utf8_lossy(&out.stderr),
            stdout = String::from_utf8_lossy(&out.stdout)
        )
    }
}

#[must_use = "Callers must immediately exit the process after reporting success"]
pub struct MustExit;
impl Drop for MustExit {
    fn drop(&mut self) {
        panic!("Must exit after relaunch");
    }
}

const EXEC_HELPER_APP_NAME: &str = "GlideExec.app";
const EXEC_HELPER_EXECUTABLE_NAME: &str = "glide_exec";
const EXEC_HELPER_BUNDLE_IDENTIFIER: &str = "org.glidewm.exec-helper";
const EXEC_RESULT_DIR_PREFIX: &str = "glide-exec-result";
const EXEC_RESULT_OUTPUT_FILE: &str = "output";
const EXEC_RESULT_STATUS_FILE: &str = "status";

#[derive(Debug)]
pub struct CommandOutput {
    pub status: i32,
    pub output: String,
}

#[derive(Clone, Copy, Default)]
struct OpenLaunchOptions {
    new_instance: bool,
    background: bool,
    wait_for_exit: bool,
}

/// Launch a command with LaunchServices, dropping Glide's special privileges.
pub fn launch_cli_with_open(args: &[String]) -> anyhow::Result<CommandOutput> {
    let Some(cache_dir) = dirs::cache_dir() else {
        bail!("Could not determine cache directory to create LaunchServices helper");
    };
    launch_cli_with_open_at(&cache_dir, EXEC_HELPER_BUNDLE_IDENTIFIER, args)
}

/// Launch a command through the exec helper script directly, without `open`.
pub fn launch_cli_privileged(args: &[String]) -> anyhow::Result<CommandOutput> {
    let Some(cache_dir) = dirs::cache_dir() else {
        bail!("Could not determine cache directory to create LaunchServices helper");
    };
    launch_cli_privileged_at(cache_dir.as_path(), EXEC_HELPER_BUNDLE_IDENTIFIER, args)
}

fn launch_cli_with_open_at(
    cache_dir: &Path,
    bundle_identifier: &str,
    args: &[String],
) -> anyhow::Result<CommandOutput> {
    launch_cli_with_helper_at(
        cache_dir,
        bundle_identifier,
        args,
        HelperLaunchMode::Open(OpenLaunchOptions {
            new_instance: true,
            background: true,
            wait_for_exit: true,
        }),
    )
}

fn launch_cli_privileged_at(
    cache_dir: &Path,
    bundle_identifier: &str,
    args: &[String],
) -> anyhow::Result<CommandOutput> {
    launch_cli_with_helper_at(cache_dir, bundle_identifier, args, HelperLaunchMode::Direct)
}

enum HelperLaunchMode {
    Open(OpenLaunchOptions),
    Direct,
}

fn launch_cli_with_helper_at(
    cache_dir: &Path,
    bundle_identifier: &str,
    args: &[String],
    launch_mode: HelperLaunchMode,
) -> anyhow::Result<CommandOutput> {
    if args.is_empty() {
        bail!("Empty argument list passed to LaunchServices helper");
    }
    let helper = ensure_exec_helper_app_at(cache_dir, bundle_identifier)?;
    let helper_executable = helper.join("Contents/MacOS").join(EXEC_HELPER_EXECUTABLE_NAME);
    let result_dir = create_exec_result_dir()?;

    let mut helper_args = vec![
        OsString::from("--result-dir"),
        result_dir.path().as_os_str().to_os_string(),
        OsString::from("--"),
    ];
    helper_args.extend(args.iter().map(|arg| OsString::from(arg.as_str())));

    let launch_output = match launch_mode {
        HelperLaunchMode::Open(launch_options) => open_path(&helper, launch_options, &helper_args)?,
        HelperLaunchMode::Direct => exec_helper(&helper_executable, &helper_args)?,
    };
    read_command_output(result_dir.path()).with_context(|| {
        format!(
            "Failed to read helper output. launcher exit: {status}, launcher stderr:\n{stderr}\n\nlauncher stdout:\n{stdout}",
            status = launch_output.status,
            stderr = String::from_utf8_lossy(&launch_output.stderr),
            stdout = String::from_utf8_lossy(&launch_output.stdout),
        )
    })
}

fn open_path(
    path: &Path,
    options: OpenLaunchOptions,
    args: &[OsString],
) -> anyhow::Result<std::process::Output> {
    let mut cmd = Command::new("/usr/bin/open");
    if options.new_instance {
        cmd.arg("-n");
    }
    if options.background {
        cmd.arg("-g").arg("-j");
    }
    if options.wait_for_exit {
        cmd.arg("-W");
    }
    cmd.arg(path).arg("--args");
    for arg in args {
        cmd.arg(arg);
    }
    cmd.output().map_err(|err| anyhow::anyhow!("Launch failed with error: {err}"))
}

fn exec_helper(
    helper_executable: &Path,
    args: &[OsString],
) -> anyhow::Result<std::process::Output> {
    Command::new(helper_executable)
        .args(args)
        .output()
        .map_err(|err| anyhow::anyhow!("Helper launch failed with error: {err}"))
}

fn create_exec_result_dir() -> anyhow::Result<TempDir> {
    tempfile::Builder::new()
        .prefix(EXEC_RESULT_DIR_PREFIX)
        .tempdir()
        .context("Failed to create temporary result directory for exec helper")
}

fn read_command_output(result_dir: &Path) -> anyhow::Result<CommandOutput> {
    let status = fs::read_to_string(result_dir.join(EXEC_RESULT_STATUS_FILE))
        .with_context(|| format!("Missing status file in {}", result_dir.display()))?;
    let status = status.trim().parse::<i32>().context("Could not parse command exit status")?;

    let output = fs::read_to_string(result_dir.join(EXEC_RESULT_OUTPUT_FILE)).unwrap_or_default();

    Ok(CommandOutput { status, output })
}

fn ensure_exec_helper_app_at(
    cache_dir: &Path,
    bundle_identifier: &str,
) -> anyhow::Result<std::path::PathBuf> {
    let app_path = cache_dir.join(bundle_identifier).join(EXEC_HELPER_APP_NAME);
    let contents_path = app_path.join("Contents");
    let macos_path = contents_path.join("MacOS");
    fs::create_dir_all(&macos_path)?;

    let plist_path = contents_path.join("Info.plist");
    fs::write(plist_path, exec_helper_plist(bundle_identifier))?;

    let executable_path = macos_path.join(EXEC_HELPER_EXECUTABLE_NAME);
    fs::write(&executable_path, exec_helper_script())?;
    let mut permissions = fs::metadata(&executable_path)?.permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&executable_path, permissions)?;

    Ok(app_path)
}

fn exec_helper_plist(bundle_identifier: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleExecutable</key>
    <string>{EXEC_HELPER_EXECUTABLE_NAME}</string>
    <key>CFBundleIdentifier</key>
    <string>{bundle_identifier}</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>GlideExec</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>1.0</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>LSBackgroundOnly</key>
    <true/>
</dict>
</plist>
"#,
    )
}

fn exec_helper_script() -> &'static str {
    r#"#!/bin/sh
if [ "$#" -lt 3 ]; then
  exit 64
fi
if [ "$1" != "--result-dir" ]; then
  exit 64
fi
result_dir="$2"
shift 2
if [ "$1" != "--" ]; then
  exit 64
fi
shift
if [ "$#" -eq 0 ]; then
  exit 64
fi
"$@" >"$result_dir/output" 2>&1
status=$?
printf '%s\n' "$status" >"$result_dir/status"
exit "$status"
"#
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn ensure_exec_helper_app_uses_provided_directory_and_bundle_identifier() {
        let temp_dir = tempdir().unwrap();
        let test_bundle_identifier = format!("{EXEC_HELPER_BUNDLE_IDENTIFIER}.test");

        let app_path = ensure_exec_helper_app_at(temp_dir.path(), &test_bundle_identifier).unwrap();

        let expected_base = temp_dir.path().join(&test_bundle_identifier);
        assert_eq!(app_path, expected_base.join(EXEC_HELPER_APP_NAME));

        let plist_path = app_path.join("Contents/Info.plist");
        let executable_path =
            app_path.join(format!("Contents/MacOS/{EXEC_HELPER_EXECUTABLE_NAME}"));

        assert!(plist_path.exists());
        assert!(executable_path.exists());

        let plist = fs::read_to_string(plist_path).unwrap();
        assert!(plist.contains(&test_bundle_identifier));
        assert!(plist.contains(EXEC_HELPER_EXECUTABLE_NAME));

        let script = fs::read_to_string(&executable_path).unwrap();
        assert!(script.contains("--result-dir"));
        assert!(script.contains("\"$@\" >\"$result_dir/output\" 2>&1"));

        let mode = fs::metadata(executable_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o700, 0o700);
    }

    #[test]
    fn launch_cli_with_open_runs_command_through_helper() {
        let temp_dir = tempdir().unwrap();
        let test_bundle_identifier =
            format!("{EXEC_HELPER_BUNDLE_IDENTIFIER}.test.{}", std::process::id());

        let output_file = temp_dir.path().join("open-helper-ran.txt");
        let command = vec![
            "/usr/bin/touch".to_owned(),
            output_file.to_str().unwrap().to_owned(),
        ];

        launch_cli_with_open_at(temp_dir.path(), &test_bundle_identifier, &command).unwrap();

        assert!(
            output_file.exists(),
            "Expected helper-launched command to create {}",
            output_file.display()
        );
    }

    #[test]
    fn launch_cli_with_open_captures_failure_output() {
        let temp_dir = tempdir().unwrap();
        let test_bundle_identifier =
            format!("{EXEC_HELPER_BUNDLE_IDENTIFIER}.test.{}", std::process::id());
        let command = vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "echo helper-stdout; echo helper-stderr >&2; exit 17".to_owned(),
        ];

        let output =
            launch_cli_with_open_at(temp_dir.path(), &test_bundle_identifier, &command).unwrap();

        assert_eq!(output.status, 17);
        assert!(output.output.contains("helper-stdout"));
        assert!(output.output.contains("helper-stderr"));
    }

    #[test]
    fn launch_cli_privileged_runs_command_through_helper_script() {
        let temp_dir = tempdir().unwrap();
        let test_bundle_identifier =
            format!("{EXEC_HELPER_BUNDLE_IDENTIFIER}.test.{}", std::process::id());
        let command = vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "echo privileged-stdout; echo privileged-stderr >&2; exit 23".to_owned(),
        ];

        let output =
            launch_cli_privileged_at(temp_dir.path(), &test_bundle_identifier, &command).unwrap();

        assert_eq!(output.status, 23);
        assert!(output.output.contains("privileged-stdout"));
        assert!(output.output.contains("privileged-stderr"));
    }
}
