# Changelog

## [0.2.13](https://github.com/tmandry/glide/compare/v0.2.12...v0.2.13) (2026-06-06)

After a short hiatus, we're back with the next release of Glide. This release brings better performance, robustness, and a few new commands.


### Features

This release adds three new commands you can bind to keys:

* `focus_next` and `focus_prev` for cycling between windows in the current layout ([#183](https://github.com/tmandry/glide/issues/183)) ([479f270](https://github.com/tmandry/glide/commit/479f270cc37d13376cad176ec179f8298566b2c9))
* `exec_cmd` keybind for running arbitrary commands ([af7878c](https://github.com/tmandry/glide/commit/af7878c4828e63d643fc7aafea279984ad8ee0da))

### Performance Improvements

Animations are now *much* more responsive. Some apps are slow to animate their windows, which can lead to a backlog of frames; this has been solved by always skipping to the latest frame. Additionally, animations no longer queue up behind each other, instead starting immediately.

* Skip stale animation frames when an app is slow to respond ([94fba24](https://github.com/tmandry/glide/commit/94fba2440a91aaa72e342b74a640de58a3586465))
* Run layout animations asynchronously ([2f5ebae](https://github.com/tmandry/glide/commit/2f5ebaebbfdb1c999619a9b0dea7a83b9ff1ced8))

### Improvements and Bug Fixes

Support for windows spanning multiple spaces was improved. These are no longer allowed to disturb the layout window ordering.

* Surface reordered top-layer windows on space change ([3b8cb41](https://github.com/tmandry/glide/commit/3b8cb419a5bc8cb746094495b378b3f0d7015e94))
* Fix a bug where windows moved between screens sometimes had the wrong size ([644e473](https://github.com/tmandry/glide/commit/644e473be16038487a2f78b3ebff5af8181acc60))


## [0.2.12](https://github.com/glide-wm/glide/compare/v0.2.11...v0.2.12) (2026-03-23)


### Bug Fixes

* Fix an issue where focus-follows-mouse would sometimes focus windows underneath the current tab ([2b1360f](https://github.com/glide-wm/glide/commit/2b1360febd068fbf93d69400220e744a8fa60881))
* Fix an issue where Firefox windows and some others would not move correctly, or would animate even when animations were disabled ([6f70a2d](https://github.com/glide-wm/glide/commit/6f70a2db1f896d246fd7eba9cfa15eb41154468c))
* Don't manage Firefox picture-in-picture windows ([adaad2f](https://github.com/glide-wm/glide/commit/adaad2fb724df1f73ade42fbee3870c99b33fb37))
* Emacs Mac windows should now be correctly managed ([#162](https://github.com/glide-wm/glide/issues/162)) ([eb3bf6a](https://github.com/glide-wm/glide/commit/eb3bf6a47dda9c47cdcdd1726ba80932d478b10c))

### Experimental Features

* Scrolling layout, inspired by niri ([#129](https://github.com/glide-wm/glide/issues/129)) ([d39b04b](https://github.com/glide-wm/glide/commit/d39b04bf1f80ae22fbf742b858dd231873896e16))

### Documentation

* Clarify keycodes are independent of keyboard layout ([02bdb13](https://github.com/glide-wm/glide/commit/02bdb13159c525c0d1197d9d91650d4e2a89d302))


## [0.2.11](https://github.com/glide-wm/glide/compare/v0.2.10...v0.2.11) (2026-02-19)


### Bug Fixes

* **config:** `~/.config/glide/config.toml` is now correctly found ([ce2ad5e](https://github.com/glide-wm/glide/commit/ce2ad5e3050452c02e106f40b15475dc5ab510fb))

### Documentation

* development: Add ARCHITECTURE.md ([d0560d3](https://github.com/glide-wm/glide/commit/d0560d3972e0f11c8e8ea79d841a68c8033ae0a1))


## [0.2.10](https://github.com/glide-wm/glide/compare/v0.2.9...v0.2.10) (2026-02-16)


### Bug Fixes

* Jetbrains IDE windows are now correctly managed ([#144](https://github.com/glide-wm/glide/issues/144)).

### Documentation

* Many improvements to documentation, including new [Layout examples](https://glidewm.org/guides/layout-examples/) and [Keyboard-centric setup](https://glidewm.org/guides/keyboard-centric-setup/) pages.


## [0.2.9](https://github.com/glide-wm/glide/compare/v0.2.8...v0.2.9) (2026-02-02)

This release adds the ability to add key bindings to the default set instead of replacing it. Note that this may become the default in the future.

You can also set any default key binding to `"disable"` to disable it.

```toml
[settings]
default_keys = true

[keys]
# Use ⎇- for horizontal split instead of ⎇=
"Alt + Minus" = { split = "vertical" }
"Alt + Equal" = "disable"
```

### Features

* **config:** Add settings.default_keys to include default key bindings ([945cd42](https://github.com/glide-wm/glide/commit/945cd42d241a6071e8055945a9216377b49c6177))
* **config:** Disable default key bindings by setting to "disable" ([a454cbc](https://github.com/glide-wm/glide/commit/a454cbc0e4ed9434002342ef4e6b931bd2e56ec5))

### Bug Fixes

* **config:** Fix server crash on `glide config update` ([6e1ca5c](https://github.com/glide-wm/glide/commit/6e1ca5c9c0fd1e175d947e21daceb2977f502669))

### Improvements

* **config:** Change preferred config path to ~/.config/glide/glide.toml ([#108](https://github.com/glide-wm/glide/issues/108)) ([4c8e94e](https://github.com/glide-wm/glide/commit/4c8e94eedb27ccbe06ecc58467afea2dd9f9b13e))


## [0.2.8](https://github.com/glide-wm/glide/compare/v0.2.7...v0.2.8) (2026-01-26)


### Features

* **spaces:** Integrate with mission control ([23619b3](https://github.com/glide-wm/glide/commit/23619b312398e867c56e22a50ffb319d4c322270)). Glide now updates its layouts correctly after using Mission Control to rearrange windows between spaces.


### Bug Fixes

* Don't cover floating windows with group bars ([3b90450](https://github.com/glide-wm/glide/commit/3b90450855f75466f269d222f05cea6f566e355a))
* **restore:** Remove terminated app windows from layout ([#115](https://github.com/glide-wm/glide/issues/115)) ([4334945](https://github.com/glide-wm/glide/commit/43349453e5916559a9278f15f23d2c25a375a7b8))


### Improvements

* **status:** Add Glide version number to status menu ([756cd0e](https://github.com/glide-wm/glide/commit/756cd0e0d6b6efce917764a13be3ed97887b0f6b))

## [0.2.7](https://github.com/glide-wm/glide/compare/v0.2.6...v0.2.7) (2026-01-24)

This version introduces a status icon to control Glide and see if it is running.

### Features

* Enable status icon by default ([0e7eb7d](https://github.com/glide-wm/glide/commit/0e7eb7db4d3261d1c9107915a144d02fd47bf3ab))
* **layout:** add resize window command ([#102](https://github.com/glide-wm/glide/issues/102)) ([e7f0492](https://github.com/glide-wm/glide/commit/e7f04927b94518e54301236fd74cd93fce47e4f6))
* **statusbar:** Add enable/disable and docs items  ([#96](https://github.com/glide-wm/glide/issues/96)) ([6823bda](https://github.com/glide-wm/glide/commit/6823bda9576c8f2c7fc4dcdb4d2772c002f9352d))


### Bug Fixes

* **group_bars:** Hide bars when Glide stops managing the space ([#111](https://github.com/glide-wm/glide/issues/111)) ([3fe9c32](https://github.com/glide-wm/glide/commit/3fe9c32b2929c17c93cfb796b6680d88b49834ef))
* **mouse:** Disable mouse_hides_on_focus if mouse_follows_focus is disabled ([7150ba5](https://github.com/glide-wm/glide/commit/7150ba5855436cf096b24148effaa59214fca34d))
* **mouse:** Hopefully fix a bug where the mouse cursor became invisible ([b21c5a0](https://github.com/glide-wm/glide/commit/b21c5a0ef4439b774a5add9714d6839d63ffd1dc))

## [0.2.6](https://github.com/glide-wm/glide/compare/v0.2.5...v0.2.6) (2026-01-16)


### Bug Fixes

* **cli:** Don't require server for `glide config verify` ([96c80cc](https://github.com/glide-wm/glide/commit/96c80cc6df8b723d65c5813a5823a715d1ac03de))
* **cli:** Report key parsing errors without panicking ([0f7c809](https://github.com/glide-wm/glide/commit/0f7c809a6dbf3b17ca1bfb9f1a8439752e457b51))
* Don't panic on show_timing command ([999dcdf](https://github.com/glide-wm/glide/commit/999dcdf38fd7aa6044b368504be0f925a5b7d0ce))

### Improvements

* **config:** Remove useless developer commands from default config ([dcb9053](https://github.com/glide-wm/glide/commit/dcb90533f3723c5949f71b601118a30739b1ca19))

### Experimental Features

* **status_icon:** Disable color by default ([266f74e](https://github.com/glide-wm/glide/commit/266f74e953ec35c35250b5ca2fecfab29cdea367))
* **status_icon:** space_index config now enables the space number ([3c01d93](https://github.com/glide-wm/glide/commit/3c01d934ffa0d939b59738d7dd34e4754e4d9e96))
* **status_icon:** Add menubar menu with initial `quit` ([#88](https://github.com/glide-wm/glide/issues/88)) ([d6b1874](https://github.com/glide-wm/glide/commit/d6b1874a5a56d2a1bd66204d79f3af1143cd688b))

## [0.2.5](https://github.com/glide-wm/glide/compare/v0.2.4...v0.2.5) (2026-01-13)


### Features

* **cli:** Add --restore option to `glide launch` ([b5cb685](https://github.com/glide-wm/glide/commit/b5cb685aea0d0a95ad074e74055a99a8eb8807c8))
* **cli:** Add service install/uninstall commands ([1663481](https://github.com/glide-wm/glide/commit/16634819dc5d50a4a5a84df36423f5c19c3f46be))
* **cli:** Add `--config` flag for custom config path support ([#79](https://github.com/glide-wm/glide/issues/79)) ([24f132b](https://github.com/glide-wm/glide/commit/24f132b22ad58cb964054e248f987c220532a7c3))

### Bug Fixes

* **layout:** Ensure fullscreen windows respect `outer_gap` configuration ([#80](https://github.com/glide-wm/glide/issues/80)) ([54af546](https://github.com/glide-wm/glide/commit/54af546128a1bd6819618bc90a07b5ec6665b856))

### Improvements

* **cli:** Add help text for config subcommands ([bb3a1a4](https://github.com/glide-wm/glide/commit/bb3a1a4edb4525821608b19ec9be60a14f4b99da))

### Developer Tools

* Add `app run` devtool command to run the app actor ([f6d4f14](https://github.com/glide-wm/glide/commit/f6d4f1453846ce6802e7693154b5edc148c5cc62))
* Make exec_cmd warning more precise ([e57a86b](https://github.com/glide-wm/glide/commit/e57a86b734ec13facc7c40addf71bbedb78e26c1))

### New Contributors

* @y3owk1n made their first contribution in https://github.com/glide-wm/glide/pull/79


## [0.2.4](https://github.com/glide-wm/glide/compare/v0.2.3...v0.2.4) (2026-01-12)


### Bug Fixes

* **cli:** Ensure `glide launch` works behind a symlink ([7cd9d3e](https://github.com/glide-wm/glide/commit/7cd9d3e1b325c6335c129649e7c72b6e0287ff7b))


### Improvements

* **cli:** Add --version flag ([f56a946](https://github.com/glide-wm/glide/commit/f56a946ba8a88eb79655ff0aca7525faa129a456))

## 0.2.3 (2026-01-12)

### What's Changed
* feat(cli): Add `glide launch` command by @tmandry in https://github.com/glide-wm/glide/pull/69


**Full Changelog**: https://github.com/glide-wm/glide/compare/v0.2.2...v0.2.3

## 0.2.2 (2026-01-10)

### What's Changed
* layout: Add configurable inner/outer gaps around windows by @intergrav in https://github.com/glide-wm/glide/pull/63

### New Contributors
* @intergrav made their first contribution in https://github.com/glide-wm/glide/pull/63

**Full Changelog**: https://github.com/glide-wm/glide/compare/v0.2.1...v0.2.2

## [0.2.1](https://github.com/glide-wm/glide/compare/v0.2.0...v0.2.1) (2026-01-09)


### Bug Fixes

* **app:** Display error instead of panicking on invalid config ([1ef2a23](https://github.com/glide-wm/glide/commit/1ef2a23fe1964c10b185163a6f83f572d1cd31f5))
* **cli:** Don't panic when config file is missing ([9fbbf21](https://github.com/glide-wm/glide/commit/9fbbf2170281caf1f355f19512ddeaf6932ad82d))

## [0.2.0](https://github.com/glide-wm/glide/compare/v0.1.1...v0.2.0) (2026-01-08)


### ⚠ BREAKING CHANGES

* Disable exec_cmd unless a cargo feature is enabled
* group_indicators config was renamed to group_bars

### Features

* Introduce glide cli ([5980f91](https://github.com/glide-wm/glide/commit/5980f9196157bb601b444c111d38154baef5a5e9))
* **client:** Add "config update" command ([1e6176f](https://github.com/glide-wm/glide/commit/1e6176f528327dc90ddbe616929b406c7b13c42d))
* **client:** Add "config update --watch" ([82bbaf1](https://github.com/glide-wm/glide/commit/82bbaf196b173f9df8e51248ff6c58f304b336c2))
* **client:** Add "config verify" ([864c3cd](https://github.com/glide-wm/glide/commit/864c3cdc5d30f3cf4e09541e044a43252be724ed))


### Bug Fixes

* Disable exec_cmd unless a cargo feature is enabled ([5a22894](https://github.com/glide-wm/glide/commit/5a228943f14276427994a832482ff9dc0a499117))

## [0.1.1](https://github.com/glide-wm/glide/compare/v0.1.0...v0.1.1) (2026-01-05)

### Bug Fixes

* **config:** Some config values were being ignored ([18abdbd](https://github.com/glide-wm/glide/commit/18abdbd7a4a71257cc7c27ac2f30bf3b61686ee6))
* **reactor:** Don't panic on windows unknown to accessibility ([f030120](https://github.com/glide-wm/glide/commit/f0301207d03fe68a5140d8da48d4f70df868d69d))

## 0.1.0 (2026-01-04)

This is the first official release of Glide, a tiling window manager for macOS.

Glide is inspired by Sway and i3 on Linux. Features include:

* Spaces support
* Keyboard-based navigation
* Tiled and untiled windows
* Tabbed and stacked groups with navigation bars
* Enable/disable on individual spaces
* Text based config
* Animations

For a better idea of what Glide can do, have a look at the [default config].

[default config]: https://github.com/glide-wm/glide/blob/3cc588bdd22cf65dc33c4e5a3afe4e6b840c41f9/glide.default.toml
