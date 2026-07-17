use std::{
  env,
  error::Error,
  ffi::{OsStr, OsString},
  fmt::{self, Display},
  path::PathBuf,
  process,
  sync::Arc,
};

use getopts::{Matches, Options};
use tokio::{
  net::UnixStream,
  sync::{RwLock, RwLockWriteGuard},
};
use tracing_appender::non_blocking::WorkerGuard;
use zeroize::Zeroize;

use crate::{
  config::{self, Settings},
  event::DEFAULT_REFRESH_RATE,
  info::{
    get_issue,
    get_last_command,
    get_last_session_path,
    get_last_user_command,
    get_last_user_name,
    get_last_user_session,
    get_last_user_username,
    get_min_max_uids,
    get_sessions,
    get_users,
  },
  ipc::PendingSession,
  power::PowerOption,
  text::Text,
  ui::{
    common::{masked::MaskedString, menu::Menu, style::Theme},
    power::Power,
    sessions::{Session, SessionSource, SessionType},
    users::User,
  },
};

// `startx` wants an absolute path to the executable as a first argument.
// We don't want to resolve the session command in the greeter though, so it should be additionally wrapped with a known noop command (like `/usr/bin/env`).
const DEFAULT_XSESSION_WRAPPER: &str = "startx /usr/bin/env";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OptionArgument {
  None,
  Required,
  Optional,
}

#[derive(Clone, Debug)]
struct OptionSpecification {
  short: Option<char>,
  long: Option<String>,
  argument: OptionArgument,
  repeatable: bool,
}

impl OptionSpecification {
  fn new(short: &str, long: &str, argument: OptionArgument, repeatable: bool) -> Self {
    Self {
      short: (!short.is_empty()).then(|| short.chars().next().expect("validated short option")),
      long: (!long.is_empty()).then(|| long.to_owned()),
      argument,
      repeatable,
    }
  }

  fn canonical_name(&self) -> String {
    self
      .long
      .clone()
      .or_else(|| self.short.map(|short| short.to_string()))
      .expect("option has a name")
  }

  fn normalized(&self, value: Option<&str>) -> OsString {
    let name = self
      .long
      .as_ref()
      .map(|long| format!("--{long}"))
      .or_else(|| self.short.map(|short| format!("-{short}")))
      .expect("option has a name");

    match value {
      Some(value) => format!("{name}={value}").into(),
      None => name.into(),
    }
  }
}

/// `getopts` deliberately reports an option name but not the argv span that
/// caused an error. Keep the option schema beside it so tolerant parsing can
/// discard exactly the malformed occurrence instead of guessing its index.
#[derive(Clone, Debug)]
pub(crate) struct CliOptions {
  parser: Options,
  specifications: Vec<OptionSpecification>,
}

impl CliOptions {
  fn new() -> Self {
    Self {
      parser: Options::new(),
      specifications: Vec::new(),
    }
  }

  fn optflag(&mut self, short: &str, long: &str, description: &str) {
    self.parser.optflag(short, long, description);
    self
      .specifications
      .push(OptionSpecification::new(short, long, OptionArgument::None, false));
  }

  fn optflagopt(&mut self, short: &str, long: &str, description: &str, hint: &str) {
    self.parser.optflagopt(short, long, description, hint);
    self
      .specifications
      .push(OptionSpecification::new(short, long, OptionArgument::Optional, false));
  }

  fn optopt(&mut self, short: &str, long: &str, description: &str, hint: &str) {
    self.parser.optopt(short, long, description, hint);
    self
      .specifications
      .push(OptionSpecification::new(short, long, OptionArgument::Required, false));
  }

  fn optmulti(&mut self, short: &str, long: &str, description: &str, hint: &str) {
    self.parser.optmulti(short, long, description, hint);
    self
      .specifications
      .push(OptionSpecification::new(short, long, OptionArgument::Required, true));
  }

  pub(crate) fn parse<C: IntoIterator>(&self, args: C) -> getopts::Result
  where
    C::Item: AsRef<OsStr>,
  {
    self.parser.parse(args)
  }

  fn usage(&self, brief: &str) -> String {
    self.parser.usage(brief)
  }

  fn by_long(&self, name: &str) -> Option<(usize, &OptionSpecification)> {
    self
      .specifications
      .iter()
      .enumerate()
      .find(|(_, specification)| specification.long.as_deref() == Some(name))
  }

  fn by_short(&self, name: char) -> Option<(usize, &OptionSpecification)> {
    self
      .specifications
      .iter()
      .enumerate()
      .find(|(_, specification)| specification.short == Some(name))
  }
}

#[derive(Debug, Copy, Clone)]
pub enum AuthStatus {
  Success,
  Failure,
  Cancel,
}

impl Display for AuthStatus {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    write!(f, "{self:?}")
  }
}

impl Error for AuthStatus {}

// A mode represents the large section of the software, usually screens to be
// displayed, or the state of the application.
#[derive(Default, Debug, Copy, Clone, PartialEq)]
pub enum Mode {
  #[default]
  Username,
  Password,
  Action,
  Users,
  Command,
  Sessions,
  Power,
  Processing,
}

// This enum models how secret values should be displayed on terminal.
#[derive(Default, Debug, Clone)]
pub enum SecretDisplay {
  #[default]
  // All characters hidden.
  Hidden,
  // All characters are replaced by a placeholder character.
  Character(String),
}

impl SecretDisplay {
  pub fn show(&self) -> bool {
    match self {
      SecretDisplay::Hidden => false,
      SecretDisplay::Character(_) => true,
    }
  }
}

// This enum models text alignment options
#[derive(Default, Debug, Clone)]
pub enum GreetAlign {
  #[default]
  Center,
  Left,
  Right,
}

pub struct Greeter {
  pub debug: bool,
  pub logfile: String,
  pub logger: Option<WorkerGuard>,

  pub text: Text,
  pub config: Option<Matches>,
  pub settings: Settings,
  pub socket: String,
  pub stream: Option<Arc<RwLock<UnixStream>>>,

  // Current mode of the application, will define what actions are permitted.
  pub mode: Mode,
  // Mode the application will return to when exiting the current mode.
  pub previous_mode: Mode,
  // Offset the cursor should be at from its base position for the current mode.
  pub cursor_offset: i16,

  // Buffer to be used as a temporary editing zone for the various modes.
  // Previous buffer is saved when a transient screen has to use the buffer, to
  // be able to restore it when leaving the transient screen.
  pub previous_buffer: Option<String>,
  pub buffer: String,

  // Define the selected session and how to resolve it.
  pub session_source: SessionSource,
  // Whether unauthenticated users may replace the session with an arbitrary command.
  pub allow_command_editor: bool,
  // List of session files found on disk.
  pub session_paths: Vec<(PathBuf, SessionType)>,
  // Menu for session selection.
  pub sessions: Menu<Session>,
  // Wrapper command to prepend to non-X11 sessions.
  pub session_wrapper: Option<String>,
  // Wrapper command to prepend to X11 sessions.
  pub xsession_wrapper: Option<String>,

  // Whether user menu is enabled.
  pub user_menu: bool,
  // Whether Tab completion may enumerate eligible usernames.
  pub user_autocomplete: bool,
  // Menu for user selection.
  pub users: Menu<User>,
  // Current username. Masked to display the full name if available.
  pub username: MaskedString,
  // Prompt that should be displayed to ask for entry.
  pub prompt: Option<String>,

  // Whether the current edition prompt should be hidden.
  pub asking_for_secret: bool,
  // How should secrets be displayed?
  pub secret_display: SecretDisplay,

  // Whether last logged-in user should be remembered.
  pub remember: bool,
  // Whether last launched session (regardless of user) should be remembered.
  pub remember_session: bool,
  // Whether last launched session for the current user should be remembered.
  pub remember_user_session: bool,

  // Style object for the terminal UI
  pub theme: Theme,
  // Display the current time
  pub time: bool,
  // Time format
  pub time_format: Option<String>,
  pub refresh_rate: u16,
  // Greeting message (MOTD) to use to welcome the user.
  pub greeting: Option<String>,
  // Transaction message to show to the user.
  pub message: Option<String>,

  // Menu for power options.
  pub powers: Menu<Power>,
  // Whether to prefix the power commands with `setsid`.
  pub power_setsid: bool,

  // Run without greetd and simulate authentication for visual testing.
  pub mock: bool,

  pub kb_command: u8,
  pub kb_sessions: u8,
  pub kb_power: u8,

  // The software is waiting for a response from `greetd`.
  pub working: bool,
  // We are done working.
  pub done: bool,
  // Immutable state captured when the StartSession request is sent.
  pub(crate) pending_session: Option<PendingSession>,
  // Should we exit?
  pub exit: Option<AuthStatus>,
}

impl Default for Greeter {
  fn default() -> Self {
    Self {
      debug: false,
      logfile: String::new(),
      logger: None,
      text: Text::default(),
      config: None,
      settings: Settings::default(),
      socket: String::new(),
      stream: None,
      mode: Mode::default(),
      previous_mode: Mode::default(),
      cursor_offset: 0,
      previous_buffer: None,
      buffer: String::new(),
      session_source: SessionSource::default(),
      allow_command_editor: false,
      session_paths: Vec::new(),
      sessions: Menu::default(),
      session_wrapper: None,
      xsession_wrapper: None,
      user_menu: false,
      user_autocomplete: false,
      users: Menu::default(),
      username: MaskedString::default(),
      prompt: None,
      asking_for_secret: false,
      secret_display: SecretDisplay::default(),
      remember: false,
      remember_session: false,
      remember_user_session: false,
      theme: Theme::default(),
      time: false,
      time_format: None,
      refresh_rate: DEFAULT_REFRESH_RATE,
      greeting: None,
      message: None,
      powers: Menu::default(),
      power_setsid: false,
      mock: false,
      kb_command: 2,
      kb_sessions: 3,
      kb_power: 12,
      working: false,
      done: false,
      pending_session: None,
      exit: None,
    }
  }
}

impl Drop for Greeter {
  fn drop(&mut self) {
    self.scrub(true, false);
  }
}

impl Greeter {
  pub async fn new() -> Self {
    let mut greeter = Self::default();

    greeter.powers = Menu {
      title: text!(greeter, title_power),
      options: Default::default(),
      selected: 0,
    };

    #[cfg(not(test))]
    {
      let args = crate::arguments_after_program(env::args_os());

      if let Err(err) = greeter.parse_options(&args).await {
        eprintln!("{err}");
        print_usage(Greeter::options());

        process::exit(1);
      }

      greeter.connect().await;
    }

    greeter.logger = match crate::logger::init(greeter.debug, &greeter.logfile) {
      Ok(logger) => logger,
      Err(error) => {
        eprintln!(
          "tuigreet: warning: failed to initialize debug log {:?}: {error}",
          greeter.logfile
        );
        None
      },
    };

    let mut sessions = get_sessions(&greeter).unwrap_or_default();

    if greeter.mock && sessions.is_empty() {
      sessions = mock_sessions();
    }

    if let SessionSource::None = greeter.session_source
      && !sessions.is_empty()
    {
      greeter.session_source = SessionSource::Session(0);
    }

    greeter.sessions = Menu {
      title: text!(greeter, title_session),
      options: sessions,
      selected: 0,
    };

    // Free-form command caches created by older releases must not become active
    // again if an administrator later enables command remembering.
    #[cfg(not(test))]
    if !greeter.allow_command_editor {
      crate::info::delete_last_command();
    }

    // If we should remember the last logged-in user.
    if greeter.remember
      && let Some(username) = get_last_user_username()
    {
      greeter.username = MaskedString::from(username, get_last_user_name());

      #[cfg(not(test))]
      if !greeter.allow_command_editor {
        crate::info::delete_last_user_command(&greeter.username.value);
      }

      // If, on top of that, we should remember their last session.
      if greeter.remember_user_session {
        // See if we have the last free-form command from the user.
        if greeter.allow_command_editor
          && let Ok(command) = get_last_user_command(&greeter.username.value)
        {
          greeter.session_source = SessionSource::Command(command);
        }

        // If a session was saved, use it and its name.
        if let Ok(ref session_path) = get_last_user_session(&greeter.username.value) {
          // Set the selected menu option and the session source.
          if let Some(index) = greeter
            .sessions
            .options
            .iter()
            .position(|Session { path, .. }| path.as_deref() == Some(session_path))
          {
            greeter.sessions.selected = index;
            greeter.session_source = SessionSource::Session(greeter.sessions.selected);
          }
        }
      }
    }

    greeter.select_only_user();

    // Same thing, but not user specific.
    if greeter.remember_session {
      if greeter.allow_command_editor
        && let Ok(command) = get_last_command()
      {
        greeter.session_source = SessionSource::Command(command.trim().to_string());
      }

      if let Ok(ref session_path) = get_last_session_path()
        && let Some(index) = greeter
          .sessions
          .options
          .iter()
          .position(|Session { path, .. }| path.as_deref() == Some(session_path))
      {
        greeter.sessions.selected = index;
        greeter.session_source = SessionSource::Session(greeter.sessions.selected);
      }
    }

    greeter
  }

  // Scrub memory of all data, unless `soft` is true, in which case, we will
  // keep the username (can happen if a wrong password was entered, we want to
  // give the user another chance, as PAM would).
  fn scrub(&mut self, scrub_message: bool, soft: bool) {
    self.buffer.zeroize();
    self.prompt.zeroize();

    if !soft {
      self.username.zeroize();
    }

    if scrub_message {
      self.message.zeroize();
    }
  }

  // Reset the software to its initial state.
  pub async fn reset(&mut self, soft: bool) {
    if soft {
      self.mode = Mode::Password;
      self.previous_mode = Mode::Password;
    } else {
      self.mode = Mode::Username;
      self.previous_mode = Mode::Username;
    }

    self.working = false;
    self.done = false;
    self.pending_session = None;

    self.scrub(false, soft);
    self.connect().await;
  }

  // Connect to `greetd` and return a stream we can safely write to.
  pub async fn connect(&mut self) {
    if self.mock {
      tracing::info!("mock mode: skipping greetd socket connection");
      return;
    }

    if self.socket.is_empty() {
      self.socket = match env::var("GREETD_SOCK") {
        Ok(socket) => socket,
        Err(_) => {
          eprintln!("GREETD_SOCK must be defined");
          process::exit(1);
        },
      };
    }

    match UnixStream::connect(&self.socket).await {
      Ok(stream) => self.stream = Some(Arc::new(RwLock::new(stream))),

      Err(err) => {
        eprintln!("{err}");
        process::exit(1);
      },
    }
  }

  pub fn config(&self) -> &Matches {
    self.config.as_ref().unwrap()
  }

  pub async fn stream(&self) -> RwLockWriteGuard<'_, UnixStream> {
    self.stream.as_ref().unwrap().write().await
  }

  pub fn option(&self, name: &str) -> Option<String> {
    self.config().opt_str(name)
  }

  pub fn options_multi(&self, name: &str) -> Option<Vec<String>> {
    match self.config().opt_present(name) {
      true => Some(self.config().opt_strs(name)),
      false => None,
    }
  }

  // Returns the width of the main window where content is displayed from the
  // provided arguments.
  pub fn width(&self) -> u16 {
    self.settings.width
  }

  // Returns the padding of the screen from the provided arguments.
  pub fn window_padding(&self) -> u16 {
    self.settings.window_padding
  }

  // Returns the padding of the main window where content is displayed from the
  // provided arguments.
  pub fn container_padding(&self) -> u16 {
    self.settings.container_padding + 1
  }

  // Returns the spacing between each prompt from the provided arguments.
  pub fn prompt_padding(&self) -> u16 {
    self.settings.prompt_padding
  }

  pub fn greet_align(&self) -> GreetAlign {
    match self.settings.greet_align.as_str() {
      "left" => GreetAlign::Left,
      "right" => GreetAlign::Right,
      _ => GreetAlign::Center,
    }
  }

  pub(crate) fn options() -> CliOptions {
    let mut opts = CliOptions::new();

    let xsession_wrapper_desc =
      format!("wrapper command to initialize X server and launch X11 sessions (default: {DEFAULT_XSESSION_WRAPPER})");

    opts.optflag("h", "help", "show this usage information");
    opts.optflag("v", "version", "print version information");
    opts.optopt("", "config", "load an explicit TOML configuration file", "FILE");
    opts.optflag(
      "",
      "check-config",
      "show active configuration files, validate them, and exit",
    );
    opts.optflagopt(
      "d",
      "debug",
      "enable debug logging to the provided file, or to /tmp/tuigreet.log",
      "FILE",
    );
    opts.optopt("c", "cmd", "command to run", "COMMAND");
    opts.optflag(
      "",
      "allow-command-editor",
      "allow unauthenticated users to replace the session command (unsafe)",
    );
    opts.optflag(
      "",
      "no-command-editor",
      "disable the command editor, overriding configuration",
    );
    opts.optmulti(
      "",
      "env",
      "environment variables to run the default session with (can appear more than once)",
      "KEY=VALUE",
    );
    opts.optopt("s", "sessions", "colon-separated list of Wayland session paths", "DIRS");
    opts.optopt(
      "",
      "session-wrapper",
      "wrapper command to initialize the non-X11 session",
      "'CMD [ARGS]...'",
    );
    opts.optopt("x", "xsessions", "colon-separated list of X11 session paths", "DIRS");
    opts.optopt(
      "",
      "xsession-wrapper",
      xsession_wrapper_desc.as_str(),
      "'CMD [ARGS]...'",
    );
    opts.optflag("", "no-xsession-wrapper", "do not wrap commands for X11 sessions");
    opts.optopt("w", "width", "width of the main prompt (default: 80)", "WIDTH");
    opts.optflag("i", "issue", "show the host's issue file");
    opts.optopt("g", "greeting", "show custom text above login prompt", "GREETING");
    opts.optflag("t", "time", "display the current date and time");
    opts.optopt(
      "",
      "time-format",
      "custom strftime format for displaying date and time",
      "FORMAT",
    );
    opts.optopt(
      "",
      "refresh-rate",
      "screen refresh rate in frames per second (default: 2, maximum: 240)",
      "FPS",
    );
    opts.optflag("r", "remember", "remember last logged-in username");
    opts.optflag("", "remember-session", "remember last selected session");
    opts.optflag(
      "",
      "remember-user-session",
      "remember last selected session for each user",
    );
    opts.optflag("", "user-menu", "allow graphical selection of users from a menu");
    opts.optflag("", "user-autocomplete", "allow Tab completion of usernames");
    opts.optopt(
      "",
      "user-menu-min-uid",
      "minimum UID exposed by user menu or completion",
      "UID",
    );
    opts.optopt(
      "",
      "user-menu-max-uid",
      "maximum UID exposed by user menu or completion",
      "UID",
    );
    opts.optopt("", "theme", "define the application theme colors", "THEME");
    opts.optflag("", "asterisks", "display asterisks when a secret is typed");
    opts.optopt(
      "",
      "asterisks-char",
      "characters to be used to redact secrets (default: *)",
      "CHARS",
    );
    opts.optopt(
      "",
      "window-padding",
      "padding inside the terminal area (default: 0)",
      "PADDING",
    );
    opts.optopt(
      "",
      "container-padding",
      "padding inside the main prompt container (default: 1)",
      "PADDING",
    );
    opts.optopt(
      "",
      "prompt-padding",
      "padding between prompt rows (default: 1)",
      "PADDING",
    );
    opts.optopt(
      "",
      "greet-align",
      "alignment of the greeting text in the main prompt container (default: 'center')",
      "[left|center|right]",
    );

    opts.optopt(
      "",
      "power-shutdown",
      "command to run to shut down the system",
      "'CMD [ARGS]...'",
    );
    opts.optopt(
      "",
      "power-reboot",
      "command to run to reboot the system",
      "'CMD [ARGS]...'",
    );
    opts.optopt(
      "",
      "power-suspend",
      "command to run to suspend the system",
      "'CMD [ARGS]...'",
    );
    opts.optopt(
      "",
      "power-hibernate",
      "command to run to hibernate the system",
      "'CMD [ARGS]...'",
    );
    opts.optflag("", "power-no-setsid", "do not prefix power commands with setsid");
    opts.optflag(
      "",
      "mock",
      "run without greetd and simulate authentication for visual testing",
    );

    opts.optopt("", "kb-command", "F-key to use to open the command menu", "[1-12]");
    opts.optopt("", "kb-sessions", "F-key to use to open the sessions menu", "[1-12]");
    opts.optopt("", "kb-power", "F-key to use to open the power menu", "[1-12]");

    opts
  }

  // Parses command line arguments to configured the software accordingly.
  pub async fn parse_options<S>(&mut self, args: &[S]) -> Result<(), Box<dyn Error>>
  where
    S: AsRef<OsStr>,
  {
    let opts = Greeter::options();

    let (matches, cli_warnings) = parse_options_ignoring_invalid(&opts, args);
    for warning in cli_warnings {
      eprintln!("tuigreet: warning: {warning}");
    }
    self.config = Some(matches);

    if self.config().opt_present("help") {
      print_usage(opts);
      process::exit(0);
    }
    if self.config().opt_present("version") {
      print_version();
      process::exit(0);
    }

    let (settings, warnings) = config::load(self.config());
    for warning in warnings {
      eprintln!("tuigreet: warning: {warning}");
    }
    self.settings = settings.clone();

    self.powers.title = text!(self, title_power);

    self.debug = settings.debug;
    self.logfile = settings.logfile.clone();
    let theme = settings.theme.specification();
    if !theme.is_empty() {
      self.theme = Theme::parse(&theme);
    }
    self.secret_display = if settings.asterisks {
      SecretDisplay::Character(settings.asterisks_chars.clone())
    } else {
      SecretDisplay::Hidden
    };
    self.time = settings.time;
    self.time_format = settings.time_format.clone();
    self.refresh_rate = settings.refresh_rate;
    self.user_menu = settings.user_menu;
    self.user_autocomplete = settings.user_autocomplete;

    if self.user_menu || self.user_autocomplete {
      let (min_uid, max_uid) = get_min_max_uids(settings.min_uid, settings.max_uid);

      tracing::info!("min/max UIDs are {}/{}", min_uid, max_uid);

      self.users = Menu {
        title: text!(self, title_users),
        options: get_users(min_uid, max_uid),
        selected: 0,
      };

      tracing::info!("found {} eligible users", self.users.options.len());
    }

    self.remember = settings.remember;
    self.remember_session = settings.remember_session;
    self.remember_user_session = settings.remember_user_session;
    self.allow_command_editor = settings.allow_command_editor;
    self.greeting = settings.greeting.clone();

    // If the `--cmd` argument is provided, it will override the selected session.
    if let Some(command) = settings.command.clone() {
      let environment = (!settings.environment.is_empty()).then(|| settings.environment.clone());
      self.session_source = SessionSource::DefaultCommand(command, environment);
    }

    for dir in &settings.sessions {
      self.add_session_path(PathBuf::from(dir), SessionType::Wayland);
    }
    for dir in &settings.xsessions {
      self.add_session_path(PathBuf::from(dir), SessionType::X11);
    }
    self.session_wrapper = settings.session_wrapper.clone();
    self.xsession_wrapper = settings.xsession_wrapper.clone();
    if settings.issue {
      self.greeting = get_issue();
    }

    self.powers.options.push(Power {
      action: PowerOption::Shutdown,
      label: text!(self, shutdown),
      command: settings
        .power_shutdown
        .clone()
        .or_else(|| crate::power::default_command(PowerOption::Shutdown)),
    });

    self.powers.options.push(Power {
      action: PowerOption::Reboot,
      label: text!(self, reboot),
      command: settings
        .power_reboot
        .clone()
        .or_else(|| crate::power::default_command(PowerOption::Reboot)),
    });

    self.powers.options.push(Power {
      action: PowerOption::Suspend,
      label: text!(self, suspend),
      command: settings
        .power_suspend
        .clone()
        .or_else(|| crate::power::default_command(PowerOption::Suspend)),
    });

    self.powers.options.push(Power {
      action: PowerOption::Hibernate,
      label: text!(self, hibernate),
      command: settings
        .power_hibernate
        .clone()
        .or_else(|| crate::power::default_command(PowerOption::Hibernate)),
    });

    self.power_setsid = settings.power_setsid;
    self.mock = settings.mock;
    self.kb_command = settings.kb_command;
    self.kb_sessions = settings.kb_sessions;
    self.kb_power = settings.kb_power;

    Ok(())
  }

  pub fn reload_settings(&mut self, mut settings: Settings) -> Vec<String> {
    let mut warnings = Vec::new();

    if settings.debug != self.debug || settings.logfile != self.logfile {
      warnings.push("general.debug and general.log-file require a restart; keeping their current values".into());
      settings.debug = self.debug;
      settings.logfile.clone_from(&self.logfile);
    }
    if settings.mock != self.mock {
      warnings.push("general.mock requires a restart; keeping its current value".into());
      settings.mock = self.mock;
    }

    let selected_path = Session::get_selected(self).and_then(|session| session.path.clone());
    let had_default_command = matches!(self.session_source, SessionSource::DefaultCommand(_, _));

    self.theme = Theme::parse(&settings.theme.specification());
    self.secret_display = if settings.asterisks {
      SecretDisplay::Character(settings.asterisks_chars.clone())
    } else {
      SecretDisplay::Hidden
    };
    self.time = settings.time;
    self.time_format.clone_from(&settings.time_format);
    self.refresh_rate = settings.refresh_rate;
    self.user_menu = settings.user_menu;
    self.user_autocomplete = settings.user_autocomplete;

    if self.user_menu || self.user_autocomplete {
      let (min_uid, max_uid) = get_min_max_uids(settings.min_uid, settings.max_uid);
      self.users = Menu {
        title: text!(self, title_users),
        options: get_users(min_uid, max_uid),
        selected: 0,
      };
    } else {
      self.users.options.clear();
      if self.mode == Mode::Users {
        self.mode = Mode::Username;
      }
    }

    self.remember = settings.remember;
    self.remember_session = settings.remember_session;
    self.remember_user_session = settings.remember_user_session;
    self.allow_command_editor = settings.allow_command_editor;
    if !self.allow_command_editor && self.mode == Mode::Command {
      self.buffer = self.previous_buffer.take().unwrap_or_default();
      self.cursor_offset = 0;
      self.mode = self.previous_mode;
    }
    #[cfg(not(test))]
    if !self.allow_command_editor {
      crate::info::delete_last_command();
      if !self.username.value.is_empty() {
        crate::info::delete_last_user_command(&self.username.value);
      }
    }
    self.greeting.clone_from(&settings.greeting);
    if settings.issue {
      self.greeting = get_issue();
    }

    self.session_paths.clear();
    for dir in &settings.sessions {
      self.add_session_path(PathBuf::from(dir), SessionType::Wayland);
    }
    for dir in &settings.xsessions {
      self.add_session_path(PathBuf::from(dir), SessionType::X11);
    }
    self.session_wrapper.clone_from(&settings.session_wrapper);
    self.xsession_wrapper.clone_from(&settings.xsession_wrapper);
    self.sessions.options = get_sessions(self).unwrap_or_default();
    self.sessions.selected = selected_path
      .as_deref()
      .and_then(|path| {
        self
          .sessions
          .options
          .iter()
          .position(|session| session.path.as_deref() == Some(path))
      })
      .unwrap_or(0);

    if let Some(command) = settings.command.clone() {
      let environment = (!settings.environment.is_empty()).then(|| settings.environment.clone());
      self.session_source = SessionSource::DefaultCommand(command, environment);
    } else if !self.allow_command_editor && matches!(&self.session_source, SessionSource::Command(_)) {
      self.session_source = if self.sessions.options.is_empty() {
        SessionSource::None
      } else {
        SessionSource::Session(0)
      };
    } else if selected_path.is_some() && !self.sessions.options.is_empty() {
      self.session_source = SessionSource::Session(self.sessions.selected);
    } else if selected_path.is_some() {
      self.session_source = SessionSource::None;
    } else if had_default_command {
      self.session_source = if self.sessions.options.is_empty() {
        SessionSource::None
      } else {
        SessionSource::Session(0)
      };
    }

    self.powers.options.clear();
    for (action, command) in [
      (PowerOption::Shutdown, settings.power_shutdown.clone()),
      (PowerOption::Reboot, settings.power_reboot.clone()),
      (PowerOption::Suspend, settings.power_suspend.clone()),
      (PowerOption::Hibernate, settings.power_hibernate.clone()),
    ] {
      let label = match action {
        PowerOption::Shutdown => text!(self, shutdown),
        PowerOption::Reboot => text!(self, reboot),
        PowerOption::Suspend => text!(self, suspend),
        PowerOption::Hibernate => text!(self, hibernate),
      };
      self.powers.options.push(Power {
        action,
        label,
        command: command.or_else(|| crate::power::default_command(action)),
      });
    }

    self.power_setsid = settings.power_setsid;
    self.kb_command = settings.kb_command;
    self.kb_sessions = settings.kb_sessions;
    self.kb_power = settings.kb_power;
    self.settings = settings;

    warnings
  }

  pub fn set_prompt(&mut self, prompt: &str) {
    self.prompt = if prompt.ends_with(' ') {
      Some(prompt.into())
    } else {
      Some(format!("{prompt} "))
    };
  }

  fn select_only_user(&mut self) {
    if self.username.value.is_empty()
      && self.user_menu
      && let [user] = self.users.options.as_slice()
    {
      self.username = MaskedString::from(user.username.clone(), user.name.clone());
    }
  }

  fn add_session_path(&mut self, path: PathBuf, session_type: SessionType) {
    if !self
      .session_paths
      .iter()
      .any(|(known_path, known_type)| known_path == &path && known_type == &session_type)
    {
      self.session_paths.push((path, session_type));
    }
  }

  pub fn remove_prompt(&mut self) {
    self.prompt = None;
  }

  // Computes the size of the prompt to help determine where input should start.
  pub fn prompt_width(&self) -> usize {
    match &self.prompt {
      None => 0,
      Some(prompt) => prompt.chars().count(),
    }
  }
}

fn mock_sessions() -> Vec<Session> {
  [
    ("mock-wayland", "Mock Wayland", SessionType::Wayland),
    ("mock-x11", "Mock X11", SessionType::X11),
    ("mock-shell", "Mock shell", SessionType::None),
  ]
  .into_iter()
  .map(|(slug, name, session_type)| Session {
    slug: Some(slug.to_string()),
    name: name.to_string(),
    command: "true".to_string(),
    session_type,
    path: None,
    xdg_desktop_names: None,
  })
  .collect()
}

fn print_usage(opts: CliOptions) {
  eprint!("{}", opts.usage("Usage: tuigreet [OPTIONS]"));
}

pub fn print_information<S>(args: &[S]) -> bool
where
  S: AsRef<OsStr>,
{
  if args
    .iter()
    .any(|arg| matches!(arg.as_ref().to_str(), Some("-h" | "--help")))
  {
    print_usage(Greeter::options());
    true
  } else if args.iter().any(|arg| arg.as_ref().to_str() == Some("--check-config")) {
    let opts = Greeter::options();
    let (matches, warnings) = parse_options_ignoring_invalid(&opts, args);
    for warning in &warnings {
      eprintln!("tuigreet: warning: {warning}");
    }
    let config_valid = crate::config::check(&matches);
    let valid = warnings.is_empty() && config_valid;
    if !valid {
      process::exit(1);
    }
    true
  } else if args
    .iter()
    .any(|arg| matches!(arg.as_ref().to_str(), Some("-v" | "--version")))
  {
    print_version();
    true
  } else {
    false
  }
}

fn parse_options_ignoring_invalid<S>(opts: &CliOptions, args: &[S]) -> (Matches, Vec<String>)
where
  S: AsRef<OsStr>,
{
  let (args, mut warnings) = recover_options(opts, args);

  let matches = match opts.parse(&args) {
    Ok(matches) => matches,
    Err(error) => {
      // The recovered argv is generated from the same schema as `getopts`, so
      // this can only indicate a bug in the recovery code. Startup must remain
      // available even then.
      warnings.push(format!(
        "could not recover command-line options ({error}); ignoring all command-line options"
      ));
      opts
        .parse(std::iter::empty::<&str>())
        .expect("the tuigreet option schema has no required options")
    },
  };

  (matches, warnings)
}

fn recover_options<S>(opts: &CliOptions, args: &[S]) -> (Vec<OsString>, Vec<String>)
where
  S: AsRef<OsStr>,
{
  let mut recovered = Vec::new();
  let mut warnings = Vec::new();
  let mut seen = vec![false; opts.specifications.len()];
  let mut index = 0;
  let mut options_ended = false;

  while index < args.len() {
    let raw = args[index].as_ref();
    let Some(argument) = raw.to_str() else {
      warnings.push(format!("argument {raw:?} is not valid UTF-8; ignoring it"));
      index += 1;
      continue;
    };

    if options_ended {
      warn_positional(argument, &mut warnings);
      index += 1;
      continue;
    }

    if argument == "--" {
      options_ended = true;
      index += 1;
      continue;
    }

    if let Some(long) = argument.strip_prefix("--") {
      let (name, attached) = long
        .split_once('=')
        .map_or((long, None), |(name, value)| (name, Some(value)));
      let Some((option_index, specification)) = opts.by_long(name) else {
        warn_unknown(name, argument, &mut warnings);
        index += 1;
        continue;
      };

      match specification.argument {
        OptionArgument::None => {
          if attached.is_some() {
            warnings.push(format!(
              "Option '{name}' does not take an argument; ignoring {argument}"
            ));
          } else {
            retain_option(
              option_index,
              specification,
              None,
              argument,
              &mut seen,
              &mut recovered,
              &mut warnings,
            );
          }
          index += 1;
        },
        OptionArgument::Optional => {
          retain_option(
            option_index,
            specification,
            attached,
            argument,
            &mut seen,
            &mut recovered,
            &mut warnings,
          );
          index += 1;
        },
        OptionArgument::Required => {
          if let Some(value) = attached {
            retain_option(
              option_index,
              specification,
              Some(value),
              argument,
              &mut seen,
              &mut recovered,
              &mut warnings,
            );
            index += 1;
          } else if let Some(raw_value) = args.get(index + 1).map(AsRef::as_ref) {
            match raw_value.to_str() {
              Some(value) => retain_option(
                option_index,
                specification,
                Some(value),
                argument,
                &mut seen,
                &mut recovered,
                &mut warnings,
              ),
              None => warnings.push(format!(
                "argument {raw_value:?} to option '{name}' is not valid UTF-8; ignoring {argument} and its argument"
              )),
            }
            index += 2;
          } else {
            warnings.push(format!("Argument to option '{name}' missing; ignoring {argument}"));
            index += 1;
          }
        },
      }
      continue;
    }

    let Some(cluster) = argument.strip_prefix('-').filter(|cluster| !cluster.is_empty()) else {
      warn_positional(argument, &mut warnings);
      index += 1;
      continue;
    };

    let mut consumed_next = false;
    for (offset, short) in cluster.char_indices() {
      let Some((option_index, specification)) = opts.by_short(short) else {
        warn_unknown(&short.to_string(), &format!("-{short}"), &mut warnings);
        continue;
      };

      if specification.argument == OptionArgument::None {
        retain_option(
          option_index,
          specification,
          None,
          &format!("-{short}"),
          &mut seen,
          &mut recovered,
          &mut warnings,
        );
        continue;
      }

      let value_offset = offset + short.len_utf8();
      if value_offset < cluster.len() {
        retain_option(
          option_index,
          specification,
          Some(&cluster[value_offset..]),
          argument,
          &mut seen,
          &mut recovered,
          &mut warnings,
        );
      } else {
        match specification.argument {
          OptionArgument::Required => {
            if let Some(raw_value) = args.get(index + 1).map(AsRef::as_ref) {
              match raw_value.to_str() {
                Some(value) => retain_option(
                  option_index,
                  specification,
                  Some(value),
                  argument,
                  &mut seen,
                  &mut recovered,
                  &mut warnings,
                ),
                None => warnings.push(format!(
                  "argument {raw_value:?} to option '{short}' is not valid UTF-8; ignoring {argument} and its argument"
                )),
              }
              consumed_next = true;
            } else {
              warnings.push(format!("Argument to option '{short}' missing; ignoring {argument}"));
            }
          },
          OptionArgument::Optional => {
            if let Some(raw_value) = args.get(index + 1).map(AsRef::as_ref) {
              match raw_value.to_str() {
                Some(value) if !is_option(value) => {
                  retain_option(
                    option_index,
                    specification,
                    Some(value),
                    argument,
                    &mut seen,
                    &mut recovered,
                    &mut warnings,
                  );
                  consumed_next = true;
                },
                Some(_) => retain_option(
                  option_index,
                  specification,
                  None,
                  argument,
                  &mut seen,
                  &mut recovered,
                  &mut warnings,
                ),
                None => {
                  warnings.push(format!(
                    "optional argument {raw_value:?} to option '{short}' is not valid UTF-8; ignoring {argument} and its argument"
                  ));
                  consumed_next = true;
                },
              }
            } else {
              retain_option(
                option_index,
                specification,
                None,
                argument,
                &mut seen,
                &mut recovered,
                &mut warnings,
              );
            }
          },
          OptionArgument::None => unreachable!(),
        }
      }

      // As in getopts, an argument-taking short option owns the rest of its
      // cluster, so no following character can be another option.
      break;
    }

    index += 1 + usize::from(consumed_next);
  }

  (recovered, warnings)
}

fn retain_option(
  option_index: usize,
  specification: &OptionSpecification,
  value: Option<&str>,
  spelling: &str,
  seen: &mut [bool],
  recovered: &mut Vec<OsString>,
  warnings: &mut Vec<String>,
) {
  if !specification.repeatable && seen[option_index] {
    warnings.push(format!(
      "Option '{}' given more than once; ignoring {spelling}",
      specification.canonical_name()
    ));
    return;
  }

  seen[option_index] = true;
  recovered.push(specification.normalized(value));
}

fn is_option(argument: &str) -> bool {
  argument.starts_with('-') && argument.len() > 1
}

fn warn_unknown(name: &str, spelling: &str, warnings: &mut Vec<String>) {
  warnings.push(format!("Unrecognized option: '{name}'; ignoring {spelling}"));
}

fn warn_positional(argument: &str, warnings: &mut Vec<String>) {
  warnings.push(format!("unexpected positional argument '{argument}'; ignoring it"));
}

fn print_version() {
  println!("tuigreet {} ({})", env!("VERSION"), env!("TARGET"));
  println!("Copyright (C) 2020 Antoine POPINEAU <https://github.com/apognu/tuigreet>.");
  println!("Licensed under GPLv3+ (GNU GPL version 3 or later).");
  println!();
  println!("This is free software, you are welcome to redistribute it under some conditions.");
  println!("There is NO WARRANTY, to the extent provided by law.");
}

#[cfg(test)]
mod test {
  use std::{ffi::OsString, os::unix::ffi::OsStringExt, path::PathBuf};

  use super::{mock_sessions, parse_options_ignoring_invalid, print_information};
  use crate::{
    Greeter,
    Mode,
    SecretDisplay,
    ui::{
      common::menu::Menu,
      sessions::{SessionSource, SessionType},
      users::User,
    },
  };

  #[test]
  fn test_prompt_width() {
    let mut greeter = Greeter::default();
    greeter.prompt = None;

    assert_eq!(greeter.prompt_width(), 0);

    greeter.prompt = Some("Hello:".into());

    assert_eq!(greeter.prompt_width(), 6);
  }

  #[test]
  fn test_set_prompt() {
    let mut greeter = Greeter::default();

    greeter.set_prompt("Hello:");

    assert_eq!(greeter.prompt, Some("Hello: ".into()));

    greeter.set_prompt("Hello World: ");

    assert_eq!(greeter.prompt, Some("Hello World: ".into()));

    greeter.remove_prompt();

    assert_eq!(greeter.prompt, None);
  }

  #[test]
  fn reload_settings_updates_runtime_values_but_keeps_startup_only_values() {
    let mut greeter = Greeter::default();
    greeter.debug = true;
    greeter.logfile = "/tmp/original.log".into();
    greeter.mock = true;
    greeter.allow_command_editor = true;
    greeter.mode = Mode::Command;
    greeter.previous_mode = Mode::Username;
    greeter.buffer = "untrusted command".into();
    greeter.previous_buffer = Some("username buffer".into());
    let mut settings = crate::config::Settings {
      debug: false,
      logfile: "/tmp/reloaded.log".into(),
      mock: false,
      time: true,
      refresh_rate: 60,
      asterisks: true,
      asterisks_chars: "#".into(),
      kb_command: 5,
      ..Default::default()
    };
    settings.theme.text = Some("red".into());

    let warnings = greeter.reload_settings(settings);

    assert_eq!(warnings.len(), 2);
    assert!(greeter.debug);
    assert_eq!(greeter.logfile, "/tmp/original.log");
    assert!(greeter.mock);
    assert!(!greeter.allow_command_editor);
    assert_eq!(greeter.mode, Mode::Username);
    assert_eq!(greeter.buffer, "username buffer");
    assert!(greeter.time);
    assert_eq!(greeter.refresh_rate, 60);
    assert!(matches!(greeter.secret_display, SecretDisplay::Character(ref value) if value == "#"));
    assert_eq!(greeter.kb_command, 5);
  }

  #[test]
  fn test_information_options() {
    assert!(print_information(&["--help"]));
    assert!(print_information(&["-v"]));
    assert!(!print_information(&["--time"]));
  }

  #[test]
  fn program_name_is_not_an_option() {
    let args = crate::arguments_after_program(["tuigreet", "--mock"]);
    let (matches, warnings) = parse_options_ignoring_invalid(&Greeter::options(), &args);

    assert!(matches.opt_present("mock"));
    assert!(matches.free.is_empty());
    assert!(warnings.is_empty());
  }

  #[test]
  fn duplicate_options_only_discard_the_duplicate_occurrence() {
    let (matches, warnings) = parse_options_ignoring_invalid(&Greeter::options(), &[
      "--check-config",
      "--config",
      "/first.toml",
      "--config",
      "/second.toml",
      "-t",
      "--time",
      "--mock",
    ]);

    assert!(matches.opt_present("check-config"));
    assert_eq!(matches.opt_str("config").as_deref(), Some("/first.toml"));
    assert!(matches.opt_present("time"));
    assert!(matches.opt_present("mock"));
    assert_eq!(warnings.len(), 2);
    assert!(warnings.iter().all(|warning| warning.contains("given more than once")));
  }

  #[test]
  fn duplicate_short_flags_do_not_remove_earlier_arguments() {
    let (matches, warnings) = parse_options_ignoring_invalid(&Greeter::options(), &[
      "--check-config",
      "--config",
      "/config.toml",
      "--mock",
      "-t",
      "-t",
    ]);

    assert!(matches.opt_present("check-config"));
    assert_eq!(matches.opt_str("config").as_deref(), Some("/config.toml"));
    assert!(matches.opt_present("mock"));
    assert!(matches.opt_present("time"));
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("Option 'time' given more than once"));

    let (matches, warnings) =
      parse_options_ignoring_invalid(&Greeter::options(), &["-c", "first", "-csecond", "--mock"]);
    assert_eq!(matches.opt_str("cmd").as_deref(), Some("first"));
    assert!(matches.opt_present("mock"));
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("Option 'cmd' given more than once"));
  }

  #[test]
  fn attached_and_detached_values_are_preserved() {
    let (matches, warnings) = parse_options_ignoring_invalid(&Greeter::options(), &[
      "--config=/config.toml",
      "-chello world",
      "--debug=/tmp/debug.log",
      "--env=A=B",
      "--env",
      "C=D",
    ]);

    assert_eq!(matches.opt_str("config").as_deref(), Some("/config.toml"));
    assert_eq!(matches.opt_str("cmd").as_deref(), Some("hello world"));
    assert_eq!(matches.opt_str("debug").as_deref(), Some("/tmp/debug.log"));
    assert_eq!(matches.opt_strs("env"), ["A=B", "C=D"]);
    assert!(warnings.is_empty());
  }

  #[test]
  fn missing_values_do_not_discard_valid_options() {
    let (matches, warnings) = parse_options_ignoring_invalid(&Greeter::options(), &["--mock", "--time", "--config"]);

    assert!(matches.opt_present("mock"));
    assert!(matches.opt_present("time"));
    assert!(!matches.opt_present("config"));
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("Argument to option 'config' missing"));

    let (matches, warnings) = parse_options_ignoring_invalid(&Greeter::options(), &["--mock", "-c"]);
    assert!(matches.opt_present("mock"));
    assert!(!matches.opt_present("cmd"));
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("Argument to option 'c' missing"));
  }

  #[test]
  fn valid_members_of_unknown_short_clusters_are_preserved() {
    let (matches, warnings) = parse_options_ignoring_invalid(&Greeter::options(), &["-tzr", "-icuname", "--mock"]);

    assert!(matches.opt_present("time"));
    assert!(matches.opt_present("remember"));
    assert!(matches.opt_present("issue"));
    assert_eq!(matches.opt_str("cmd").as_deref(), Some("uname"));
    assert!(matches.opt_present("mock"));
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("Unrecognized option: 'z'"));
  }

  #[test]
  fn malformed_long_options_do_not_affect_later_options() {
    let (matches, warnings) = parse_options_ignoring_invalid(&Greeter::options(), &[
      "--unknown=value",
      "--mock=yes",
      "--time",
      "--remember",
    ]);

    assert!(!matches.opt_present("mock"));
    assert!(matches.opt_present("time"));
    assert!(matches.opt_present("remember"));
    assert_eq!(warnings.len(), 2);
    assert!(warnings[0].contains("Unrecognized option: 'unknown'"));
    assert!(warnings[1].contains("does not take an argument"));
  }

  #[test]
  fn positional_arguments_are_ignored_without_stopping_option_parsing() {
    let (matches, warnings) =
      parse_options_ignoring_invalid(&Greeter::options(), &["first", "--mock", "second", "--time"]);

    assert!(matches.opt_present("mock"));
    assert!(matches.opt_present("time"));
    assert!(matches.free.is_empty());
    assert_eq!(warnings.len(), 2);
    assert!(warnings.iter().all(|warning| warning.contains("positional argument")));
  }

  #[test]
  fn double_dash_ends_option_parsing() {
    let (matches, warnings) = parse_options_ignoring_invalid(&Greeter::options(), &["--time", "--", "--mock", "tail"]);

    assert!(matches.opt_present("time"));
    assert!(!matches.opt_present("mock"));
    assert!(matches.free.is_empty());
    assert_eq!(warnings.len(), 2);
    assert!(warnings.iter().all(|warning| warning.contains("positional argument")));
  }

  #[test]
  fn non_utf8_tokens_only_discard_their_own_option_span() {
    let invalid = OsString::from_vec(vec![b'-', b'-', b'x', 0xff]);
    let invalid_value = OsString::from_vec(vec![b'/', b't', b'm', b'p', b'/', 0xff]);
    let args = vec![
      OsString::from("--time"),
      invalid,
      OsString::from("--config"),
      invalid_value,
      OsString::from("--mock"),
    ];
    let (matches, warnings) = parse_options_ignoring_invalid(&Greeter::options(), &args);

    assert!(matches.opt_present("time"));
    assert!(!matches.opt_present("config"));
    assert!(matches.opt_present("mock"));
    assert_eq!(warnings.len(), 2);
    assert!(warnings.iter().all(|warning| warning.contains("not valid UTF-8")));
  }

  #[tokio::test]
  async fn test_session_paths_are_deduplicated() {
    let mut greeter = Greeter::default();

    greeter
      .parse_options(&[
        "--sessions",
        "/sessions:/sessions",
        "--xsessions",
        "/sessions:/sessions",
      ])
      .await
      .unwrap();

    assert_eq!(greeter.session_paths.len(), 2);
    assert_eq!(
      greeter.session_paths[0],
      (PathBuf::from("/sessions"), SessionType::Wayland)
    );
    assert_eq!(greeter.session_paths[1], (PathBuf::from("/sessions"), SessionType::X11));
  }

  #[test]
  fn test_mock_sessions() {
    let sessions = mock_sessions();

    assert_eq!(sessions.len(), 3);
    assert!(
      sessions
        .iter()
        .all(|session| session.command == "true" && session.path.is_none())
    );
  }

  #[test]
  fn sole_menu_user_is_preselected() {
    let mut greeter = Greeter::default();
    greeter.user_menu = true;
    greeter.users = Menu {
      title: String::new(),
      options: vec![User {
        username: "origuchi".into(),
        name: Some("Origuchi".into()),
      }],
      selected: 0,
    };

    greeter.select_only_user();

    assert_eq!(greeter.username.value, "origuchi");
    assert_eq!(greeter.username.mask.as_deref(), Some("Origuchi"));
  }

  #[test]
  fn multiple_menu_users_are_not_preselected() {
    let mut greeter = Greeter::default();
    greeter.user_menu = true;
    greeter.users = Menu {
      title: String::new(),
      options: vec![
        User {
          username: "one".into(),
          name: None,
        },
        User {
          username: "two".into(),
          name: None,
        },
      ],
      selected: 0,
    };

    greeter.select_only_user();

    assert!(greeter.username.value.is_empty());
  }

  #[tokio::test]
  async fn test_command_line_arguments() {
    type Case<'a> = (&'a [&'a str], bool, Option<fn(&Greeter)>);

    let table: &[Case<'_>] = &[
      // No arguments
      (&[], true, None),
      // Valid combinations
      (&["--cmd", "hello"], true, None),
      (
        &["--allow-command-editor"],
        true,
        Some(|greeter| assert!(greeter.allow_command_editor)),
      ),
      (
        &[
          "--time",
          "--power-suspend",
          "systemctl suspend",
          "--future-option=value",
          "--cmd",
          "hello",
        ],
        true,
        Some(|greeter| {
          assert!(greeter.config().opt_present("time"));
          assert!(matches!(&greeter.session_source, SessionSource::DefaultCommand(cmd, None) if cmd == "hello"));
        }),
      ),
      (&["-z", "--remember"], true, Some(|greeter| assert!(greeter.remember))),
      (
        &[
          "--cmd",
          "uname",
          "--env",
          "A=B",
          "--env",
          "C=D=E",
          "--asterisks",
          "--asterisks-char",
          ".",
          "--issue",
          "--time",
          "--prompt-padding",
          "0",
          "--window-padding",
          "1",
          "--container-padding",
          "12",
          "--user-menu",
        ],
        true,
        Some(|greeter| {
          assert!(
            matches!(&greeter.session_source, SessionSource::DefaultCommand(cmd, Some(env)) if cmd == "uname" && env.len() == 2)
          );

          if let SessionSource::DefaultCommand(_, Some(env)) = &greeter.session_source {
            assert_eq!(env[0], "A=B");
            assert_eq!(env[1], "C=D=E");
          }

          assert!(matches!(&greeter.secret_display, SecretDisplay::Character(c) if c == "."));
          assert_eq!(greeter.prompt_padding(), 0);
          assert_eq!(greeter.window_padding(), 1);
          assert_eq!(greeter.container_padding(), 13);
          assert!(greeter.user_menu);
          assert!(matches!(
            greeter.xsession_wrapper.as_deref(),
            Some("startx /usr/bin/env")
          ));
        }),
      ),
      (
        &["--xsession-wrapper", "mywrapper.sh"],
        true,
        Some(|greeter| {
          assert!(matches!(greeter.xsession_wrapper.as_deref(), Some("mywrapper.sh")));
        }),
      ),
      (
        &["--no-xsession-wrapper"],
        true,
        Some(|greeter| {
          assert!(greeter.xsession_wrapper.is_none());
        }),
      ),
      (
        &["--power-suspend", "do-suspend", "--power-hibernate", "do-hibernate"],
        true,
        Some(|greeter| {
          assert_eq!(greeter.powers.options[2].command.as_deref(), Some("do-suspend"));
          assert_eq!(greeter.powers.options[3].command.as_deref(), Some("do-hibernate"));
        }),
      ),
      (
        &[
          "--user-menu",
          "--user-menu-min-uid",
          "70000",
          "--user-menu-max-uid",
          "70000",
        ],
        true,
        None,
      ),
      (&["--mock"], true, Some(|greeter| assert!(greeter.mock))),
      (
        &["--user-autocomplete"],
        true,
        Some(|greeter| assert!(greeter.user_autocomplete)),
      ),
      (
        &["--refresh-rate", "60"],
        true,
        Some(|greeter| assert_eq!(greeter.refresh_rate, 60)),
      ),
      // Unknown options are ignored
      (&["--asterisk-char", ""], true, None),
      (&["--min-uid", "10000", "--max-uid", "5000"], true, None),
      // Invalid values and combinations are ignored without preventing startup.
      (&["--remember-session", "--remember-user-session"], true, None),
      (
        &["--remember-user-session"],
        true,
        Some(|greeter| assert!(greeter.remember)),
      ),
      (&["--issue", "--greeting", "Hello, world!"], true, None),
      (&["--kb-command", "F2", "--kb-sessions", "F2"], true, None),
      (&["--time-format", "%i %"], true, None),
      (&["--refresh-rate", "0"], true, None),
      (&["--refresh-rate", "241"], true, None),
      (&["--refresh-rate", "fast"], true, None),
      (&["--cmd", "cmd", "--env"], true, None),
      (&["--cmd", "cmd", "--env", "A"], true, None),
    ];

    for (opts, valid, check) in table {
      let mut greeter = Greeter::default();

      match valid {
        true => {
          assert!(
            matches!(greeter.parse_options(opts).await, Ok(())),
            "{opts:?} cannot be parsed"
          );

          if let Some(check) = check {
            check(&greeter);
          }
        },
        false => assert!(greeter.parse_options(opts).await.is_err()),
      }
    }
  }
}
