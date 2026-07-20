# tuigreety

A minimal, configurable console greeter for [greetd](https://git.sr.ht/~kennylevinsen/greetd). The project and package are named `tuigreety`; the installed executable remains `tuigreet` for compatibility with existing greetd configurations.

![Screenshot of tuigreet](contrib/screenshot.png)

```
Usage: tuigreet [OPTIONS]

Options:
    -h, --help          show this usage information
    -v, --version       print version information
        --config FILE   load an explicit TOML configuration file
        --check-config  show active configuration files, validate them, and
                        exit
        --ipc-timeout SECONDS
                        maximum seconds to wait for a greetd response (default:
                        120)
    -q, --quiet         discard output from the launched session
        --no-quiet      keep launched session output, overriding configuration
        --numlock       enable Num Lock before showing the login prompt
        --no-numlock    preserve the current Num Lock state
    -d, --debug [FILE]  enable debug logging to the provided file, or to
                        /tmp/tuigreet.log
        --no-debug      disable debug logging, overriding configuration
    -c, --cmd COMMAND   command to run
        --allow-command-editor
                        allow unauthenticated users to replace the session
                        command (unsafe)
        --no-command-editor
                        disable the command editor, overriding configuration
        --env KEY=VALUE environment variables to run the default session with
                        (can appear more than once)
    -s, --sessions DIRS colon-separated list of Wayland session paths
        --session-wrapper 'CMD [ARGS]...'
                        wrapper command to initialize the non-X11 session
    -x, --xsessions DIRS
                        colon-separated list of X11 session paths
        --xsession-wrapper 'CMD [ARGS]...'
                        wrapper command to initialize X server and launch X11
                        sessions (default: startx /usr/bin/env)
        --no-xsession-wrapper
                        do not wrap commands for X11 sessions
    -w, --width WIDTH   width of the main prompt (default: 80)
        --title TITLE   set a custom login prompt title
        --default-title use the hostname-based login prompt title
        --no-title      hide the login prompt title
    -i, --issue         show the host's issue file
        --no-issue      do not show the host's issue file
    -g, --greeting GREETING
                        show custom text above login prompt
    -t, --time          display the current date and time
        --no-time       do not display the current date and time
        --time-position POSITION
                        place the time at top, bottom, or hide it
        --time-format FORMAT
                        custom strftime format for displaying date and time
    -b, --battery       display the sampled battery percentage
        --no-battery    hide the battery percentage
        --battery-position SIDE
                        place battery status on the left or right
        --refresh-rate FPS
                        screen refresh rate in frames per second (default: 2,
                        maximum: 240)
    -r, --remember      remember last logged-in username
        --no-remember   do not remember the last logged-in username
        --remember-session
                        remember last selected session
        --no-remember-session
                        do not remember the last selected session
        --remember-user-session
                        remember last selected session for each user
        --no-remember-user-session
                        do not remember the last selected session for each user
    -u, --user USER     pre-fill an administrator-selected username
        --no-user       clear a configured default username
        --user-menu     allow graphical selection of users from a menu
        --no-user-menu  disable graphical user selection
        --user-autocomplete
                        allow Tab completion of usernames
        --no-user-autocomplete
                        disable Tab completion of usernames
        --user-menu-min-uid UID
                        minimum UID exposed by user menu or completion
        --user-menu-max-uid UID
                        maximum UID exposed by user menu or completion
        --theme THEME   define the application theme colors
        --asterisks     display asterisks when a secret is typed
        --no-asterisks  hide typed secrets completely
        --asterisks-char CHARS
                        characters to be used to redact secrets (default: *)
        --window-padding PADDING
                        padding inside the terminal area (default: 0)
        --container-padding PADDING
                        padding inside the main prompt container (default: 1)
        --prompt-padding PADDING
                        padding between prompt rows (default: 1)
        --greet-align [left|center|right]
                        alignment of the greeting text in the main prompt
                        container (default: 'center')
        --status-position POSITION
                        place the status bar at top, bottom, or hide it
        --status-reset / --no-status-reset
        --status-command / --no-status-command
        --status-sessions / --no-status-sessions
        --status-power / --no-status-power
        --status-selection / --no-status-selection
        --status-caps-lock / --no-status-caps-lock
        --status-config / --no-status-config
                        show or hide individual responsive status items
        --power-shutdown 'CMD [ARGS]...'
                        command to run to shut down the system
        --power-reboot 'CMD [ARGS]...'
                        command to run to reboot the system
        --power-suspend 'CMD [ARGS]...'
                        command to run to suspend the system
        --power-hibernate 'CMD [ARGS]...'
                        command to run to hibernate the system
        --power-setsid  start power commands in a new session, overriding
                        configuration
        --power-no-setsid
                        do not start power commands in a new session
        --mock          run without greetd and simulate authentication for
                        visual testing
        --no-mock       require greetd, overriding mock configuration
        --kb-command [1-12]
                        F-key to use to open the command menu
        --kb-sessions [1-12]
                        F-key to use to open the sessions menu
        --kb-power [1-12]
                        F-key to use to open the power menu
```

## Usage

The default configuration tends to be as minimal as possible, visually speaking, only showing the authentication prompts and some minor information in the status bar. You may print your system's `/etc/issue` at the top of the prompt with `--issue` and the current date and time with `--time` (and possibly customize it with `--time-format`). You may include a custom one-line greeting message instead of `/etc/issue` with `--greeting`.

Issue expansion is single-pass and supports the local agetty-compatible escapes `\\`, `\d`, `\t`, `\u`, `\U`, `\l`, `\s`, `\r`, `\v`, `\n`, `\m`, `\o`, `\S`, `\S{VARIABLE}`, `\e`, and `\e{name}`. `\S` reads `PRETTY_NAME` from `/etc/os-release` (falling back to `/usr/lib/os-release` and then `\s`), while the braced form reads the named field. Legacy `\033` and `\x1b` color escapes remain accepted. Unsupported escapes are displayed literally. A final blank line already supplied by the issue file is used as the prompt separator rather than doubled.

The initial prompt container will be 80 column wide. You may change this with `--width` in case you need more space (for example, to account for large PAM challenge messages). Please refer to usage information (`--help`) for more customization options. Various padding settings are available through the `*-padding` options.

Debug logging appends only to a private (`0600`) regular file. Symbolic links,
files with additional hard links, and non-regular files are refused. If the
configured log cannot be opened safely, `tuigreet` prints a warning and
continues without file logging. The log path is not accessed when debugging is
disabled.

You can instruct `tuigreet` to remember the last username that successfully opened a session with the `--remember` option (that way, the username field will be pre-filled). Similarly, the selected session can be retained between runs with `--remember-session`, globally or per user with `--remember-user-session`. Remembered values are committed as one versioned, atomic state file only after greetd confirms that the selected session started; authentication failures, cancellations, and `--mock` sessions do not change the system cache. Existing username and desktop-session caches are migrated on first use; legacy free-form command entries are deliberately discarded because their successful-session provenance cannot be verified. Check the [cache instructions](#cache-instructions) if `/var/cache/tuigreet` doesn't exist after installing tuigreet.

By default, the session command can only come from administrator configuration or an installed session file. You can list those sessions with `F3`; power options are available through `F12`.

The administrator can restore the legacy `F2` command editor with `--allow-command-editor` or `session.allow-command-editor = true`. This is intentionally disabled by default: anyone who can reach the greeter can use it to choose arbitrary code that will run as the next user who successfully authenticates, without another confirmation after authentication. Enable it only when every person with physical access is trusted. Free-form command caches are ignored while the editor is disabled; the global cache and per-user entries encountered during login are removed.

Text editing follows Unicode grapheme boundaries, and long values scroll horizontally to keep the cursor visible. To bound work and memory before authentication, interactively typed usernames are limited to 256 UTF-8 bytes, PAM responses to 4096 bytes, and free-form commands to 16384 bytes. Rejected input produces an on-screen warning; configured or discovered values are not truncated.

## Install

### From source

Building from source requires Rust 1.88 or newer from the stable channel, including `cargo`.

```
$ git clone https://github.com/Tobiichi-Origuchi/tuigreety && cd tuigreety
$ cargo build --release
# mv target/release/tuigreet /usr/local/bin/tuigreet
```

<a id="cache-instructions"></a>
The cache directory must be private and owned by the user running the greeter. Tuigreet refuses group- or world-writable cache paths and restricts safely owned paths and files to `0700` and `0600`, respectively. Distribution packages create it through tmpfiles; source installations can use:

```
# mkdir /var/cache/tuigreet
# chown greeter:greeter /var/cache/tuigreet
# chmod 0700 /var/cache/tuigreet
```

### From Arch Linux

The original project remains available from Arch Linux's official repositories as `greetd-tuigreet`. Tuigreety is published to the AUR in three variants:

- `greetd-tuigreety` builds the latest tagged release locally.
- `greetd-tuigreety-bin` installs the prebuilt release binary.
- `greetd-tuigreety-git` builds the latest `master` revision.

All three install `/usr/bin/tuigreet` and `/etc/tuigreet/config.toml`, provide `greetd-greeter`, and conflict with `greetd-tuigreet` and each other. Package upgrades preserve administrator changes to the configuration file.

The `-git` package intentionally keeps stable AUR metadata while `pkgver()` resolves the current upstream commit at build time. Enable development-package checks in your AUR helper so upstream-only commits are discovered during upgrades. For example, paru users can set `Devel` in `paru.conf` or run `paru --devel -Syu`; `paru --gendb` initializes tracking when migrating an existing VCS package from another helper.

### Pre-built binaries

Pre-built packages for x86_64, AArch64, i686, and ARMv7 can be found in the [releases](https://github.com/Tobiichi-Origuchi/tuigreety/releases) section of this repository. Each archive is a staged root filesystem containing `usr/bin/tuigreet`, the man page, project documentation, license and copyright notices, tmpfiles configuration, and `etc/tuigreet/config.toml`. It is not an installer and should not be unpacked blindly over `/`, because doing so could overwrite an existing login-manager configuration.

Verify the adjacent SHA-256 file and the GitHub/Sigstore provenance attestation before installation:

```sh
sha256sum --check tuigreety-VERSION-ARCH.tar.gz.sha256
gh attestation verify tuigreety-VERSION-ARCH.tar.gz \
  --repo Tobiichi-Origuchi/tuigreety
```

The attestation identifies the repository workflow and commit that produced the archive; release builds also compile every architecture twice in independent target directories and reject non-identical binaries. The included `usr/share/doc/tuigreety/INSTALL.md` preserves an existing `/etc/tuigreet/config.toml`; the same instructions are available in [`contrib/release/INSTALL.md`](contrib/release/INSTALL.md). The [tip prerelease](https://github.com/Tobiichi-Origuchi/tuigreety/releases/tag/tip) is continuously built and kept in sync with the `master` branch.

## Running the tests

The complete test suite runs without host-specific setup by running `cargo test`.

All builds, lints, and tests use the stable toolchain declared in `rust-toolchain.toml`. Code formatting alone uses the exact dated nightly declared in `RUSTFMT_VERSION`, because `.rustfmt.toml` enables unstable formatting rules:

```sh
RUSTFMT_TOOLCHAIN=$(cat RUSTFMT_VERSION)
rustup toolchain install "$RUSTFMT_TOOLCHAIN" --profile minimal --component rustfmt
cargo +"$RUSTFMT_TOOLCHAIN" fmt --all --check
taplo fmt --check Cargo.toml .taplo.toml Cross.toml rust-toolchain.toml
```

The formatter pin is updated deliberately rather than following the moving `nightly` channel. When updating it, choose the newest dated nightly whose rustfmt component is [available](https://rust-lang.github.io/rustup/concepts/components.html#component-availability), format the whole tree, review any formatting changes, and commit the new pin together with those changes.

## Configuration

tuigreet reads TOML configuration from these layers, with later layers overriding earlier ones:

1. `/etc/tuigreet/config.toml`
2. The file selected by `--config FILE`
3. Individual command-line options

All fields are optional. Unknown fields, invalid values, unreadable files, and malformed command-line options produce warnings on standard error and are ignored; valid fields still take effect. A file with invalid TOML syntax is ignored as a whole. This makes a configuration mistake non-fatal, while preserving the previous valid layer or built-in default.

TOML does not permit duplicate keys or duplicate table declarations. Such duplicates are syntax errors, so tuigreet rejects that file instead of choosing one value arbitrarily. Run `tuigreet --check-config`, optionally together with `--config FILE`, to print every active path and validate syntax, field names, value types, ranges, and relationships. The check command exits unsuccessfully for any error or warning; ordinary greeter startup remains non-fatal.

Configuration can select commands that run as the authenticated user or control system power. Each active file is therefore expected to be owned by root and not writable by its group or other users (normally `root:root 0644`). Unsafe ownership or mode produces a non-fatal startup/reload warning and makes `--check-config` fail; tuigreet never changes the file automatically.

The system configuration and an explicit configuration file, when selected, are monitored for changes. Valid updates are applied automatically, while command-line options retain the highest priority. An unreadable or malformed update is rejected and the last valid runtime configuration remains active. Changes to `general.debug`, `general.log-file`, `general.numlock`, and `general.mock` require a restart.

See [`contrib/tuigreet.toml`](contrib/tuigreet.toml) for every supported field and its default. Arrays are used for session directories and environment entries, for example:

```toml
[general]
ipc-timeout = 120

[session]
command = "sway"
environment = ["XDG_CURRENT_DESKTOP=sway"]
sessions = ["/usr/share/wayland-sessions"]

[display]
time = true
refresh-rate = 30

[users]
autocomplete = true
```

Commands and environment values are stored as plain text; do not put credentials in them and protect the files appropriately.

### greetd configuration

Edit `/etc/greetd/config.toml` and set the `command` setting to use `tuigreet`:

```
[terminal]
vt = 1

[default_session]
command = "tuigreet --cmd sway"
user = "greeter"
```

Please refer to [greetd's wiki](https://man.sr.ht/~kennylevinsen/greetd/) for more information on setting up `greetd`.

### Sessions

Sessions are loaded from `.desktop` files below the XDG data directories (`wayland-sessions` and `xsessions`); the usual defaults include `/usr/local/share` and `/usr/share`. Use `--sessions` or `--xsessions` with colon-separated directories to replace the defaults for only that session type.

#### Desktop environments

Tuigreet accepts regular Desktop Entries with `[Desktop Entry]`, `Type=Application`, `Name`, and `Exec`. It also honors `Hidden`, `NoDisplay`, `TryExec`, `DesktopNames`, and standard `Exec` quoting/field-code rules. Invalid or shadowed entries are ignored deterministically.

For example, a custom Wayland session can be placed in a directory configured through `--sessions`:

```ini
[Desktop Entry]
Type=Application
Name=Wayland Gnome
TryExec=/usr/bin/dbus-run-session
Exec=/usr/bin/env "XDG_SESSION_TYPE=wayland" dbus-run-session gnome-session
DesktopNames=GNOME
```

greetd supports both a command and environment entries in `StartSession`. `--env KEY=VALUE` supplies environment only for the configured default command; selected desktop sessions instead receive `XDG_SESSION_TYPE` and, when available, `XDG_CURRENT_DESKTOP` inferred from their entry. Additional environment can be expressed explicitly in `Exec` as above or supplied by a trusted wrapper.

#### Common wrappers

`--session-wrapper 'CMD [ARGS]...'` prepends a command to non-X11 sessions, while `--xsession-wrapper 'CMD [ARGS]...'` does the same for X11 sessions. For example, `--session-wrapper dbus-run-session` starts the selected session under `dbus-run-session`.

The wrapper is intentionally generic and can contain several arguments. For example, `wrapper = "uwsm start --"` or `--session-wrapper 'uwsm start --'` passes the selected compositor command through UWSM. For full UWSM metadata, the [UWSM documentation](https://github.com/Vladimir-csp/uwsm#from-a-display-manager) recommends a dedicated Wayland Desktop Entry whose `Exec` references another entry, such as `Exec=uwsm start -- my-compositor.desktop`; this avoids duplicating UWSM's evolving session policy inside tuigreet.

X11 sessions use `startx /usr/bin/env` by default. Set `xsession-wrapper = false` in TOML or pass `--no-xsession-wrapper` to disable it.

### Power management

Four power actions are possible from `tuigreet`: shutting down, rebooting, suspending and hibernating the machine. Shutdown and reboot use `shutdown -h now` and `shutdown -r now`. Suspend and hibernate use `systemctl` when systemd is running and `loginctl` when elogind is running. Actions without an automatically detected or explicitly configured command are omitted from the menu.

The commands can be customized with `--power-shutdown`, `--power-reboot`, `--power-suspend` and `--power-hibernate`. Each option takes one shell-quoted string which is parsed into a program and literal arguments, then executed directly without a shell. Shell expansion, pipelines, redirection, and environment assignments are therefore not interpreted. The provided commands must be non-interactive, meaning they will not be able to print anything or prompt for anything. If you need to use `sudo` or `doas`, they will need to be configured to run passwordless for those specific commands.

An example for `/etc/greetd/config.toml`:

```
[default_session]
command = "tuigreet --power-shutdown 'sudo systemctl poweroff'"
```

In `/etc/tuigreet/config.toml`, the preferred format is an exact argv array. The first item is the program and each remaining item is one argument, so whitespace and other special characters need no extra escaping for a shell:

```toml
[power]
shutdown = ["sudo", "systemctl", "poweroff"]
suspend = false
```

Omitting a power field selects the automatically detected default. Setting it to `false` disables and hides that action, including its default. Legacy command strings are still accepted for compatibility and use the same shell-quoting parser as the command-line options; they are never executed by a shell.

By default, each power command is detached from the TTY in a new session and process group using the `setsid(2)` system call; no external `setsid` binary is invoked. Use `--power-no-setsid` to disable this isolation.

The waiting screen is drawn before a command starts. Press Esc to cancel it. Commands time out after 30 seconds; cancellation and timeout send SIGTERM, then escalate to SIGKILL after 500 ms. With the default session isolation, signals cover the whole command process group.

### Visual mock mode

Use `tuigreet --mock` to inspect themes and layout changes without a running greetd service. Mock mode does not require `GREETD_SOCK`; it provides placeholder sessions and simulates the normal username, password, and successful-login flow locally. Selecting a power action exits mock mode without executing a real power command.

### User discovery

Optionally, a user can be selected from a menu instead of typing out their name, with the `--user-menu` option, this will present all users returned by NSS at the time `tuigreet` was run, with a UID within the acceptable range. The values for the minimum and maximum UIDs are selected as follows, for each value:

 * A user-provided value, through `--user-menu-min-uid` or `--user-menu-max-uid`;
 * **Or**, the available values for `UID_MIN` or `UID_MAX` from `/etc/login.defs`;
 * **Or**, hardcoded `1000` for minimum UID and `60000` for maximum UID.

`--user-autocomplete` uses the same filtered user set without displaying a list. On an empty field, Tab completes only when exactly one eligible user exists. After at least one character is entered, Tab completes a unique match or expands a shared prefix; if the prefix is already a complete username, Tab submits it as before. User discovery is disabled by default so administrators who treat local usernames as sensitive do not expose them accidentally.

### Theming

A theme specification can be given through the `--theme` argument to control some of the colors used to draw the UI. This specification string must have the following format: `component1=color;component2=color[;...]` where the component is one of the value listed in the table below, and the color is a valid ANSI color name as listed [here](https://github.com/ratatui-org/ratatui/blob/main/src/style/color.rs#L15).

In TOML, the same components can be configured individually. Theme fields merge across configuration layers, and `false` removes a color inherited from a lower-priority layer:

```toml
[theme]
border = "magenta"
text = "cyan"
prompt = "green"
time = false
```

Mind that the specification string include semicolons, which are command delimiters in most shells, hence, you should enclose it in single-quotes so it is considered a single argument instead.

Please note that we can only render colors as supported by the running terminal. In the case of the Linux virtual console, those colors might not look as good as one may think. Your mileage may vary.

| Component name | Description                                                                        |
| -------------- | ---------------------------------------------------------------------------------- |
| text           | Base text color other than those specified below                                   |
| time           | Color of the date and time. If unspecified, falls back to `text`                   |
| container      | Background color for the centered containers used throughout the app               |
| border         | Color of the borders of those containers                                           |
| title          | Color of the containers' titles. If unspecified, falls back to `border`            |
| greet          | Color of the issue of greeting message. If unspecified, falls back to `text`       |
| prompt         | Color of the prompt ("Username:", etc.)                                            |
| input          | Color of user input feedback                                                       |
| action         | Color of the actions displayed at the bottom of the screen                         |
| button         | Color of the keybindings for those actions. If unspecified, falls back to `action` |

Below is a screenshot of the greeter with the following theme applied: `border=magenta;text=cyan;prompt=green;time=red;action=blue;button=yellow;container=black;input=red`:

![Screenshot of tuigreet](contrib/screenshot-themed.png)

## License and copyright

Tuigreety is an independently maintained modified version of [tuigreet](https://github.com/apognu/tuigreet). Copyright in the original work remains with Antoine POPINEAU and other respective contributors; copyright in the Tuigreety modifications remains with Tobiichi-Origuchi and their respective contributors.

The complete project is free software under the [GNU General Public License, version 3 or later](LICENSE). The GPL text is kept unmodified; project-specific attribution and the required modification notice are recorded separately in [COPYRIGHT](COPYRIGHT), Git history, and the changelog.
