# agents.md

This file provides guidance to autonomous agents when working with code in this repository.

If present, see @agents.local.md for additional instructions.

## Project

Glide is a tiling window manager for macOS, written in Rust. It manages windows via the Accessibility API and private SkyLight framework. macOS-only (aarch64 and x86_64 Apple Darwin).

## Build and Run Commands

```bash
cargo build                    # Debug build
cargo build --release          # Release build
cargo test                     # Run all tests
cargo test --lib               # Run library tests only
cargo test model::layout_tree  # Run tests in a specific module
cargo +nightly fmt --check     # Check formatting
cargo +nightly fmt             # Format code
```

For automation and agent sessions, do not run `cargo run`, `cargo run --release`, or `glide launch` because they start the live window manager. If you need runtime investigation, prefer tests, record/replay artifacts supplied by the user, or `devtool`.

**Developer tools:**
```bash
cargo run --example devtool                           # List devtool commands
cargo run --example devtool -- list ax                # List windows via accessibility
cargo run --example devtool -- replay traces/foo.ron  # Replay a recorded trace
```

**Logging** uses the `RUST_LOG` env var via the `tracing` crate:
```bash
RUST_LOG=info cargo run --example devtool -- replay traces/foo.ron
RUST_LOG=info,glide_wm::actor::reactor=debug cargo run --example devtool -- replay traces/foo.ron
```

Ask user to **record and replay** for debugging the Reactor:
```bash
RUST_LOG=info cargo run -- --record traces/trace-$(date +%Y%m%d-%H%M%S).ron
```

**Website:**
```bash
cd site
npm run dev
npm run build
npm run preview
```

## Architecture

Three layers with strict dependency rules: `actor → model → sys (geometry types only)`, `actor → sys`.

- **sys** (`src/sys/`): Thin wrappers around macOS APIs (Accessibility, SkyLight, screens/spaces, event taps, CFRunLoop). No business logic. Types like `SpaceId`, `WindowServerId`, `CGRect` are used throughout.
- **model** (`src/model/`): Pure data structures and algorithms with NO side effects. No I/O, system calls, or clock reads. Time-dependent methods accept an `Instant` parameter for determinism.
- **actor** (`src/actor/`): Event-driven actors with MPSC channels. All I/O and OS interaction happens here.
- **config** (`src/config/`): Layered merge system (embedded defaults from `glide.default.toml` → user config → validation). Uses `partial_config!` macro for partial types.
- **ui** (`src/ui/`): Native AppKit components (group bars, status icon, permission flow).

### Key Actors

- **Reactor** (`actor/reactor.rs`): Central event processor maintaining coherence between system and model state. Contains the LayoutManager inline.
- **WmController** (`actor/wm_controller.rs`): Top-level orchestrator on main thread.
- **App** (`actor/app.rs`): One per managed process, on dedicated threads. Reads/writes window frames via Accessibility API.
- **WindowServer** (`actor/window_server.rs`): Mirrors macOS window server, owns ScreenCache.
- **LayoutManager** (`actor/layout.rs`): Embedded in Reactor. Converts events into tree operations, calculates window frames.

### Core Data Structures

- **LayoutTree** (`model/layout_tree.rs`): Central data structure wrapping `Tree<Components>` with three observers: Size (weight-based), Selection (selected path), Window (leaf ↔ WindowId mapping).
- **Tree** (`model/tree.rs`): Generic N-ary tree backed by `SlotMap<NodeId, Node>`. Uses observer pattern for structural mutations. `OwnedNode` is RAII with typestate enforcement (`UnattachedNode`, `DetachedNode`, `ReattachedNode`).
- **SpaceLayoutMapping** (`model/layout_mapping.rs`): Per-space layouts with copy-on-write per screen size.

### Transaction System

Each window has a `TransactionId` tracking the last write. Stale reads from app threads (events that arrived before the app processed the last write) are ignored to prevent feedback loops from accessibility API delays.

## Design Principles

- **Keep the model pure** – no side effects, I/O, or clock reads in `model/`
- **Validate at the boundary** – clamp/filter config values in `validated()`; defense-in-depth caps on iteration counts
- **Config reload preserves user state** – only unmodified values update on hot-reload
- **Defaults from `glide.default.toml`** – not handwritten `Default` impls with duplicated values
- **Incrementalism** – avoid global initialization/discovery steps; adapt to new info from each app as it becomes available
- **Policy decisions in LayoutManager** – refutable policy decisions, e.g. defaulting certain windows to floating, go through LayoutManager's classification, not scattered across Reactor/App

## Comments

Comments say what the code does. Explain why only when the reason isn't clear
from the surrounding code and this document, or when the logic is subtle enough
that a reader would otherwise assume it's wrong.

Don't argue for a change in a comment. A comment is read by someone who has only
ever seen the current code, so justifying it against what was there before, or
against an alternative that was considered, is noise.

## Testing Patterns

- **Model tests**: Construct a tree, perform operations, assert exact pixel-level frames. No mocking or timing needed.
- **Reactor integration tests**: `Apps` harness simulates app thread responses. `simulate_events()` processes requests and generates responses. `simulate_until_quiet()` runs until no more requests are produced.

## Log Levels

- **Panic**: Bug – fundamental assumption violated, can't recover
- **Error**: Unexpected, recoverable, possibly a bug
- **Warn**: Unexpected or extreme condition; downgrade if fires on common installations
- **Info**: Notable expected events useful for debugging behavior
- **Debug**: Fine-grained state changes and implementation details

## Binaries

- `glide_server` (`src/bin/glide_server.rs`): Main window manager process (default `cargo run` target)
- `glide` (`src/bin/glide.rs`): CLI client communicating via CFMessagePort IPC

## Source Control

Break changes into small and atomic commits. When a feature requires a refactor, do the refactor first in a commit that builds with passing tests. Always run the formatter before making a commit.

If a commit both adds a capability to a lower layer, e.g. a new `sys` wrapper,
and uses it, split the capability into its own commit first. Before a branch is
merged, squash each fix into the commit that introduced the problem; only a bug
that's already on `main` gets its own `fix:` commit. Every commit should build
and pass tests; `scripts/check-each-commit.sh` checks this. The
`polish-branch` skill does all of this, along with an outside review.

For user-facing changes, use commit message prefixes like `feat`, `fix`, `docs`, `improve`, and `perf`. The first line of each message goes into the release notes, categorized by prefix. Use Github conventions to reference issues. For example:

```
fix: Fix a bug where floating windows were forgotten on space change

Fixes #12345.
```

Features should include a short user-facing blurb for the release notes summarizing the feature and how to use it.

When writing or reviewing release notes, follow [docs/release-notes.md](docs/release-notes.md).

Keep the body short: what was wrong and what the change does. Defend the change
only when it reverses an earlier decision, or when it's subtle enough that a
reviewer would otherwise think it's wrong. Weighing alternatives, restating what
the diff plainly shows, and noting the absence of tests are all noise.

Non user-facing changes can use `refactor`, `internal`, `chore`, `build`, `ci`, `style`, `test`, or skip the prefix if none apply.
