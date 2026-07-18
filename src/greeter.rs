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
use tracing_appender::non_blocking::WorkerGuard;
use zeroize::Zeroize;

use crate::{
  cache::{CacheState, CacheStore, RememberedSelection},
  config::{self, Settings},
  event::DEFAULT_REFRESH_RATE,
  info::{get_issue, get_min_max_uids, get_sessions, get_users, session_paths},
  ipc::AuthState,
  power::{CommandLine, PowerOption},
  text::Text,
  ui::{
    common::{
      masked::MaskedString,
      menu::{Menu, MenuItem},
      style::Theme,
    },
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
  pub config: Option<Arc<Matches>>,
  pub settings: Settings,
  pub socket: String,
  pub ipc_timeout: u16,

  // Current mode of the application, will define what actions are permitted.
  pub mode: Mode,
  // Mode the application will return to when exiting the current mode.
  pub previous_mode: Mode,
  // Absolute UTF-8 byte positions, always normalized to grapheme boundaries.
  pub(crate) username_cursor: usize,
  pub(crate) response_cursor: usize,
  pub(crate) command_cursor: usize,

  // Authentication responses and the optional command editor must never
  // share storage: the latter is available before authentication and is a
  // transient modal screen.
  pub buffer: String,
  pub command_buffer: String,

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
  // Last-known-good remembered state and the persistence target backing it.
  pub(crate) cache_state: CacheState,
  pub(crate) cache_store: CacheStore,

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
  // Non-secret, transient feedback from the local input editor.
  pub(crate) input_warning: Option<String>,
  // Non-sensitive summary of the most recent configuration reload.
  pub(crate) config_notice: Option<String>,

  // Menu for power options.
  pub powers: Menu<Power>,
  // Whether to prefix the power commands with `setsid`.
  pub power_setsid: bool,

  // Run without greetd and simulate authentication for visual testing.
  pub mock: bool,

  pub kb_command: u8,
  pub kb_sessions: u8,
  pub kb_power: u8,

  // Explicit state of the current greetd authentication/session transaction.
  pub(crate) auth_state: AuthState,
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
      ipc_timeout: config::DEFAULT_IPC_TIMEOUT,
      mode: Mode::default(),
      previous_mode: Mode::default(),
      username_cursor: 0,
      response_cursor: 0,
      command_cursor: 0,
      buffer: String::new(),
      command_buffer: String::new(),
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
      cache_state: CacheState::default(),
      cache_store: CacheStore::disabled(),
      theme: Theme::default(),
      time: false,
      time_format: None,
      refresh_rate: DEFAULT_REFRESH_RATE,
      greeting: None,
      message: None,
      input_warning: None,
      config_notice: None,
      powers: Menu::default(),
      power_setsid: false,
      mock: false,
      kb_command: 2,
      kb_sessions: 3,
      kb_power: 12,
      auth_state: AuthState::Idle,
      exit: None,
    }
  }
}

impl Drop for Greeter {
  fn drop(&mut self) {
    self.scrub(true, false);
  }
}

#[derive(Clone)]
pub(crate) struct ReloadSnapshot {
  settings: Settings,
  text: Text,
  debug: bool,
  logfile: String,
  mock: bool,
}

pub(crate) struct ReloadPlan {
  settings: Settings,
  users: Option<Vec<User>>,
  sessions: Option<DiscoveredSessions>,
  greeting: Option<Option<String>>,
  powers: Option<Vec<Power>>,
  warnings: Vec<String>,
}

pub(crate) struct DiscoveredSessions {
  paths: Vec<(PathBuf, SessionType)>,
  options: Vec<Session>,
}

pub(crate) struct ReloadApplied {
  pub refresh_rate: u16,
  pub warnings: Vec<String>,
  #[cfg_attr(test, allow(dead_code))]
  pub clear_command_cache: bool,
}

impl ReloadPlan {
  pub(crate) fn prepare(snapshot: ReloadSnapshot, mut settings: Settings) -> Self {
    let mut warnings = Vec::new();

    if settings.debug != snapshot.debug || settings.logfile != snapshot.logfile {
      warnings.push("general.debug and general.log-file require a restart; keeping their current values".into());
      settings.debug = snapshot.debug;
      settings.logfile.clone_from(&snapshot.logfile);
    }
    if settings.mock != snapshot.mock {
      warnings.push("general.mock requires a restart; keeping its current value".into());
      settings.mock = snapshot.mock;
    }

    let users_changed = settings.user_menu != snapshot.settings.user_menu
      || settings.user_autocomplete != snapshot.settings.user_autocomplete
      || settings.min_uid != snapshot.settings.min_uid
      || settings.max_uid != snapshot.settings.max_uid;
    let users = users_changed.then(|| {
      if settings.user_menu || settings.user_autocomplete {
        let (min_uid, max_uid) = get_min_max_uids(settings.min_uid, settings.max_uid);
        tracing::info!("min/max UIDs are {}/{}", min_uid, max_uid);
        get_users(min_uid, max_uid)
      } else {
        Vec::new()
      }
    });

    let session_paths_changed =
      settings.sessions != snapshot.settings.sessions || settings.xsessions != snapshot.settings.xsessions;
    let sessions = session_paths_changed.then(|| {
      let paths = session_paths(&settings.sessions, &settings.xsessions);
      let mut sessions = get_sessions(&paths).unwrap_or_default();
      if settings.mock && sessions.is_empty() {
        sessions = mock_sessions();
      }
      DiscoveredSessions {
        paths,
        options: sessions,
      }
    });

    let greeting_changed = settings.issue != snapshot.settings.issue || settings.greeting != snapshot.settings.greeting;
    let greeting = greeting_changed.then(|| {
      if settings.issue {
        get_issue()
      } else {
        settings.greeting.clone()
      }
    });

    let power_changed = settings.power_shutdown != snapshot.settings.power_shutdown
      || settings.power_reboot != snapshot.settings.power_reboot
      || settings.power_suspend != snapshot.settings.power_suspend
      || settings.power_hibernate != snapshot.settings.power_hibernate;
    let powers = power_changed.then(|| build_power_options(&snapshot.text, &settings));

    Self {
      settings,
      users,
      sessions,
      greeting,
      powers,
      warnings,
    }
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

    let paths = greeter.session_paths.clone();
    let mut sessions = tokio::task::spawn_blocking(move || get_sessions(&paths).unwrap_or_default())
      .await
      .unwrap_or_default();

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

    #[cfg(not(test))]
    {
      greeter.cache_store = CacheStore::for_runtime(greeter.mock);
    }

    let store = greeter.cache_store.clone();
    let cache_sessions = greeter.sessions.options.clone();
    let allow_commands = greeter.allow_command_editor;
    let cache_required = greeter.remember || greeter.remember_session || greeter.remember_user_session;
    let cache = if store.is_enabled() {
      tokio::task::spawn_blocking(move || store.load(&cache_sessions, allow_commands, cache_required))
        .await
        .unwrap_or_else(|error| crate::cache::CacheLoad {
          state: CacheState::default(),
          warnings: vec![format!("cache worker failed: {error}")],
        })
    } else {
      crate::cache::CacheLoad::default()
    };
    for warning in cache.warnings {
      eprintln!("tuigreet: warning: {warning}");
      tracing::warn!("{warning}");
    }
    greeter.cache_state = cache.state;

    if greeter.remember
      && let Some(user) = greeter.cache_state.last_user().cloned()
    {
      greeter.username = MaskedString::from(user.username, user.display_name);

      if greeter.remember_user_session
        && let Some(selection) = greeter.cache_state.user_selection(&greeter.username.value).cloned()
      {
        greeter.restore_cached_selection(&selection);
      }
    }

    if greeter.remember_session
      && let Some(selection) = greeter.cache_state.global_selection().cloned()
    {
      greeter.restore_cached_selection(&selection);
    }

    greeter.normalize_derived_state();

    greeter
  }

  pub(crate) fn restore_cached_selection(&mut self, selection: &RememberedSelection) -> bool {
    if let Some(command) = selection.command_value() {
      if !self.allow_command_editor {
        return false;
      }
      self.session_source = SessionSource::Command(command.to_string());
      return true;
    }

    let Some(index) = selection.resolve(&self.sessions.options) else {
      return false;
    };
    self.sessions.selected = index;
    self.session_source = SessionSource::Session(index);
    true
  }

  // Scrub memory of all data, unless `soft` is true, in which case, we will
  // keep the username (can happen if a wrong password was entered, we want to
  // give the user another chance, as PAM would).
  fn scrub(&mut self, scrub_message: bool, soft: bool) {
    self.buffer.zeroize();
    self.command_buffer.zeroize();
    self.prompt.zeroize();
    self.response_cursor = 0;
    self.command_cursor = 0;
    self.input_warning = None;

    if !soft {
      self.username.zeroize();
      self.username_cursor = 0;
    }

    if scrub_message {
      self.message.zeroize();
    }
  }

  // Reset the software to its initial state.
  pub async fn reset(&mut self, soft: bool) {
    self.reset_local(soft);
  }

  pub(crate) fn reset_local(&mut self, soft: bool) {
    if soft {
      self.mode = Mode::Password;
      self.previous_mode = Mode::Password;
    } else {
      self.mode = Mode::Username;
      self.previous_mode = Mode::Username;
    }

    self.auth_state = AuthState::Idle;

    self.scrub(false, soft);
  }

  pub fn open_command_editor(&mut self) {
    self.previous_mode = match self.mode {
      Mode::Users | Mode::Command | Mode::Sessions | Mode::Power => self.previous_mode,
      _ => self.mode,
    };

    let command = self
      .session_source
      .command(self)
      .map(str::to_string)
      .unwrap_or_default();
    self.command_buffer.zeroize();
    self.command_buffer = command;
    self.command_cursor = self.command_buffer.len();
    self.input_warning = None;
    self.mode = Mode::Command;
  }

  pub fn close_command_editor(&mut self) {
    self.command_buffer.zeroize();
    self.command_cursor = 0;
    self.input_warning = None;
    if self.mode == Mode::Command {
      self.mode = self.previous_mode;
    }
  }

  pub fn config(&self) -> &Matches {
    self.config.as_deref().unwrap()
  }

  pub(crate) fn config_handle(&self) -> Arc<Matches> {
    self
      .config
      .as_ref()
      .expect("configuration was parsed at startup")
      .clone()
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
    self.settings.container_padding.saturating_add(1)
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
    opts.optopt(
      "",
      "ipc-timeout",
      "maximum seconds to wait for a greetd response (default: 120)",
      "SECONDS",
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
    self.config = Some(Arc::new(matches));

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
    self.ipc_timeout = settings.ipc_timeout;
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
      let min_uid = settings.min_uid;
      let max_uid = settings.max_uid;
      let users = tokio::task::spawn_blocking(move || {
        let (min_uid, max_uid) = get_min_max_uids(min_uid, max_uid);
        tracing::info!("min/max UIDs are {}/{}", min_uid, max_uid);
        get_users(min_uid, max_uid)
      })
      .await
      .unwrap_or_default();

      self.users = Menu {
        title: text!(self, title_users),
        options: users,
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

    self.session_paths = session_paths(&settings.sessions, &settings.xsessions);
    self.session_wrapper = settings.session_wrapper.clone();
    self.xsession_wrapper = settings.xsession_wrapper.clone();
    if settings.issue {
      self.greeting = tokio::task::spawn_blocking(get_issue).await.unwrap_or_default();
    }

    self.powers.options = build_power_options(&self.text, &settings);

    self.power_setsid = settings.power_setsid;
    self.mock = settings.mock;
    self.kb_command = settings.kb_command;
    self.kb_sessions = settings.kb_sessions;
    self.kb_power = settings.kb_power;

    Ok(())
  }

  pub(crate) fn reload_snapshot(&self) -> ReloadSnapshot {
    ReloadSnapshot {
      settings: self.settings.clone(),
      text: self.text.clone(),
      debug: self.debug,
      logfile: self.logfile.clone(),
      mock: self.mock,
    }
  }

  pub(crate) fn apply_reload(&mut self, plan: ReloadPlan) -> ReloadApplied {
    let ReloadPlan {
      settings,
      users,
      sessions,
      greeting,
      powers,
      warnings,
    } = plan;
    let old_settings = self.settings.clone();
    let highlighted_user = self
      .users
      .options
      .get(self.users.selected)
      .map(|user| user.username.clone());
    let highlighted_session = self.sessions.options.get(self.sessions.selected).and_then(Session::id);
    let active_session = match self.session_source {
      SessionSource::Session(index) => self.sessions.options.get(index).and_then(Session::id),
      _ => None,
    };
    let highlighted_power = self.powers.options.get(self.powers.selected).map(|power| power.action);
    let command_editor_disabled = self.allow_command_editor && !settings.allow_command_editor;

    self.theme = Theme::parse(&settings.theme.specification());
    self.secret_display = if settings.asterisks {
      SecretDisplay::Character(settings.asterisks_chars.clone())
    } else {
      SecretDisplay::Hidden
    };
    self.time = settings.time;
    self.time_format.clone_from(&settings.time_format);
    self.refresh_rate = settings.refresh_rate;
    self.ipc_timeout = settings.ipc_timeout;
    self.user_menu = settings.user_menu;
    self.user_autocomplete = settings.user_autocomplete;

    if let Some(users) = users {
      self.users.options = users;
      self.users.selected = highlighted_user
        .as_deref()
        .and_then(|username| self.users.options.iter().position(|user| user.username == username))
        .or_else(|| {
          (!self.username.value.is_empty())
            .then(|| {
              self
                .users
                .options
                .iter()
                .position(|user| user.username == self.username.value)
            })
            .flatten()
        })
        .unwrap_or(0);
    }

    self.remember = settings.remember;
    self.remember_session = settings.remember_session;
    self.remember_user_session = settings.remember_user_session;
    self.allow_command_editor = settings.allow_command_editor;
    if command_editor_disabled {
      self.cache_state.purge_commands();
    }
    if !self.allow_command_editor && self.mode == Mode::Command {
      self.close_command_editor();
    }

    if let Some(greeting) = greeting {
      self.greeting = greeting;
    }

    if let Some(discovered) = sessions {
      self.session_paths = discovered.paths;
      self.sessions.options = discovered.options;
      self.sessions.selected = highlighted_session
        .as_ref()
        .and_then(|id| {
          self
            .sessions
            .options
            .iter()
            .position(|session| session.id().as_ref() == Some(id))
        })
        .unwrap_or(0);

      if matches!(self.session_source, SessionSource::Session(_)) {
        self.session_source = active_session
          .as_ref()
          .and_then(|id| {
            self
              .sessions
              .options
              .iter()
              .position(|session| session.id().as_ref() == Some(id))
          })
          .map(SessionSource::Session)
          .unwrap_or_else(|| self.fallback_session_source(&settings));
      }
    }

    let command_changed = settings.command != old_settings.command;
    let environment_changed = settings.environment != old_settings.environment;
    if command_changed {
      if let Some(command) = settings.command.clone() {
        self.session_source = SessionSource::DefaultCommand(command, configured_environment(&settings));
      } else if matches!(self.session_source, SessionSource::DefaultCommand(_, _)) {
        self.session_source = self.fallback_session_source(&settings);
      }
    } else if environment_changed && let SessionSource::DefaultCommand(command, _) = &self.session_source {
      self.session_source = SessionSource::DefaultCommand(command.clone(), configured_environment(&settings));
    }

    if !self.allow_command_editor && matches!(self.session_source, SessionSource::Command(_)) {
      self.session_source = self.fallback_session_source(&settings);
    }

    if let Some(powers) = powers {
      self.powers.options = powers;
      self.powers.selected = highlighted_power
        .and_then(|action| self.powers.options.iter().position(|power| power.action == action))
        .unwrap_or(0);
    }

    self.session_wrapper.clone_from(&settings.session_wrapper);
    self.xsession_wrapper.clone_from(&settings.xsession_wrapper);
    self.power_setsid = settings.power_setsid;
    self.kb_command = settings.kb_command;
    self.kb_sessions = settings.kb_sessions;
    self.kb_power = settings.kb_power;
    self.settings = settings;
    self.normalize_derived_state();

    ReloadApplied {
      refresh_rate: self.refresh_rate,
      warnings,
      clear_command_cache: command_editor_disabled,
    }
  }

  fn fallback_session_source(&self, settings: &Settings) -> SessionSource {
    settings.command.clone().map_or_else(
      || {
        if self.sessions.options.is_empty() {
          SessionSource::None
        } else {
          SessionSource::Session(0)
        }
      },
      |command| SessionSource::DefaultCommand(command, configured_environment(settings)),
    )
  }

  fn normalize_derived_state(&mut self) {
    clamp_menu(&mut self.users);
    clamp_menu(&mut self.sessions);
    clamp_menu(&mut self.powers);

    if matches!(self.session_source, SessionSource::Session(index) if index >= self.sessions.options.len()) {
      self.session_source = self.fallback_session_source(&self.settings);
    }
    if matches!(self.session_source, SessionSource::None) {
      self.session_source = self.fallback_session_source(&self.settings);
    }

    let invalid_modal = match self.mode {
      Mode::Users => !self.user_menu || self.users.options.is_empty(),
      Mode::Sessions => self.sessions.options.is_empty(),
      Mode::Power => self.powers.options.is_empty(),
      Mode::Command => !self.allow_command_editor,
      _ => false,
    };
    if invalid_modal {
      self.mode = self.safe_previous_mode();
    }

    self.select_only_user();
  }

  fn safe_previous_mode(&self) -> Mode {
    match self.previous_mode {
      Mode::Username | Mode::Password | Mode::Action => self.previous_mode,
      _ => {
        if matches!(self.auth_state, AuthState::AwaitingInput(_)) {
          Mode::Password
        } else {
          Mode::Username
        }
      },
    }
  }

  pub fn set_prompt(&mut self, prompt: &str) {
    self.prompt = if prompt.ends_with(' ') {
      Some(prompt.into())
    } else {
      Some(format!("{prompt} "))
    };
    self.response_cursor = self.buffer.len();
    self.input_warning = None;
  }

  fn select_only_user(&mut self) {
    if self.username.value.is_empty()
      && self.user_menu
      && self.mode == Mode::Username
      && self.auth_state == AuthState::Idle
      && let [user] = self.users.options.as_slice()
    {
      self.username = MaskedString::from(user.username.clone(), user.name.clone());
      self.username_cursor = self.username.value.len();
    }
  }

  pub fn remove_prompt(&mut self) {
    self.prompt = None;
  }

  // Computes the size of the prompt to help determine where input should start.
  pub fn prompt_width(&self) -> usize {
    match &self.prompt {
      None => 0,
      Some(prompt) => crate::ui::input::width(prompt),
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

fn configured_environment(settings: &Settings) -> Option<Vec<String>> {
  (!settings.environment.is_empty()).then(|| settings.environment.clone())
}

fn build_power_options(text: &Text, settings: &Settings) -> Vec<Power> {
  build_power_options_with_default(text, settings, crate::power::default_command)
}

fn build_power_options_with_default(
  text: &Text,
  settings: &Settings,
  default: impl Fn(PowerOption) -> Option<CommandLine> + Copy,
) -> Vec<Power> {
  [
    (
      PowerOption::Shutdown,
      text.shutdown.clone(),
      settings.power_shutdown.clone(),
    ),
    (PowerOption::Reboot, text.reboot.clone(), settings.power_reboot.clone()),
    (
      PowerOption::Suspend,
      text.suspend.clone(),
      settings.power_suspend.clone(),
    ),
    (
      PowerOption::Hibernate,
      text.hibernate.clone(),
      settings.power_hibernate.clone(),
    ),
  ]
  .into_iter()
  .filter_map(|(action, label, command)| {
    command.resolve_with(action, default).map(|command| Power {
      action,
      label,
      command: Some(command),
    })
  })
  .collect()
}

fn clamp_menu<T: MenuItem>(menu: &mut Menu<T>) {
  if menu.options.is_empty() {
    menu.selected = 0;
  } else {
    menu.selected = menu.selected.min(menu.options.len() - 1);
  }
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
  println!("{}", version_information());
}

fn version_information() -> String {
  format!(
    "tuigreet (tuigreety) {}\n\
     Target: {}\n\
     Copyright (C) 2026 Tobiichi-Origuchi <https://github.com/Tobiichi-Origuchi/tuigreety>.\n\
     Copyright (C) 2020 Antoine POPINEAU <https://github.com/apognu/tuigreet>.\n\
     License GPLv3+: GNU GPL version 3 or later <https://gnu.org/licenses/gpl.html>\n\
     \n\
     This is free software: you are free to change and redistribute it.\n\
     There is NO WARRANTY, to the extent permitted by law.",
    env!("VERSION"),
    env!("TARGET")
  )
}

#[cfg(test)]
mod test {
  use std::{ffi::OsString, os::unix::ffi::OsStringExt, path::PathBuf};

  use super::{
    DiscoveredSessions,
    ReloadPlan,
    build_power_options_with_default,
    mock_sessions,
    parse_options_ignoring_invalid,
    print_information,
    version_information,
  };
  use crate::{
    Greeter,
    Mode,
    SecretDisplay,
    config::Settings,
    power::{CommandLine, PowerCommand, PowerOption, default_command},
    text::Text,
    ui::{
      common::menu::Menu,
      sessions::{Session, SessionSource, SessionType},
      users::User,
    },
  };

  fn reload_plan(settings: crate::config::Settings) -> ReloadPlan {
    ReloadPlan {
      settings,
      users: None,
      sessions: None,
      greeting: None,
      powers: None,
      warnings: Vec::new(),
    }
  }

  fn defaults_without_sleep(option: PowerOption) -> Option<CommandLine> {
    match option {
      PowerOption::Suspend | PowerOption::Hibernate => None,
      _ => default_command(option),
    }
  }

  #[test]
  fn unavailable_and_disabled_power_actions_are_omitted() {
    let mut settings = Settings::default();
    let text = Text::default();

    let automatic = build_power_options_with_default(&text, &settings, defaults_without_sleep);
    assert_eq!(automatic.iter().map(|power| power.action).collect::<Vec<_>>(), [
      PowerOption::Shutdown,
      PowerOption::Reboot
    ]);

    settings.power_suspend = PowerCommand::Explicit(CommandLine::parse("custom-suspend").unwrap());
    settings.power_hibernate = PowerCommand::Disabled;
    let custom = build_power_options_with_default(&text, &settings, defaults_without_sleep);
    assert_eq!(custom.iter().map(|power| power.action).collect::<Vec<_>>(), [
      PowerOption::Shutdown,
      PowerOption::Reboot,
      PowerOption::Suspend
    ]);

    settings.power_shutdown = PowerCommand::Disabled;
    settings.power_reboot = PowerCommand::Disabled;
    settings.power_suspend = PowerCommand::Disabled;
    assert!(build_power_options_with_default(&text, &settings, defaults_without_sleep).is_empty());
  }

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
    greeter.buffer = "password buffer".into();
    greeter.command_buffer = "untrusted command".into();
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

    let plan = ReloadPlan::prepare(greeter.reload_snapshot(), settings);
    let applied = greeter.apply_reload(plan);

    assert_eq!(applied.warnings.len(), 2);
    assert!(greeter.debug);
    assert_eq!(greeter.logfile, "/tmp/original.log");
    assert!(greeter.mock);
    assert!(!greeter.allow_command_editor);
    assert_eq!(greeter.mode, Mode::Username);
    assert_eq!(greeter.buffer, "password buffer");
    assert!(greeter.command_buffer.is_empty());
    assert!(greeter.time);
    assert_eq!(greeter.refresh_rate, 60);
    assert!(matches!(greeter.secret_display, SecretDisplay::Character(ref value) if value == "#"));
    assert_eq!(greeter.kb_command, 5);
  }

  #[test]
  fn unrelated_reload_preserves_the_runtime_session_choice() {
    let mut greeter = Greeter::default();
    greeter.settings.command = Some("configured-default".into());
    greeter.sessions.options = vec![
      Session {
        name: "first".into(),
        slug: Some("first".into()),
        ..Default::default()
      },
      Session {
        name: "chosen".into(),
        slug: Some("chosen".into()),
        ..Default::default()
      },
    ];
    greeter.sessions.selected = 1;
    greeter.session_source = SessionSource::Session(1);
    let mut settings = greeter.settings.clone();
    settings.time = true;

    greeter.apply_reload(reload_plan(settings));

    assert!(matches!(greeter.session_source, SessionSource::Session(1)));
  }

  #[test]
  fn changed_configured_command_resets_but_environment_only_does_not_override_a_session() {
    let mut greeter = Greeter::default();
    greeter.settings.command = Some("old-default".into());
    greeter.sessions.options = vec![Session {
      name: "chosen".into(),
      slug: Some("chosen".into()),
      ..Default::default()
    }];
    greeter.session_source = SessionSource::Session(0);

    let mut settings = greeter.settings.clone();
    settings.environment = vec!["A=B".into()];
    greeter.apply_reload(reload_plan(settings.clone()));
    assert!(matches!(greeter.session_source, SessionSource::Session(0)));

    settings.command = Some("new-default".into());
    greeter.apply_reload(reload_plan(settings));
    assert!(matches!(
      greeter.session_source,
      SessionSource::DefaultCommand(ref command, Some(ref environment))
        if command == "new-default" && environment == &["A=B"]
    ));
  }

  #[test]
  fn reload_normalizes_user_modes_and_selects_a_new_sole_user() {
    let mut greeter = Greeter::default();
    let mut settings = greeter.settings.clone();
    settings.user_menu = true;
    let mut plan = reload_plan(settings.clone());
    plan.users = Some(vec![User {
      username: "only-user".into(),
      name: Some("Only User".into()),
    }]);

    greeter.apply_reload(plan);
    assert_eq!(greeter.username.value, "only-user");
    assert_eq!(greeter.username.mask.as_deref(), Some("Only User"));

    greeter.mode = Mode::Users;
    greeter.previous_mode = Mode::Username;
    settings.user_menu = false;
    settings.user_autocomplete = true;
    let mut plan = reload_plan(settings);
    plan.users = Some(vec![User {
      username: "only-user".into(),
      name: None,
    }]);
    greeter.apply_reload(plan);

    assert_eq!(greeter.mode, Mode::Username);
    assert_eq!(greeter.users.selected, 0);
  }

  #[test]
  fn reload_preserves_synthetic_sessions_and_closes_an_empty_menu() {
    let mut greeter = Greeter::default();
    greeter.mock = true;
    greeter.settings.mock = true;
    greeter.sessions.options = mock_sessions();
    greeter.sessions.selected = 1;
    greeter.session_source = SessionSource::Session(1);

    let settings = greeter.settings.clone();
    let mut plan = reload_plan(settings.clone());
    let mut reordered = mock_sessions();
    reordered.swap(0, 1);
    plan.sessions = Some(DiscoveredSessions {
      paths: Vec::new(),
      options: reordered,
    });
    greeter.apply_reload(plan);
    assert!(matches!(greeter.session_source, SessionSource::Session(0)));
    assert_eq!(greeter.sessions.options[0].slug.as_deref(), Some("mock-x11"));

    greeter.mode = Mode::Sessions;
    greeter.previous_mode = Mode::Username;
    let mut plan = reload_plan(settings);
    plan.sessions = Some(DiscoveredSessions {
      paths: Vec::new(),
      options: Vec::new(),
    });
    greeter.apply_reload(plan);
    assert_eq!(greeter.mode, Mode::Username);
    assert!(matches!(greeter.session_source, SessionSource::None));
    assert_eq!(greeter.sessions.selected, 0);
  }

  #[test]
  fn test_information_options() {
    assert!(print_information(&["--help"]));
    assert!(print_information(&["-v"]));
    assert!(!print_information(&["--time"]));
  }

  #[test]
  fn version_identifies_the_derivative_and_both_copyright_holders() {
    let information = version_information();

    assert!(information.starts_with(&format!("tuigreet (tuigreety) {}\n", env!("VERSION"))));
    assert!(information.contains("Copyright (C) 2026 Tobiichi-Origuchi"));
    assert!(information.contains("Copyright (C) 2020 Antoine POPINEAU"));
    assert!(information.contains("License GPLv3+: GNU GPL version 3 or later"));
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
          assert_eq!(greeter.powers.options[2].command.as_ref().unwrap().argv(), [
            "do-suspend"
          ]);
          assert_eq!(greeter.powers.options[3].command.as_ref().unwrap().argv(), [
            "do-hibernate"
          ]);
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
      (
        &["--ipc-timeout", "60"],
        true,
        Some(|greeter| assert_eq!(greeter.ipc_timeout, 60)),
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
      (&["--ipc-timeout", "0"], true, None),
      (&["--ipc-timeout", "3601"], true, None),
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
