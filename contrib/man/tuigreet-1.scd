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
	Load an explicit TOML configuration file. It overrides the standard system
	and user files, while individual command-line options override it.

*-d, --debug [FILE]*
	Enables debug logging to the provided FILE path, or to /tmp/tuigreet.log if no
	file is specified.

*-c, --cmd CMD*
	Specify which command to run on successful authentication. This can be
	overridden by manual selection within *tuigreet*.

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

*--text-config*
	Load text overrides from */etc/tuigreet/text.conf*, then from
	*$XDG_CONFIG_HOME/tuigreet/text.conf* or *$HOME/.config/tuigreet/text.conf*.
	No text configuration is read unless this option is present.

*--text-config-file FILE*
	Load text overrides from FILE. If *--text-config* is also present, FILE is
	applied last. The file uses key=value entries.

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
	Allow Tab completion from the eligible local users. Unique matches are
	completed; ambiguous matches expand only their common prefix. Disabled by
	default.

*--user-menu-min-uid*
	Minimum UID exposed by the user menu or username completion.

*--user-menu-max-uid*
	Maximum UID exposed by the user menu or username completion.

*-r, --remember*
	Remember the username of the last successfully opened session, so the
	username field will be pre-filled on the next run.

*--remember-session*
	Remember the last selected session, effectively overriding the given *--cmd*
	option on subsequent runs.

*--remember-user-session*
	Remember the last opened session, per user (requires *--remember*).

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

*--power-no-setsid*
	Do not prefix power commands with *setsid*, which is used to detach it from
	current TTY.

*--mock*
	Run without connecting to greetd and simulate authentication locally for
	visual inspection. Power actions exit without changing system power state.

*--kb-[command|sessions|power] [1-12]*
	change the default F-key keybindings to access the command, sessions and power
	menus.

# CONFIGURATION

Configuration is loaded from */etc/tuigreet/config.toml*, then from
*$XDG_CONFIG_HOME/tuigreet/config.toml* or *$HOME/.config/tuigreet/config.toml*.
An explicit *--config FILE* is applied next, and individual command-line options
have the highest priority. The environment variables and home directory are
those of the account running tuigreet.

All fields are optional. Unknown or invalid fields are warned about and ignored
individually; an invalid TOML document is ignored as a whole. Configuration and
command-line mistakes do not prevent tuigreet from starting. See the example
*contrib/tuigreet.toml* distributed with the source for the complete schema.

# AUTHORS

Maintained by the tuigreet contributors.

Contributor history is available at
https://github.com/Tobiichi-Origuchi/tuigreety/graphs/contributors.

# DEVELOPMENT

Issue reporting and development discussion should happen at
https://github.com/Tobiichi-Origuchi/tuigreety.

# SEE ALSO

*greetd*(1)
