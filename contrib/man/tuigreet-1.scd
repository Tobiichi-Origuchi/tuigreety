tuigreet(1)

# NAME

tuigreet - A graphical console greeter for greetd

# SYNOPSIS

*tuigreet* [OPTIONS]...

# OPTIONS

*-h, --help*
	Show usage and exit.

*-v, --version*
	Print program version and exit.

*--config FILE*
	Load an explicit TOML configuration file after the system file. Individual
	command-line options override it.

*--check-config*
	Print every active configuration path, validate the effective configuration,
	and exit. Any syntax, field, type, range, or relationship warning produces a
	non-zero exit status. Use with *--config FILE* to check an explicit layer.

*--ipc-timeout SECONDS*
	Set the maximum time to wait for a greetd response. The value must be between
	1 and 3600 seconds; the default is 120.

*-d, --debug [FILE]*
	Enables debug logging to the provided FILE path, or to /tmp/tuigreet.log if no
	file is specified. Logs are appended with mode 0600. FILE must be a regular
	file without symbolic or additional hard links; otherwise a warning is
	printed and startup continues without file logging. The path is not accessed
	when debug logging is disabled.

*-c, --cmd CMD*
	Specify which command to run on successful authentication. This can be
	overridden by selecting an installed session within *tuigreet*.

*--allow-command-editor*
	Enable the F-key editor that lets an unauthenticated user replace the session
	with an arbitrary command. The command runs as the next user who successfully
	authenticates, without another confirmation after authentication. This is
	disabled by default and should only be enabled when all people with physical
	access are trusted.

*--no-command-editor*
	Disable the command editor, overriding a configuration file that enabled it.

*--env KEY=VALUE*
	Environment variables to run the default session with (can appear more then once).

*-s, --sessions DIR1[:DIR2]...*
	Location of desktop-files to be used as Wayland session definitions. By
	default, Wayland sessions are fetched from */usr/share/wayland-sessions*.

*--session-wrapper 'CMD [ARGS]...'*
	Specify a wrapper command to execute instead of the session for non-X11
	sessions. This command will receive the session command as its arguments.

*-x, --xsessions DIR1[:DIR2]...*
	Location of desktop-files to be used as X11 session definitions. By
	default, X11 sessions are fetched from */usr/share/xsessions*.

*--xsession-wrapper 'CMD [ARGS]...'*
	Specify a wrapper command to initialize X server and launch X11 sessions.
	By default, *startx /usr/bin/env* will be prepended to all X11 session
	commands.

*--no-xsession-wrapper*
	Do not wrap commands for X11 sessions.

*-w, --width COLS*
	Number of columns the main prompt area should take on the screen.

*-i, --issue*
	Print the content of */etc/issue* at the top of the prompt area.

	This option is mutually exclusive with *--greeting*.

*-g, --greeting GREETING*
	Specify the text to be displayed at the top of the prompt area.

	This option is mutually exclusive with *--issue*.

*-t, --time*
	Print the current date and time at the top of the screen.

*--time-format FORMAT*
	Configure a custom strftime-compliant format string for the current date
	and time.

*--refresh-rate FPS*
	Set the periodic screen refresh rate. The value must be between 1 and 240;
	the default is 2. Cursor blinking uses an independent 500 ms timer.

*--user-menu*
	Allow selecting a user from a graphical menu.

*--user-autocomplete*
	Allow Tab completion from the eligible local users. An empty field completes
	only when exactly one eligible user exists. Otherwise, after at least one
	character, unique matches are completed and ambiguous matches expand only
	their common prefix. Disabled by default.

*--user-menu-min-uid*
	Minimum UID exposed by the user menu or username completion.

*--user-menu-max-uid*
	Maximum UID exposed by the user menu or username completion.

*-r, --remember*
	Remember the username of the last successfully opened session, so the
	username field will be pre-filled on the next run.

*--remember-session*
	Remember the last session acknowledged as started by greetd. Authentication
	failures and cancellations do not update the cache.

*--remember-user-session*
	Remember the last session acknowledged as started by greetd, per user
	(requires *--remember*).

	Remembered values are stored atomically in a versioned state file under
	*/var/cache/tuigreet*. The directory must be owned by the greeter user and
	must not be group- or world-writable. Mock sessions never update this cache.
	Legacy usernames and desktop sessions are migrated; legacy free-form commands
	are discarded because their successful-session provenance cannot be verified.

*--theme SPEC*
	Define colors to be used to draw the UI components. You can find the proper
	syntax in the project's README.

*--asterisks*
	Add visual feedback when typing secrets, as one asterisk character for every
	keystroke. By default, no feedback is given at all.

*--asterisks-char CHARS*
	Change the default feedback character from an asterisk to a random
	distribution of the provided characters.

*--window-padding COLS*
	Add spacing between the edge of the screen area the drawing area.

*--container-padding COLS*
	Add spacing between the border of the main prompt area and its contents.

*--prompt-padding ROWS*
	Add spacing between form fields.

*--greet-align [left|center|right]*
	Alignment of the greeting text in the main prompt container
	(default: 'center').

*--power-shutdown CMD [ARGS]...*
	Customize the command run when instructed to shut down the machine. This must
	be a non-interactive command (sudo cannot prompt for a password, for example).

*--power-reboot CMD [ARGS]...*
	Customize the command run when instructed to reboot the machine. This must be
	a non-interactive command (sudo cannot prompt for a password, for example).

*--power-suspend CMD [ARGS]...*
	Customize the command run when instructed to suspend the machine. By default,
	this uses *systemctl* with systemd and *loginctl* with elogind.

*--power-hibernate CMD [ARGS]...*
	Customize the command run when instructed to hibernate the machine. By default,
	this uses *systemctl* with systemd and *loginctl* with elogind.

	Each power option takes one shell-quoted string. It is parsed into a program
	and literal arguments, then executed directly without a shell. Shell expansion,
	pipelines, redirection, and environment assignments are not interpreted.

*--power-no-setsid*
	Do not start power commands in a new session and process group. By default,
	tuigreet calls *setsid(2)* internally to detach them from the current TTY; it
	does not invoke an external *setsid* binary.

	The waiting screen is drawn before a power command starts. Press Escape to
	cancel it. Commands time out after 30 seconds. Cancellation and timeout send
	SIGTERM, then SIGKILL after 500 ms; with session isolation this covers the
	whole process group.

*--mock*
	Run without connecting to greetd and simulate authentication locally for
	visual inspection. Power actions exit without changing system power state.

*--kb-[command|sessions|power] [1-12]*
	change the default F-key keybindings to access the command, sessions and power
	menus. The command binding is inactive unless *--allow-command-editor* is set.

# CONFIGURATION

Configuration is loaded from */etc/tuigreet/config.toml*. An explicit
*--config FILE* is applied next, and individual command-line options have the
highest priority.

All fields are optional. Unknown or invalid fields are warned about and ignored
individually; an invalid TOML document is ignored as a whole. Configuration and
command-line mistakes do not prevent tuigreet from starting. See the example
*contrib/tuigreet.toml* distributed with the source for the complete schema.

Power commands use exact argv arrays in TOML, with the program first and one
item per argument. An omitted field uses the automatically detected default;
*false* disables and hides the action. Actions without an automatically detected
or explicitly configured command are also hidden. Legacy command strings use the
same shell-quoting parser as the command-line options, but are never run through
a shell.

Duplicate keys and duplicate table declarations are invalid TOML. A file with
duplicates is rejected as a whole rather than applying an arbitrary occurrence.
Use *--check-config* for source-context diagnostics without starting the greeter.

Configuration can select commands run as the authenticated user or control
system power. Active files are expected to be root-owned and not writable by
their group or other users (normally *root:root 0644*). Unsafe ownership or mode
is warned about without preventing ordinary startup; it makes *--check-config*
exit unsuccessfully. Tuigreet never changes configuration permissions itself.

The system and explicit configuration files are monitored and valid changes are
applied automatically. Command-line options retain the highest priority. An
unreadable or malformed update is rejected, preserving the last valid runtime
configuration. Changes to *general.debug*, *general.log-file*, and *general.mock*
require a restart.

# INPUT EDITING

Cursor movement and deletion follow Unicode grapheme boundaries. Long values
scroll horizontally to keep the cursor visible. To bound work and memory before
authentication, interactively typed usernames are limited to 256 UTF-8 bytes,
PAM responses to 4096 bytes, and free-form commands to 16384 bytes. Rejected
input produces an on-screen warning; configured or discovered values are not
truncated.

# AUTHORS

Maintained by the tuigreet contributors.

Contributor history is available at
https://github.com/Tobiichi-Origuchi/tuigreety/graphs/contributors.

# DEVELOPMENT

Issue reporting and development discussion should happen at
https://github.com/Tobiichi-Origuchi/tuigreety.

# SEE ALSO

*greetd*(1)
