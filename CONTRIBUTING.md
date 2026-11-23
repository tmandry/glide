# Contributing to Glide

Thank you for helping make Glide better! We love to see new contributors join our community.

## How to contribute

Contributions can take the form of bug reports, feature requests, documentation, or code.

We try to keep the codebase accessible to new contributors so you can dive in freely. Help with this is welcome. If something is unclear to you but you figure it out, please open a pull request to make it clearer.

Most significant features should start as [issues] so we can discuss implementation strategy and track progress. However, small fixes can be submitted as pull requests directly.

If you're looking for something to contribute, the issues are a good place to start, particularly open issues marked [help wanted]. To see what's on our roadmap, check the [roadmap] label.

[issues]: https://github.com/glide-wm/glide/issues
[help wanted]: https://github.com/glide-wm/glide/issues?q=is%3Aissue%20state%3Aopen%20label%3A%22help%20wanted%22
[roadmap]: https://github.com/glide-wm/glide/issues?q=is%3Aissue%20state%3Aopen%20label%3Aroadmap

### Contribution philosophy

If you aren't sure how to go about something it is generally best to start discussion early, before you think a PR is "ready". Besides initial discussion in an issue, uploading a draft PR is a great way to do this that can save time in the long run.

We believe *perfection is a process* and do not strive to get everything perfect in the first PR. However, we do prefer to keep the amount of code as small as reasonably possible. Features that are not ready for broad consumption can be kept behind an experimental config flag, which exempts them from normal stability standards.

We prefer code changes that are tested, and may ask for tests before merging. Tests should be manually verified to fail before the behavior change and succeed after. At some layers of the stack (e.g. the system level, app actor, and UI) this is not currently feasible, and we substitute with manual verification and sharing logs or screen captures instead.

Tests must be deterministic. Avoid sleeping, polling until a deadline, or anything else that depends on timing, since those tests flake on slow or busy machines. Instead, give the test a way to wait for the work to finish (for example, by joining a background thread), or pass time in as a parameter.

## Product values and architecture

* **Reliability.** This is the most important. The entire architecture is built around creating order from chaos. We don't introduce new sources of determinism, like timers, whenever possible.
* **Performance.** We the people demand a slick experience.

This combination leads to some architectural principles:

* **Incrementalism.** This is especially apparent in the reactor. Don't try to force a global view when that doesn't mesh with how the OS environment works. Adapt to new information as it becomes available. Generally this looks like avoiding big initialization and discovery steps before doing anything.

This list is a work in progress, and we don't always live up to these values today. But understanding the philosophy can help you understand architectural decisions and how the codebase is meant to evolve.

## Release notes

For user-facing changes, include a short description in your PR of what changes
for users and how to use the feature. This description is input to the release-note
automation, not the final release note; related changes may be combined and the
wording edited. See the [release-note guidelines](docs/release-notes.md). You can
also suggest wording or corrections on the open release PR; the maintainer reviews
the notes before release.

## Development process

Glide is very easy to build, run, and test with cargo. Running tests is as simple as

```
git clone https://github.com/glide-wm/glide
cd glide
cargo test
```

You must have [Rust](https://rustup.rs) and Xcode installed. Xcode can be installed via the Mac App Store.

Running from source is as easy as

```
cargo run
```

The first time you do this, you may have to follow instructions to enable
Accessibility permissions. Instead of enabling them for Glide, enable them for
whatever application your terminal is running in.

### Layout assertions and snapshots

Tests that check where windows end up can state how the windows relate instead of spelling out pixel frames. A `Scene` (in `src/testing/scene.rs`) is a screen plus labelled windows:

```rust
scene.assert("a1").left_of("a2").same_size_as("a2");
scene.assert("b1").wider_than("b2", 20.0);
```

A failed assertion prints a drawing of the scene along with each window's frame. Use exact frames only when the pixels themselves are what the test is about.

A test can also compare the drawing against a snapshot written inline with [expect-test]:

```rust
expect![[r#"
    ┌───────────┬───────────┐
    │ a1        │ a2        │
    └───────────┴───────────┘
"#]]
.assert_eq(&scene.draw());
```

Don't draw these by hand. Write `expect![[""]]`, then have the test fill it in:

```
UPDATE_EXPECT=1 cargo test name_of_the_test
```

The same command rewrites snapshots after an intended change. Read the diff before committing it, since updating accepts whatever the code produces. Each `expect!` can back only one `assert_eq`.

[expect-test]: https://docs.rs/expect-test

### Logging

Glide makes extensive use of the [tracing] crate to provide detailed and contextual logs of what is going on in its internals. You can enable logs by setting the `RUST_LOG` environment variable.

For a tasteful amount of logs, start with `info`:

```
RUST_LOG=info cargo run
```

Often it makes sense to do this plus enable debug logging in a particular module you are developing. For example, to enable debugging in the reactor:

```
RUST_LOG=info,glide_wm::actor::reactor=debug cargo run
```

See the [tracing_subscriber EnvFilter docs](https://docs.rs/tracing-subscriber/0.3.22/tracing_subscriber/filter/struct.EnvFilter.html) for a complete reference of what you can specify here.

[tracing]: https://docs.rs/tracing/latest/tracing/

### Save and restore

If you need to update Glide or restart it for any reason, exit with the
`save_and_exit` key binding (default Alt+Shift+E). Then, when starting again,
run it with the `--restore` flag:

```
cargo run --release -- --restore
```

Glide also saves its state as it changes. To keep a development build from
overwriting your saved state, pass `--no-save` (implied by `--one`). It still
restores your saved state unless you also pass `--no-restore`.

### Building an app bundle

To build an app bundle locally, you have to comment out the line in Cargo.toml
that begins with `macos.signingIdentity`. Then you can run:

```
cargo build && cargo packager -f app
```

This depends on cargo-packager, which you can install with `cargo install
cargo-packager@<VERSION>`. Check the current version used in
.github/workflows/package.yml.

This workflow is obviously not ideal. I haven't worked out a better way yet.

### devtool

Glide includes a developer tool called `devtool` that can be used to explore macOS APIs. To see the available commands, run it with no arguments like so:

```
❯ cargo run --example devtool
   Compiling glide-wm v0.1.0 (/Users/tyler/code/glide)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 3.81s
     Running `target/debug/examples/devtool`
Usage: devtool [OPTIONS] <COMMAND>

Commands:
  list
  app
  window-server
  replay
  mouse
  inspect
  help           Print this message or the help of the given subcommand(s)

Options:
      --bundle <BUNDLE>
      --verbose
  -h, --help             Print help
```

To list the windows on screen using the accessibility APIs, use the `list ax` command. Note the use of `--` to separate arguments passed to cargo from arguments passed to devtool.

```
❯ cargo run --example devtool -- list ax
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.07s
     Running `target/debug/examples/devtool list ax`
WindowInfo { is_standard: false, title: "glide: cargo run --example devtool -- list ax", frame: CGRect { origin: CGPoint { x: 0.0, y: 39.0 }, size: CGSize { width: 1800.0, height: 1050.0 } }, sys_id: Some(WindowServerId(620)) } from com.googlecode.iterm2
WindowInfo { is_standard: true, title: "installing xcode - Google Search - Google Chrome", frame: CGRect { origin: CGPoint { x: -0.0, y: 40.0 }, size: CGSize { width: 606.0, height: 1129.0 } }, sys_id: Some(WindowServerId(89)) } from com.google.Chrome
WindowInfo { is_standard: true, title: "glide — CONTRIBUTING.md", frame: CGRect { origin: CGPoint { x: 606.0, y: 40.0 }, size: CGSize { width: 1188.0, height: 1129.0 } }, sys_id: Some(WindowServerId(16811)) } from dev.zed.Zed
WindowInfo { is_standard: true, title: "accessibility — Cargo.toml", frame: CGRect { origin: CGPoint { x: 606.0, y: 40.0 }, size: CGSize { width: 1188.0, height: 1129.0 } }, sys_id: Some(WindowServerId(16812)) } from dev.zed.Zed
WindowInfo { is_standard: false, title: "", frame: CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: CGSize { width: 1800.0, height: 1169.0 } }, sys_id: None } from com.apple.finder
```

### Record and replay

Another very useful debugging tool for Glide is the record/replay feature. Enable recording with the `--record` flag, e.g.

```
mkdir traces/
RUST_LOG=info cargo run -- --record traces/trace-$(date +%Y%m%d-%H%M%S).ron
```

This records a trace of the events and commands passed to the Reactor. From this trace you can replay the events through the reactor using devtool and see what actions it takes as `info` level logs. You can also increase the logging level of any component "behind" the reactor, even if logging was not enabled originally. For example:

```
❯ RUST_LOG=info,glide_wm::actor::layout=debug cargo run --example devtool -- replay traces/trace-20251208-220116.ron
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.08s
     Running `target/debug/examples/devtool replay traces/trace-20251208-220116.ron`
2025-12-09 6:08:31.347928  INFO glide_wm::actor::reactor screen parameters changed
2025-12-09 6:08:31.348716 DEBUG glide_wm::actor::layout event=WindowFocused([], WindowId { pid: 1582, idx: 89 })
2025-12-09 6:08:31.348732 DEBUG glide_wm::actor::layout event=WindowFocused([], WindowId { pid: 1582, idx: 89 })
2025-12-09 6:08:31.350923 DEBUG glide_wm::actor::layout event=WindowRemoved(WindowId { pid: 1601, idx: 620 })
2025-12-09 6:08:31.351744  INFO glide_wm::actor::reactor space changed
2025-12-09 6:08:31.351754 DEBUG glide_wm::actor::layout event=SpaceExposed(SpaceId(8), CGSize { width: 1800.0, height: 1129.0 })
2025-12-09 6:08:31.351813 DEBUG glide_wm::actor::layout Tree
NodeId(20v11) Horizontal [size 0 total=2.3033333; fullscreen]
├─ ☐ NodeId(32v9) WindowId { pid: 1582, idx: 89 } [size 0.77606475]
└─ ☒ NodeId(16v15) Stacked [size 1.5272684 total=3]
    ├─ ☐ NodeId(29v9) WindowId { pid: 1582, idx: 10548 } [size 1]
    ├─ ☒ NodeId(30v9) WindowId { pid: 16960, idx: 16811 } [size 1]
    └─ ☐ NodeId(31v9) WindowId { pid: 16960, idx: 16812 } [size 1]
2025-12-09 6:08:31.656719  INFO devtool request=BeginWindowAnimation(WindowId { pid: 1582, idx: 89 })
...
2025-12-09 6:08:31.95875  INFO devtool request=SetWindowFrame(WindowId { pid: 1582, idx: 89 }, CGRect { origin: CGPoint { x: 0.0, y: 40.0 }, size: CGSize { width: 606.0, height: 1129.0 } }, TransactionId(2))
2025-12-09 6:08:31.95879  INFO devtool request=EndWindowAnimation(WindowId { pid: 1582, idx: 89 })
```

Note that the traces do not have a stable format, and may record personal details. If you share them, make sure to sanitize them for anything private and include the exact commit they were created with.

## Website development

Most website and documentation changes can be made directly in markdown.

To preview the website locally you must have [npm] installed. I recommend installing nvm (brew install nvm, follow directions to enable in your shell) and using that to install node and npm.

```
$ cd site/
$ npm run dev
...

 astro  v5.16.4 ready in 1228 ms

┃ Local    http://localhost:4321/
┃ Network  use --host to expose
```

From here you can make changes and see them reflected live. See the [Starlight] and [Astro] docs for more.

[npm]: https://docs.npmjs.com/downloading-and-installing-node-js-and-npm
[Astro]: https://astro.build/
[Starlight]: https://starlight.astro.build/
