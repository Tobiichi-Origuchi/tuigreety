# Changelog

## 0.10.3 - 2026-07-18

### Added

- [`db6a095`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/db6a09577db5b3ce0671a57d884c212f7c170798) Add `--ipc-timeout` and `general.ipc-timeout` controls for greetd transactions, with a 120-second default.

### Changed

- [`d891a6f`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/d891a6fc4a980996b6879a76a1d75792feebba46) Follow the Desktop Entry specification when discovering sessions, including `Type`, `Hidden`, `TryExec`, desktop-ID priority, and deterministic ordering.
- [`1d7873a`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/1d7873af54dce0c6211cca4a46804695af6c4875) Parse session `Exec` values with specification-compliant quoting and field-code handling while preserving exact arguments through greetd's shell invocation.
- [`f81f9e9`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/f81f9e971ea2a1f12d20d2d94883a3e1f552caa9) Make text editing grapheme-aware and keep long input fields and menus usable through horizontal and vertical viewports.

### Fixed

- [`2843417`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/28434171530c5e5a740b9e9a037d41e0b56d3536) Make multi-character password masks cycle predictably and prevent an empty mask from causing a panic.
- [`172f39b`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/172f39b536a15be60b68bfa4c4eb0b1da83770f2) Ignore key-release events, handle repeated keys and compound modifiers correctly, and reveal an empty-prefix username completion only when exactly one eligible user exists.
- [`250d9ac`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/250d9ac4148439e6c25a83863ef18dfc7f31d0a6) Correct `--version` and packaged notices to identify Tuigreety, its derivative status, both copyright holders, and the GPLv3+ license.
- [`606f785`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/606f7852302c21a700e245ed9e71d1ceea1439e6) Start the selected session command consistently in both debug and release builds.
- [`db6a095`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/db6a09577db5b3ce0671a57d884c212f7c170798) Prevent greetd errors or stalled PAM exchanges from freezing the greeter; failures are surfaced, recoverable connections are retried, cancellation remains responsive, and state changes render immediately.
- [`ebf89eb`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/ebf89ebc0e4ac7dc8ab56490f4040fa129b2ac4f) Restore raw mode, the alternate screen, and cursor visibility after normal exits, errors, panics, power actions, `SIGINT`, and `SIGTERM`.
- [`f81f9e9`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/f81f9e971ea2a1f12d20d2d94883a3e1f552caa9) Prevent layout overflow and cursor errors on tiny terminals, extreme dimensions, and Unicode input; oversized interactive values now produce a visible warning instead of growing without bound.

### Security

- [`3a44314`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/3a443144d225bfb5f93557c46c6f6ccc8967a774) Isolate and zeroize command-editor input so opening or closing nested menus cannot replace or retain a PAM response.

## 0.10.2 - 2026-07-17

### Added

- [`46bf09c`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/46bf09c3d65541c0e507948a6791159b3fe1c0c3) Add CLI and TOML controls for explicitly enabling or disabling the legacy F2 command editor.

### Changed

- [`46bf09c`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/46bf09c3d65541c0e507948a6791159b3fe1c0c3) Disable the unauthenticated F2 arbitrary-command editor by default; session commands now come from administrator configuration or installed sessions unless explicitly enabled.

### Fixed

- [`99edd90`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/99edd900e1f690360076c1915687fcc50e0fb7eb) Exclude the executable name from option parsing, removing the spurious `unexpected positional argument 'tuigreet'` warning.
- [`46bf09c`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/46bf09c3d65541c0e507948a6791159b3fe1c0c3) Update remembered usernames and sessions only after greetd confirms the exact submitted session started; failed or cancelled attempts no longer change the cache.
- [`7786e54`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/7786e5435ae8c1cdf8ded69b3489414232ca87bd) Keep `--version` nonempty and current for release tags, development commits, dirty trees, shallow clones, and source archives.
- [`f4f3a06`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/f4f3a06276f318c5eaba94f94eca1608b05609ed) Preserve unrelated valid options when duplicate, unknown, malformed, positional, or non-UTF-8 command-line arguments are ignored.
- [`003d741`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/003d741b3c427a0a5777505f7c04eb109b50f4ea) Prevent authentication success and real or mock power actions from deadlocking behind a full render/event queue.

### Security

- [`c5d3ab8`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/c5d3ab806d0d74ee28ebe6717ba296e9ede1327b) Write debug logs only to private regular files, reject unsafe links and special files, and redact session commands and environment values.

## 0.10.1 - 2026-07-15

### Added

- [`5bf0e03`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/5bf0e034e4c9fa2728e11e8bd5557e0f686ba6ba) Add configuration hot reload with last-known-good fallback for invalid updates.
- [`af9d6b4`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/af9d6b4951656876eec027cb4e5b08236da3856a) Add `--check-config`, source-located diagnostics, and a fully documented system configuration template.

### Changed

- [`5bb3a73`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/5bb3a73833f7be3afe95aa6e47a18edaa5d922d3) Restrict display-manager configuration to system and explicitly selected files; CLI options remain highest priority.
- [`af9d6b4`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/af9d6b4951656876eec027cb4e5b08236da3856a) Reject duplicate TOML keys and preserve valid runtime settings when configuration parsing fails.

### Fixed

- [`aec2031`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/aec2031e37e8501d09a0c18a909839a16af8ed96) Prevent exit requests from deadlocking when the event queue is full.

### Removed

- [`811434c`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/811434c42f1576662f869d2ecb10083ff019d503) Remove external text overrides and the obsolete `text.conf` interface.

## 0.10.0 - 2026-07-14

### Added

- [`59906e4`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/59906e44eb04366f35e4fa5159319e877bedce0f) Add suspend and hibernate actions, selecting `systemctl` for systemd and `loginctl` for elogind systems (inspired by NotAShelf's fork commit [`993ad6f`](https://github.com/NotAShelf/tuigreet/commit/993ad6f7155d5f411ba8b185589889c8594ab377)).
- [`b646be4`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/b646be46a97d0d6aba816b46e8b4120328cf3270) Add `--mock` mode for previewing the interface and authentication flow without a running greetd instance.
- [`c5ee801`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/c5ee8017aceb5a3c3d7d082b51d00c525b6cf1b2) Add configurable refresh rates with `--refresh-rate`, while keeping cursor blinking on an independent, stable timer.
- [`262ca68`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/262ca686758ebb9eb27dff9720f89c2045d2c26) Add opt-in username completion with `--user-autocomplete`, including unique-prefix and single-user completion.
- [`ba6d6b7`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/ba6d6b7f4136aad5b9826098e6048e3a8556d9c0) Add layered TOML configuration with CLI, user, and system precedence; invalid fields produce warnings without preventing startup.
- [`50c6be3`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/50c6be31d62e0c3b82fd0d81e45bf408f12f6940) Add complete TOML theme configuration for every existing theme color field.

### Changed

- [`20332b5`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/20332b5a69db9cf3dd6b9c18bcd3679a09dac991) Ignore unknown command-line options with a warning so stale greetd configurations can still start the greeter.
- [`8a57206`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/8a572069137224c93416edd9b3aa0bdc3ecca1ce) Allow a user-menu UID range containing a single UID (adapted from NotAShelf's fork commit [`b629525`](https://github.com/NotAShelf/tuigreet/commit/b62952530614c4fe44dcec6dcec862abcf1a25e6)).
- [`8fd54e4`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/8fd54e4e7403a3eb63d5a66689b98353da72d9b) Support the full 32-bit UID range in user filtering (adapted from NotAShelf's fork commit [`9a812bd`](https://github.com/NotAShelf/tuigreet/commit/9a812bdf2e5139f6ebfae83998c8d30236f74c60)).
- [`b379c21`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/b379c21383dc84faed15bdc39b00ebdecb360b5c) Replace built-in i18n assets with opt-in text override files, allowing every displayed label to be customized without loading localization data by default.
- [`3d13831`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/3d13831407a36c15173138772900260c42a8bc4a) Complete initial authentication before the first visible frame when a user is already selected, avoiding the one-row-to-two-row prompt jump.
- [`9b0850d`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/9b0850dd45efd032a79575e9a9954b923765e7c7) Rename the project and package to `tuigreety` while retaining the `tuigreet` executable name for configuration compatibility.

### Fixed

- [`4bc8f2c`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/4bc8f2c00a23ebba9ba8ca8ded62146f4cd361fa) Use the real terminal dimensions for the first frame, preventing an initial layout rendered at the wrong size.
- [`15746f1`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/15746f164a645443d73273e1a6d47cdc5656a788) Clear the startup screen directly through crossterm, restoring immediate display after Plymouth or boot handoff.
- [`4226eba`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/4226eba268b1d45124ca3caa0852b6d3be1dbad7) Prevent concurrent IPC handling from racing over greetd state (adapted from NotAShelf's fork commit [`e0785c4`](https://github.com/NotAShelf/tuigreet/commit/e0785c4e9bc49ef0fbfd7fd28673bfe635aeb5a9)).
- [`6d9af2d`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/6d9af2d019f896f087414fdfda38129b75c23f95) Prevent empty session, user, command, or power menus from underflowing or panicking (adapted from [NotAShelf's fork PR #58](https://github.com/NotAShelf/tuigreet/pull/58)).
- [`80c84e0`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/80c84e0342e07fa882ce3220d206d4dc7864b373) Preserve intentional leading and trailing whitespace in greeting text.
- [`0c72ee0`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/0c72ee06dbb6e725ebf941024f437f948b18be18) Allow `--help`, `--version`, and other information-only options to run without `GREETD_SOCK` (fixes [upstream issue #178](https://github.com/apognu/tuigreet/issues/178)).
- [`25af04f`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/25af04f64c5c035c64820192c34ab570a21d1094) Deduplicate configured session search paths while preserving their order (adapted from NotAShelf's fork commit [`de73f43`](https://github.com/NotAShelf/tuigreet/commit/de73f43f624a0b62dc072f98e437bcb2b3052b27)).
- [`c5ee801`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/c5ee8017aceb5a3c3d7d082b51d00c525b6cf1b2) Prevent periodic redraws from resetting the visible cursor blink phase.

### Removed

- [`b379c21`](https://github.com/Tobiichi-Origuchi/tuigreety/commit/b379c21383dc84faed15bdc39b00ebdecb360b5c) Remove bundled translations and their runtime dependencies; custom text files now provide the localization interface.
