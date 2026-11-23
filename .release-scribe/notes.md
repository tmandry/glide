This release of Glide adds shortcuts for left- or right-side modifier keys and a warning when macOS Secure Input blocks shortcuts. It also improves layout resizing and fixes window management around Mission Control and screen locking.

### Features

* Bind shortcuts to left- or right-specific modifiers, such as `AltLeft + H`, to leave the other Option key available for typing accented characters. See the [configuration reference](https://glidewm.org/reference/config/) ([#313](https://github.com/tmandry/glide/pull/313)).
* Glide now shows a menu bar warning when macOS Secure Input blocks keyboard shortcuts; choose “Shortcuts Paused by Secure Input…” for troubleshooting guidance ([#314](https://github.com/tmandry/glide/pull/314)).

### Improvements

* Glide’s alerts now show its parachute icon even when running outside the app bundle, making them easier to identify ([#314](https://github.com/tmandry/glide/pull/314)).

### Bug Fixes

* Default shortcuts can now be disabled or overridden using equivalent spellings, such as `Option + H` instead of `Alt + H` ([#313](https://github.com/tmandry/glide/pull/313)).
* Avoid unexpected Space switches when leaving Mission Control after moving the focused window to another Space or opening a new window ([#309](https://github.com/tmandry/glide/pull/309)).
* Apps that launch or relaunch while the screen is locked no longer remain unmanaged after unlocking ([#312](https://github.com/tmandry/glide/pull/312)).
* Mouse-driven focus and scrolling now recover automatically if macOS disables mouse monitoring after a slow response ([#288](https://github.com/tmandry/glide/pull/288)).
* Edge resizing in the experimental scroll layout now respects application size limits and brings the focused column back into view when resizing ends ([#279](https://github.com/tmandry/glide/pull/279)).
* Closing the last window beside a tabbed or stacked group no longer leaves an unnecessary outer group ([#259](https://github.com/tmandry/glide/pull/259)).
