use std::{
  env,
  error::Error,
  ffi::OsStr,
  fmt::{self, Display},
  path::PathBuf,
  process,
  sync::Arc,
};

use getopts::{Fail, Matches, Options};
use tokio::{
  net::UnixStream,
  sync::{RwLock, RwLockWriteGuard, mpsc::Sender},
};
use tracing_appender::non_blocking::WorkerGuard;
use zeroize::Zeroize;

use crate::{
  config::{self, Settings},
  event::{DEFAULT_REFRESH_RATE, Event},
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

#[derive(Debug, Copy, Clone)]
pub enum AuthStatus {
  Success,
  Failure,
  Cancel,
}

impl Display for AuthStatus {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    write!(f, "{:?}", self)
  }
}

impl Error for AuthStatus {}

// A mode represents the large section of the software, usually screens to be
// displayed, or the state of the application.
#[derive(SmartDefault, Debug, Copy, Clone, PartialEq)]
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
#[derive(SmartDefault, Debug, Clone)]
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
#[derive(SmartDefault, Debug, Clone)]
pub enum GreetAlign {
  #[default]
  Center,
  Left,
  Right,
}

#[derive(SmartDefault)]
pub struct Greeter {
  pub debug: bool,
  pub logfile: String,
  pub logger: Option<WorkerGuard>,

  pub text: Text,
  pub config: Option<Matches>,
  pub settings: Settings,
  pub socket: String,
  pub stream: Option<Arc<RwLock<UnixStream>>>,
  pub events: Option<Sender<Event>>,

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
  #[default(DEFAULT_REFRESH_RATE)]
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

  #[default(2)]
  pub kb_command: u8,
  #[default(3)]
  pub kb_sessions: u8,
  #[default(12)]
  pub kb_power: u8,

  // The software is waiting for a response from `greetd`.
  pub working: bool,
  // We are done working.
  pub done: bool,
  // Should we exit?
  pub exit: Option<AuthStatus>,
}

impl Drop for Greeter {
  fn drop(&mut self) {
    self.scrub(true, false);
  }
}

impl Greeter {
  pub async fn new(events: Sender<Event>) -> Self {
    let mut greeter = Self::default();

    greeter.events = Some(events);
    greeter.powers = Menu {
      title: text!(greeter, title_power),
      options: Default::default(),
      selected: 0,
    };

    #[cfg(not(test))]
    {
      let args = env::args().collect::<Vec<String>>();

      if let Err(err) = greeter.parse_options(&args).await {
        eprintln!("{err}");
        print_usage(Greeter::options());

        process::exit(1);
      }

      greeter.connect().await;
    }

    greeter.logger = crate::init_logger(&greeter);

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

    // If we should remember the last logged-in user.
    if greeter.remember
      && let Some(username) = get_last_user_username()
    {
      greeter.username = MaskedString::from(username, get_last_user_name());

      // If, on top of that, we should remember their last session.
      if greeter.remember_user_session {
        // See if we have the last free-form command from the user.
        if let Ok(command) = get_last_user_command(greeter.username.get()) {
          greeter.session_source = SessionSource::Command(command);
        }

        // If a session was saved, use it and its name.
        if let Ok(ref session_path) = get_last_user_session(greeter.username.get()) {
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
      if let Ok(command) = get_last_command() {
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

  pub fn options() -> Options {
    let mut opts = Options::new();

    let xsession_wrapper_desc =
      format!("wrapper command to initialize X server and launch X11 sessions (default: {DEFAULT_XSESSION_WRAPPER})");

    opts.optflag("h", "help", "show this usage information");
    opts.optflag("v", "version", "print version information");
    opts.optopt("", "config", "load an explicit TOML configuration file", "FILE");
    opts.optflagopt(
      "d",
      "debug",
      "enable debug logging to the provided file, or to /tmp/tuigreet.log",
      "FILE",
    );
    opts.optopt("c", "cmd", "command to run", "COMMAND");
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
    opts.optflag(
      "",
      "text-config",
      "load text overrides from the system configuration file",
    );
    opts.optopt(
      "",
      "text-config-file",
      "load text overrides from an explicit file",
      "FILE",
    );
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

    if settings.text_config
      && let Err(error) = self.text.load_standard()
    {
      eprintln!("tuigreet: warning: cannot load standard text configuration: {error}");
    }
    if let Some(path) = &settings.text_config_file
      && let Err(error) = self.text.load_file(path)
    {
      eprintln!(
        "tuigreet: warning: {}: cannot load text configuration: {error}",
        path.display()
      );
    }
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

fn print_usage(opts: Options) {
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

fn parse_options_ignoring_invalid<S>(opts: &Options, args: &[S]) -> (Matches, Vec<String>)
where
  S: AsRef<OsStr>,
{
  let mut args: Vec<&OsStr> = args.iter().map(AsRef::as_ref).collect();
  let mut warnings = Vec::new();

  loop {
    match opts.parse(&args) {
      Ok(matches) => {
        for argument in &matches.free {
          warnings.push(format!("unexpected positional argument '{argument}'; ignoring it"));
        }
        return (matches, warnings);
      },
      Err(error) => {
        let name = match &error {
          Fail::ArgumentMissing(name)
          | Fail::UnrecognizedOption(name)
          | Fail::OptionDuplicated(name)
          | Fail::OptionMissing(name)
          | Fail::UnexpectedArgument(name) => name,
        };
        let index = args.iter().rposition(|arg| option_has_name(arg, name)).unwrap_or(0);
        warnings.push(format!("{error}; ignoring {}", args[index].to_string_lossy()));
        args.remove(index);
      },
    }
  }
}

fn option_has_name(arg: &OsStr, name: &str) -> bool {
  let Some(arg) = arg.to_str() else {
    return false;
  };

  if let Some(long) = arg.strip_prefix("--") {
    return long.split_once('=').map_or(long, |(name, _)| name) == name;
  }

  name.chars().count() == 1
    && arg
      .strip_prefix('-')
      .is_some_and(|shorts| shorts.chars().any(|short| name.starts_with(short)))
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
  use std::path::PathBuf;

  use super::{mock_sessions, print_information};
  use crate::{
    Greeter,
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
  fn test_information_options() {
    assert!(print_information(&["tuigreet", "--help"]));
    assert!(print_information(&["tuigreet", "-v"]));
    assert!(!print_information(&["tuigreet", "--time"]));
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

  #[tokio::test]
  async fn explicit_text_config_overrides_defaults() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("text.conf");
    std::fs::write(&path, "username=Login:\ntitle_power=Actions\n").unwrap();
    let mut greeter = Greeter::default();

    greeter
      .parse_options(&["--text-config-file".as_ref(), path.as_os_str()])
      .await
      .unwrap();

    assert_eq!(greeter.text.username, "Login:");
    assert_eq!(greeter.powers.title, "Actions");
    assert_eq!(greeter.text.reboot, "Reboot");
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
            "{:?} cannot be parsed",
            opts
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
