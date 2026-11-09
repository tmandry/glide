// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end test of updating Glide, run by hand in a Tart VM.
//!
//! ```text
//! cargo run --example update_vm_test -- --vm <name>
//! ```
//!
//! On the host, this packages a signed Glide.app, copies it and this binary
//! into the VM, and runs the test there. In the VM, the test installs the
//! bundle, launches it as if it were an older version, serves a fake update
//! site offering the same bundle, and checks that `glide update install`
//! swaps it in and restarts Glide with its state restored.
//!
//! The VM needs Accessibility granted to org.glidewm.glide. The test replaces
//! the VM user's Glide config.

use std::fs::{self, File};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, bail, ensure};
use clap::Parser;

#[derive(Parser)]
struct Opt {
    /// The Tart VM to run the test in.
    #[arg(long, required_unless_present = "guest")]
    vm: Option<String>,

    /// Run the test in this machine, installing the zipped bundle at this path.
    #[arg(long, hide = true)]
    guest: Option<PathBuf>,
}

const APP: &str = "/Applications/Glide.app";
const OLD_VERSION: &str = "0.2.10";

fn main() -> anyhow::Result<()> {
    let opt = Opt::parse();
    match (opt.vm, opt.guest) {
        (_, Some(zip)) => guest(&zip),
        (Some(vm), None) => host(&vm),
        (None, None) => unreachable!(),
    }
}

fn host(vm: &str) -> anyhow::Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let release = root.join("target/release");
    let zip = release.join("Glide.app.zip");

    step("Packaging Glide.app");
    run(Command::new("cargo").args(["build", "--release"]).current_dir(root))?;
    run(Command::new("cargo")
        .args(["packager", "--release", "--formats", "app"])
        .current_dir(root))?;
    remove_file(&zip)?;
    run(Command::new("ditto")
        .args(["-c", "-k", "--keepParent"])
        .arg(release.join("Glide.app"))
        .arg(&zip))?;

    step("Copying to the VM");
    copy_to_vm(vm, &zip, "update-test/Glide.app.zip")?;
    copy_to_vm(vm, &std::env::current_exe()?, "update-test/update_vm_test")?;

    step("Running the test in the VM");
    run(Command::new("tart").args([
        "exec",
        vm,
        "sh",
        "-c",
        "chmod +x ~/update-test/update_vm_test && \
         ~/update-test/update_vm_test --guest ~/update-test/Glide.app.zip",
    ]))
}

fn copy_to_vm(vm: &str, source: &Path, dest: &str) -> anyhow::Result<()> {
    run(Command::new("tart")
        .args(["exec", "-i", vm, "sh", "-c"])
        .arg(format!("mkdir -p \"$(dirname ~/{dest})\" && cat > ~/{dest}"))
        .stdin(File::open(source)?))
}

fn guest(zip: &Path) -> anyhow::Result<()> {
    let app = Path::new(APP);
    let glide = app.join("Contents/MacOS/glide");
    let home = dirs::home_dir().context("no home directory")?;
    let data_dir = home.join(".glide");
    let log_dir = home.join("Library/Logs/Glide");

    step("Installing the bundle");
    _ = Command::new("pkill").args(["-x", "glide_server"]).status();
    wait_for("the old server to exit", || {
        Ok(server_pid()?.is_none().then_some(()))
    })?;
    remove_dir_all(app)?;
    run(Command::new("ditto").args(["-x", "-k"]).arg(zip).arg("/Applications"))?;
    let version = output(Command::new(&glide).arg("--version"))?
        .trim()
        .trim_start_matches("glide ")
        .to_owned();
    for file in ["state.ron", "update.log"] {
        remove_file(&data_dir.join(file))?;
    }
    remove_dir_all(&log_dir)?;
    // Updates restore the saved state even with this off.
    let config_dir = home.join(".config/glide");
    fs::create_dir_all(&config_dir)?;
    fs::write(
        config_dir.join("glide.toml"),
        "[settings]\nauto_restore = false\n",
    )?;

    step("Serving a fake update site");
    let base = serve_update_site(zip, &version)?;

    step(&format!("Launching Glide {version} as version {OLD_VERSION}"));
    run(Command::new("open")
        .arg("--env")
        .arg(format!("GLIDE_UPDATE_MANIFEST_URL={base}/update/v1"))
        .arg("--env")
        .arg(format!("GLIDE_UPDATE_RELEASES_URL={base}/releases"))
        .arg("--env")
        .arg(format!("GLIDE_UPDATE_CURRENT_VERSION={OLD_VERSION}"))
        .arg(app))?;
    let old_pid = wait_for("Glide to start", server_pid)?;
    wait_for("Glide to respond", || {
        Ok(Command::new(&glide).arg("ping").output()?.status.success().then_some(()))
    })?;
    // Pause to give the saved state something to carry across the update.
    run(Command::new(&glide).arg("pause"))?;

    step("Checking for the update");
    run(Command::new(&glide).args(["update", "check"]))?;

    // Mark the old bundle so we can tell it was replaced.
    // HACK: This breaks the bundle's signature, so it has to come after the
    // updater has checked it for the current bundle. The `update check` above
    // waits for this.
    let marker = app.join("Contents/update-test-marker");
    File::create(&marker)?;
    ensure!(marker.exists(), "could not create {}", marker.display());

    step("Installing the update");
    run(Command::new(&glide).args(["update", "install"]))?;

    step("Checking the new version");
    let new_pid = wait_for("the new version to start", || {
        Ok(server_pid()?.filter(|pid| *pid != old_pid))
    })?;
    wait_for("the new version to respond", || {
        let out = Command::new(&glide).arg("version").output()?;
        let out = String::from_utf8_lossy(&out.stdout);
        Ok(out.contains(&format!("server: {version}")).then_some(()))
    })?;
    let started = format!("started with pid {new_pid}\n");
    wait_for("the new version to restore its state", || {
        let restored = read_logs(&log_dir)?
            .split_once(&started)
            .is_some_and(|(_, log)| log.contains("Restoring state saved by version"));
        Ok(restored.then_some(()))
    })?;
    let restored = fs::read_to_string(data_dir.join("state.ron"))?;
    ensure!(
        restored.contains("globally_enabled:false"),
        "restored state didn't keep Glide paused"
    );
    let update_log = fs::read_to_string(data_dir.join("update.log"))?;
    ensure!(
        update_log.contains("Installed; launching"),
        "unexpected update log:\n{update_log}"
    );
    for entry in fs::read_dir("/Applications")? {
        let name = entry?.file_name();
        ensure!(
            !name.to_string_lossy().starts_with(".Glide.app."),
            "left behind /Applications/{}",
            name.to_string_lossy()
        );
    }
    ensure!(!marker.exists(), "{} was not replaced", app.display());
    run(Command::new(&glide).arg("resume"))?;

    println!("\nPASS");
    Ok(())
}

/// Serve a manifest and GitHub-style release offering the bundle at `zip`,
/// and return the base URL.
fn serve_update_site(zip: &Path, version: &str) -> anyhow::Result<String> {
    let server = tiny_http::Server::http("127.0.0.1:0").map_err(|e| anyhow::anyhow!(e))?;
    let port = server.server_addr().to_ip().context("server has no IP address")?.port();
    let base = format!("http://127.0.0.1:{port}");
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x64"
    };
    let zip_path = format!("/Glide_{version}_{arch}.app.zip");
    let manifest = r#"{"enabled": true, "version": "latest"}"#.to_owned();
    let release = format!(
        r#"{{"tag_name": "v{version}", "assets": [
            {{"name": "{name}", "browser_download_url": "{base}{zip_path}"}}
        ]}}"#,
        name = &zip_path[1..],
    );
    let zip = fs::read(zip)?;
    thread::spawn(move || {
        for request in server.incoming_requests() {
            let body = match request.url() {
                "/update/v1/default.json" => Some(manifest.clone().into_bytes()),
                "/releases/latest" => Some(release.clone().into_bytes()),
                url if url == zip_path => Some(zip.clone()),
                _ => None,
            };
            println!("  {} {}", if body.is_some() { 200 } else { 404 }, request.url());
            _ = match body {
                Some(body) => request.respond(tiny_http::Response::from_data(body)),
                None => request.respond(tiny_http::Response::empty(404)),
            };
        }
    });
    Ok(base)
}

fn server_pid() -> anyhow::Result<Option<u32>> {
    let out = Command::new("pgrep").args(["-x", "glide_server"]).output()?;
    Ok(String::from_utf8_lossy(&out.stdout).lines().next().and_then(|l| l.parse().ok()))
}

fn wait_for<T>(what: &str, mut f: impl FnMut() -> anyhow::Result<Option<T>>) -> anyhow::Result<T> {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(120) {
        if let Some(value) = f()? {
            return Ok(value);
        }
        thread::sleep(Duration::from_millis(250));
    }
    bail!("Timed out waiting for {what}")
}

fn remove_file(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

fn remove_dir_all(path: &Path) -> anyhow::Result<()> {
    match fs::remove_dir_all(path) {
        Err(e) if e.kind() != ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// The contents of the server logs in `dir`, oldest first.
fn read_logs(dir: &Path) -> anyhow::Result<String> {
    let mut paths =
        fs::read_dir(dir)?.map(|e| Ok(e?.path())).collect::<anyhow::Result<Vec<_>>>()?;
    paths.sort();
    Ok(paths.iter().map(fs::read_to_string).collect::<Result<_, _>>()?)
}

fn step(what: &str) {
    println!("--- {what}");
}

fn run(cmd: &mut Command) -> anyhow::Result<()> {
    let status = cmd.status().with_context(|| format!("Could not run {cmd:?}"))?;
    ensure!(status.success(), "{cmd:?} failed with {status}");
    Ok(())
}

fn output(cmd: &mut Command) -> anyhow::Result<String> {
    let out = cmd.output().with_context(|| format!("Could not run {cmd:?}"))?;
    ensure!(out.status.success(), "{cmd:?} failed with {}", out.status);
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
