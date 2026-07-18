use std::{
  env,
  fs,
  io::{self, Write},
  ops::Range,
  path::{Path, PathBuf},
  str::FromStr,
};

use getopts::Matches;
use ratatui::style::Color;
use toml_edit::{Document, Item, Table};

use crate::{
  event::{DEFAULT_REFRESH_RATE, MAX_REFRESH_RATE},
  power::{CommandLine, PowerCommand},
};

pub const SYSTEM_CONFIG: &str = "/etc/tuigreet/config.toml";
pub const DEFAULT_IPC_TIMEOUT: u16 = 120;
pub const MAX_IPC_TIMEOUT: u16 = 3600;
const DEFAULT_LOG_FILE: &str = "/tmp/tuigreet.log";
const DEFAULT_XSESSION_WRAPPER: &str = "startx /usr/bin/env";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Settings {
  pub debug: bool,
  pub logfile: String,
  pub ipc_timeout: u16,
  pub command: Option<String>,
  pub allow_command_editor: bool,
  pub environment: Vec<String>,
  pub sessions: Vec<String>,
  pub xsessions: Vec<String>,
  pub session_wrapper: Option<String>,
  pub xsession_wrapper: Option<String>,
  pub width: u16,
  pub issue: bool,
  pub greeting: Option<String>,
  pub time: bool,
  pub time_format: Option<String>,
  pub refresh_rate: u16,
  pub remember: bool,
  pub remember_session: bool,
  pub remember_user_session: bool,
  pub user_menu: bool,
  pub user_autocomplete: bool,
  pub min_uid: Option<u32>,
  pub max_uid: Option<u32>,
  pub theme: ThemeSettings,
  pub asterisks: bool,
  pub asterisks_chars: String,
  pub window_padding: u16,
  pub container_padding: u16,
  pub prompt_padding: u16,
  pub greet_align: String,
  pub power_shutdown: PowerCommand,
  pub power_reboot: PowerCommand,
  pub power_suspend: PowerCommand,
  pub power_hibernate: PowerCommand,
  pub power_setsid: bool,
  pub mock: bool,
  pub kb_command: u8,
  pub kb_sessions: u8,
  pub kb_power: u8,
}

impl Default for Settings {
  fn default() -> Self {
    Self {
      debug: false,
      logfile: DEFAULT_LOG_FILE.into(),
      ipc_timeout: DEFAULT_IPC_TIMEOUT,
      command: None,
      allow_command_editor: false,
      environment: Vec::new(),
      sessions: Vec::new(),
      xsessions: Vec::new(),
      session_wrapper: None,
      xsession_wrapper: Some(DEFAULT_XSESSION_WRAPPER.into()),
      width: 80,
      issue: false,
      greeting: None,
      time: false,
      time_format: None,
      refresh_rate: DEFAULT_REFRESH_RATE,
      remember: false,
      remember_session: false,
      remember_user_session: false,
      user_menu: false,
      user_autocomplete: false,
      min_uid: None,
      max_uid: None,
      theme: ThemeSettings::default(),
      asterisks: false,
      asterisks_chars: "*".into(),
      window_padding: 0,
      container_padding: 1,
      prompt_padding: 1,
      greet_align: "center".into(),
      power_shutdown: PowerCommand::Auto,
      power_reboot: PowerCommand::Auto,
      power_suspend: PowerCommand::Auto,
      power_hibernate: PowerCommand::Auto,
      power_setsid: true,
      mock: false,
      kb_command: 2,
      kb_sessions: 3,
      kb_power: 12,
    }
  }
}

#[derive(Default)]
struct Layer {
  debug: Option<bool>,
  logfile: Option<String>,
  ipc_timeout: Option<u16>,
  command: Option<Option<String>>,
  allow_command_editor: Option<bool>,
  environment: Option<Vec<String>>,
  sessions: Option<Vec<String>>,
  xsessions: Option<Vec<String>>,
  session_wrapper: Option<Option<String>>,
  xsession_wrapper: Option<Option<String>>,
  width: Option<u16>,
  issue: Option<bool>,
  greeting: Option<Option<String>>,
  time: Option<bool>,
  time_format: Option<Option<String>>,
  refresh_rate: Option<u16>,
  remember: Option<bool>,
  remember_session: Option<bool>,
  remember_user_session: Option<bool>,
  user_menu: Option<bool>,
  user_autocomplete: Option<bool>,
  min_uid: Option<Option<u32>>,
  max_uid: Option<Option<u32>>,
  theme: ThemeLayer,
  asterisks: Option<bool>,
  asterisks_chars: Option<String>,
  window_padding: Option<u16>,
  container_padding: Option<u16>,
  prompt_padding: Option<u16>,
  greet_align: Option<String>,
  power_shutdown: Option<PowerCommand>,
  power_reboot: Option<PowerCommand>,
  power_suspend: Option<PowerCommand>,
  power_hibernate: Option<PowerCommand>,
  power_setsid: Option<bool>,
  mock: Option<bool>,
  kb_command: Option<u8>,
  kb_sessions: Option<u8>,
  kb_power: Option<u8>,
}

#[derive(Default)]
struct ThemeLayer {
  border: Option<Option<String>>,
  text: Option<Option<String>>,
  time: Option<Option<String>>,
  container: Option<Option<String>>,
  title: Option<Option<String>>,
  greet: Option<Option<String>>,
  prompt: Option<Option<String>>,
  input: Option<Option<String>>,
  action: Option<Option<String>>,
  button: Option<Option<String>>,
}

pub(crate) fn load_from(system: Option<&Path>, matches: &Matches) -> (Settings, Vec<String>) {
  let explicit = matches.opt_str("config").map(PathBuf::from);
  load_paths(system, explicit.as_deref(), matches)
}

pub fn reload(matches: &Matches) -> Result<(Settings, Vec<String>), Vec<String>> {
  let explicit = matches.opt_str("config").map(PathBuf::from);
  reload_paths(Path::new(SYSTEM_CONFIG), explicit.as_deref(), matches)
}

fn reload_paths(
  system: &Path,
  explicit: Option<&Path>,
  matches: &Matches,
) -> Result<(Settings, Vec<String>), Vec<String>> {
  let mut settings = Settings::default();
  let mut warnings = Vec::new();

  if !load_optional_strict(system, &mut settings, &mut warnings) {
    return Err(warnings);
  }
  if let Some(path) = explicit
    && !load_required(path, &mut settings, &mut warnings)
  {
    return Err(warnings);
  }

  apply_layer(
    &mut settings,
    cli_layer(matches, &mut warnings),
    "command line",
    &mut warnings,
  );
  Ok((settings, warnings))
}

fn active_paths(matches: &Matches) -> Vec<PathBuf> {
  let mut paths = vec![PathBuf::from(SYSTEM_CONFIG)];
  if let Some(path) = matches.opt_str("config").map(PathBuf::from)
    && path != Path::new(SYSTEM_CONFIG)
  {
    paths.push(path);
  }
  paths
}

#[cfg(not(test))]
pub fn watched_paths(matches: &Matches) -> Vec<PathBuf> {
  active_paths(matches)
}

pub fn check(matches: &Matches) -> bool {
  println!("Configuration files:");
  for (index, path) in active_paths(matches).iter().enumerate() {
    let role = if index == 0 { "system, optional" } else { "explicit" };
    let state = if path.exists() { "found" } else { "not found" };
    println!("  {} ({role}; {state})", path.display());
  }
  let _ = io::stdout().flush();

  match reload(matches) {
    Ok((_, warnings)) if warnings.is_empty() => {
      println!("Configuration is valid.");
      true
    },
    Ok((_, warnings)) | Err(warnings) => {
      for warning in warnings {
        eprintln!("{warning}");
      }
      eprintln!("Configuration is invalid.");
      false
    },
  }
}

fn load_paths(system: Option<&Path>, explicit: Option<&Path>, matches: &Matches) -> (Settings, Vec<String>) {
  let mut settings = Settings::default();
  let mut warnings = Vec::new();

  if let Some(path) = system {
    load_optional(path, &mut settings, &mut warnings);
  }
  if let Some(path) = explicit {
    load_required(path, &mut settings, &mut warnings);
  }

  apply_layer(
    &mut settings,
    cli_layer(matches, &mut warnings),
    "command line",
    &mut warnings,
  );
  (settings, warnings)
}

fn load_optional(path: &Path, settings: &mut Settings, warnings: &mut Vec<String>) {
  match path.try_exists() {
    Ok(true) => {
      load_required(path, settings, warnings);
    },
    Ok(false) => {},
    Err(error) => warnings.push(format!("{}: cannot access configuration: {error}", path.display())),
  }
}

fn load_optional_strict(path: &Path, settings: &mut Settings, warnings: &mut Vec<String>) -> bool {
  match path.try_exists() {
    Ok(true) => load_required(path, settings, warnings),
    Ok(false) => true,
    Err(error) => {
      warnings.push(format!("{}: cannot access configuration: {error}", path.display()));
      false
    },
  }
}

fn load_required(path: &Path, settings: &mut Settings, warnings: &mut Vec<String>) -> bool {
  let content = match fs::read_to_string(path) {
    Ok(content) => content,
    Err(error) => {
      warnings.push(format!("{}: cannot read configuration: {error}", path.display()));
      return false;
    },
  };
  let document = match content.parse::<Document<String>>() {
    Ok(document) => document,
    Err(error) => {
      warnings.push(toml_diagnostic(path, &content, &error));
      return false;
    },
  };

  let layer = toml_layer(&document, path, &content, warnings);
  apply_layer(settings, layer, &path.display().to_string(), warnings);
  true
}

fn toml_diagnostic(path: &Path, source: &str, error: &toml_edit::TomlError) -> String {
  let Some(span) = error.span() else {
    return format!("error: invalid TOML in {}: {}", path.display(), error.message());
  };
  source_diagnostic("error", "invalid TOML", path, source, span, error.message())
}

fn source_diagnostic(level: &str, title: &str, path: &Path, source: &str, span: Range<usize>, message: &str) -> String {
  let offset = span.start.min(source.len());
  let (line, column) = line_column(source, offset);
  let line_start = source[..offset].rfind('\n').map_or(0, |index| index + 1);
  let line_end = source[offset..].find('\n').map_or(source.len(), |index| offset + index);
  let source_line = &source[line_start..line_end];
  let underline_start = source[line_start..offset].chars().count();
  let underline_end = span.end.min(line_end);
  let underline_width = source[offset..underline_end].chars().count().max(1);
  let gutter = line.to_string().len();

  format!(
    "{level}: {title}\n{space:>gutter$}--> {path}:{line}:{column}\n{space:>gutter$} |\n{line:>gutter$} | {source_line}\n{space:>gutter$} | {padding}{carets} {message}",
    space = "",
    path = path.display(),
    padding = " ".repeat(underline_start),
    carets = "^".repeat(underline_width),
  )
}

fn apply_layer(settings: &mut Settings, layer: Layer, source: &str, warnings: &mut Vec<String>) {
  macro_rules! apply {
    ($($field:ident),* $(,)?) => { $(if let Some(value) = layer.$field { settings.$field = value; })* };
  }

  apply!(
    debug,
    logfile,
    ipc_timeout,
    command,
    allow_command_editor,
    environment,
    sessions,
    xsessions,
    session_wrapper,
    xsession_wrapper,
    width,
    time,
    time_format,
    refresh_rate,
    remember,
    user_menu,
    user_autocomplete,
    asterisks,
    asterisks_chars,
    window_padding,
    container_padding,
    prompt_padding,
    greet_align,
    power_shutdown,
    power_reboot,
    power_suspend,
    power_hibernate,
    power_setsid,
    mock,
  );

  macro_rules! apply_theme {
    ($($field:ident),* $(,)?) => {
      $(if let Some(value) = layer.theme.$field { settings.theme.$field = value; })*
    };
  }
  apply_theme!(
    border, text, time, container, title, greet, prompt, input, action, button
  );

  if layer.issue == Some(true) && layer.greeting.as_ref().is_some_and(Option::is_some) {
    warnings.push(format!(
      "{source}: display.issue and display.greeting conflict; using the greeting"
    ));
  }
  if layer.issue == Some(true) {
    settings.greeting = None;
  }
  if let Some(issue) = layer.issue {
    settings.issue = issue;
  }
  if let Some(greeting) = layer.greeting {
    if greeting.is_some() {
      settings.issue = false;
    }
    settings.greeting = greeting;
  }

  let proposed_min = layer.min_uid.unwrap_or(settings.min_uid);
  let proposed_max = layer.max_uid.unwrap_or(settings.max_uid);
  if matches!((proposed_min, proposed_max), (Some(min), Some(max)) if min > max) {
    warnings.push(format!(
      "{source}: users.min-uid exceeds users.max-uid; ignoring both fields from this layer"
    ));
  } else {
    settings.min_uid = proposed_min;
    settings.max_uid = proposed_max;
  }

  let proposed_session = layer.remember_session.unwrap_or(settings.remember_session);
  let proposed_user_session = layer.remember_user_session.unwrap_or(settings.remember_user_session);
  if proposed_session && proposed_user_session {
    warnings.push(format!(
      "{source}: remember.session and remember.user-session cannot both be true; ignoring both fields from this layer"
    ));
  } else {
    settings.remember_session = proposed_session;
    settings.remember_user_session = proposed_user_session;
  }
  if settings.remember_user_session && !settings.remember {
    warnings.push(format!(
      "{source}: remember.user-session requires remember.username; enabling remember.username"
    ));
    settings.remember = true;
  }

  let mut keys = [settings.kb_command, settings.kb_sessions, settings.kb_power];
  for (index, value) in [layer.kb_command, layer.kb_sessions, layer.kb_power]
    .into_iter()
    .enumerate()
  {
    if let Some(value) = value {
      let old = keys[index];
      keys[index] = value;
      if keys
        .iter()
        .enumerate()
        .any(|(other, key)| other != index && *key == value)
      {
        warnings.push(format!(
          "{source}: duplicate keybinding F{value}; ignoring the conflicting field"
        ));
        keys[index] = old;
      }
    }
  }
  [settings.kb_command, settings.kb_sessions, settings.kb_power] = keys;
}

fn cli_layer(matches: &Matches, warnings: &mut Vec<String>) -> Layer {
  let mut layer = Layer::default();
  let string = |name: &str| matches.opt_str(name);
  let flag = |name: &str| matches.opt_present(name).then_some(true);

  layer.debug = flag("debug");
  if let Some(path) = string("debug") {
    layer.logfile = Some(path);
  }
  layer.ipc_timeout = cli_number(matches, "ipc-timeout", 1, MAX_IPC_TIMEOUT, warnings);
  layer.command = string("cmd").map(Some);
  if matches.opt_present("allow-command-editor") && matches.opt_present("no-command-editor") {
    warnings.push(
      "command line: --allow-command-editor conflicts with --no-command-editor; keeping the editor disabled".into(),
    );
  }
  layer.allow_command_editor = if matches.opt_present("no-command-editor") {
    Some(false)
  } else {
    flag("allow-command-editor")
  };
  if matches.opt_present("env") {
    layer.environment = Some(valid_environment(matches.opt_strs("env"), "command line", warnings));
  }
  layer.sessions = string("sessions").map(|value| split_paths(&value));
  layer.xsessions = string("xsessions").map(|value| split_paths(&value));
  layer.session_wrapper = string("session-wrapper").map(Some);
  layer.xsession_wrapper = if matches.opt_present("no-xsession-wrapper") {
    Some(None)
  } else {
    string("xsession-wrapper").map(Some)
  };
  layer.width = cli_number(matches, "width", 1, u16::MAX, warnings);
  layer.issue = flag("issue");
  layer.greeting = string("greeting").map(Some);
  layer.time = flag("time");
  layer.time_format =
    string("time-format").and_then(|value| valid_time_format(&value, "--time-format", warnings).then_some(Some(value)));
  layer.refresh_rate = cli_number(matches, "refresh-rate", 1, MAX_REFRESH_RATE, warnings);
  layer.remember = flag("remember");
  layer.remember_session = flag("remember-session");
  layer.remember_user_session = flag("remember-user-session");
  layer.user_menu = flag("user-menu");
  layer.user_autocomplete = flag("user-autocomplete");
  layer.min_uid = cli_number::<u32>(matches, "user-menu-min-uid", 0, u32::MAX, warnings).map(Some);
  layer.max_uid = cli_number::<u32>(matches, "user-menu-max-uid", 0, u32::MAX, warnings).map(Some);
  if let Some(specification) = string("theme") {
    layer.theme = theme_specification(&specification, "command line --theme", warnings);
  }
  layer.asterisks = flag("asterisks");
  if let Some(value) = string("asterisks-char") {
    if value.is_empty() {
      warnings.push("command line: --asterisks-char is empty; ignoring it".into());
    } else {
      layer.asterisks_chars = Some(value);
    }
  }
  layer.window_padding = cli_number(matches, "window-padding", 0, u16::MAX, warnings);
  layer.container_padding = cli_number(matches, "container-padding", 0, u16::MAX - 1, warnings);
  layer.prompt_padding = cli_number(matches, "prompt-padding", 0, u16::MAX, warnings);
  if let Some(value) = string("greet-align") {
    if matches!(value.as_str(), "left" | "center" | "right") {
      layer.greet_align = Some(value);
    } else {
      warnings.push(format!(
        "command line: invalid --greet-align '{value}'; expected left, center, or right"
      ));
    }
  }
  layer.power_shutdown = cli_power_command(matches, "power-shutdown", warnings);
  layer.power_reboot = cli_power_command(matches, "power-reboot", warnings);
  layer.power_suspend = cli_power_command(matches, "power-suspend", warnings);
  layer.power_hibernate = cli_power_command(matches, "power-hibernate", warnings);
  if matches.opt_present("power-no-setsid") {
    layer.power_setsid = Some(false);
  }
  layer.mock = flag("mock");
  layer.kb_command = cli_number(matches, "kb-command", 1, 12, warnings);
  layer.kb_sessions = cli_number(matches, "kb-sessions", 1, 12, warnings);
  layer.kb_power = cli_number(matches, "kb-power", 1, 12, warnings);
  layer
}

fn cli_power_command(matches: &Matches, name: &str, warnings: &mut Vec<String>) -> Option<PowerCommand> {
  let value = matches.opt_str(name)?;
  match CommandLine::parse(&value) {
    Ok(command) => Some(PowerCommand::Explicit(command)),
    Err(error) => {
      warnings.push(format!("command line: invalid --{name} value: {error}; ignoring it"));
      None
    },
  }
}

fn cli_number<T>(matches: &Matches, name: &str, min: T, max: T, warnings: &mut Vec<String>) -> Option<T>
where
  T: std::str::FromStr + PartialOrd + Copy + std::fmt::Display,
{
  let value = matches.opt_str(name)?;
  match value.parse::<T>() {
    Ok(parsed) if parsed >= min && parsed <= max => Some(parsed),
    _ => {
      warnings.push(format!(
        "command line: invalid --{name} '{value}'; expected {min}..={max}"
      ));
      None
    },
  }
}

fn toml_layer(document: &Document<String>, path: &Path, source: &str, warnings: &mut Vec<String>) -> Layer {
  const ROOT: &[&str] = &[
    "general",
    "session",
    "display",
    "remember",
    "users",
    "secret",
    "layout",
    "power",
    "keybindings",
    "theme",
  ];
  warn_unknown(document.as_table(), ROOT, path, source, warnings, "");
  let mut layer = Layer::default();

  if let Some(table) = read_table(document.as_table(), "general", path, source, warnings) {
    warn_unknown(
      table,
      &["debug", "log-file", "ipc-timeout", "mock"],
      path,
      source,
      warnings,
      "general",
    );
    layer.debug = read_bool(table, "debug", path, source, warnings, "general");
    layer.logfile = read_string(table, "log-file", path, source, warnings, "general");
    layer.ipc_timeout = read_u16(
      table,
      "ipc-timeout",
      (1, MAX_IPC_TIMEOUT),
      path,
      source,
      warnings,
      "general",
    );
    layer.mock = read_bool(table, "mock", path, source, warnings, "general");
  }
  if let Some(table) = read_table(document.as_table(), "session", path, source, warnings) {
    const KEYS: &[&str] = &[
      "command",
      "allow-command-editor",
      "environment",
      "sessions",
      "xsessions",
      "wrapper",
      "xsession-wrapper",
    ];
    warn_unknown(table, KEYS, path, source, warnings, "session");
    layer.command = read_optional_string(table, "command", path, source, warnings, "session");
    layer.allow_command_editor = read_bool(table, "allow-command-editor", path, source, warnings, "session");
    layer.environment = read_strings(table, "environment", path, source, warnings, "session")
      .map(|values| valid_environment(values, &path.display().to_string(), warnings));
    layer.sessions = read_strings(table, "sessions", path, source, warnings, "session");
    layer.xsessions = read_strings(table, "xsessions", path, source, warnings, "session");
    layer.session_wrapper = read_optional_string(table, "wrapper", path, source, warnings, "session");
    layer.xsession_wrapper = read_string_or_false(table, "xsession-wrapper", path, source, warnings, "session");
  }
  if let Some(table) = read_table(document.as_table(), "display", path, source, warnings) {
    const KEYS: &[&str] = &[
      "width",
      "issue",
      "greeting",
      "time",
      "time-format",
      "refresh-rate",
      "theme",
    ];
    warn_unknown(table, KEYS, path, source, warnings, "display");
    layer.width = read_u16(table, "width", (1, u16::MAX), path, source, warnings, "display");
    layer.issue = read_bool(table, "issue", path, source, warnings, "display");
    layer.greeting = read_optional_string(table, "greeting", path, source, warnings, "display");
    layer.time = read_bool(table, "time", path, source, warnings, "display");
    layer.time_format =
      read_optional_string(table, "time-format", path, source, warnings, "display").and_then(|value| {
        value.map_or(Some(None), |format| {
          valid_time_format(&format, "display.time-format", warnings).then_some(Some(format))
        })
      });
    layer.refresh_rate = read_u16(
      table,
      "refresh-rate",
      (1, MAX_REFRESH_RATE),
      path,
      source,
      warnings,
      "display",
    );
    if let Some(specification) = read_string(table, "theme", path, source, warnings, "display") {
      layer.theme = theme_specification(&specification, "display.theme", warnings);
    }
  }
  if let Some(table) = read_table(document.as_table(), "remember", path, source, warnings) {
    warn_unknown(
      table,
      &["username", "session", "user-session"],
      path,
      source,
      warnings,
      "remember",
    );
    layer.remember = read_bool(table, "username", path, source, warnings, "remember");
    layer.remember_session = read_bool(table, "session", path, source, warnings, "remember");
    layer.remember_user_session = read_bool(table, "user-session", path, source, warnings, "remember");
  }
  if let Some(table) = read_table(document.as_table(), "users", path, source, warnings) {
    warn_unknown(
      table,
      &["menu", "autocomplete", "min-uid", "max-uid"],
      path,
      source,
      warnings,
      "users",
    );
    layer.user_menu = read_bool(table, "menu", path, source, warnings, "users");
    layer.user_autocomplete = read_bool(table, "autocomplete", path, source, warnings, "users");
    layer.min_uid = read_u32(table, "min-uid", path, source, warnings, "users").map(Some);
    layer.max_uid = read_u32(table, "max-uid", path, source, warnings, "users").map(Some);
  }
  if let Some(table) = read_table(document.as_table(), "secret", path, source, warnings) {
    warn_unknown(table, &["asterisks", "characters"], path, source, warnings, "secret");
    layer.asterisks = read_bool(table, "asterisks", path, source, warnings, "secret");
    if let Some(value) = read_string(table, "characters", path, source, warnings, "secret") {
      if value.is_empty() {
        warn_item(
          table.get("characters"),
          path,
          source,
          warnings,
          "secret.characters must not be empty",
        );
      } else {
        layer.asterisks_chars = Some(value);
      }
    }
  }
  if let Some(table) = read_table(document.as_table(), "layout", path, source, warnings) {
    const KEYS: &[&str] = &["window-padding", "container-padding", "prompt-padding", "greet-align"];
    warn_unknown(table, KEYS, path, source, warnings, "layout");
    layer.window_padding = read_u16(table, "window-padding", (0, u16::MAX), path, source, warnings, "layout");
    layer.container_padding = read_u16(
      table,
      "container-padding",
      (0, u16::MAX - 1),
      path,
      source,
      warnings,
      "layout",
    );
    layer.prompt_padding = read_u16(table, "prompt-padding", (0, u16::MAX), path, source, warnings, "layout");
    if let Some(value) = read_string(table, "greet-align", path, source, warnings, "layout") {
      if matches!(value.as_str(), "left" | "center" | "right") {
        layer.greet_align = Some(value);
      } else {
        warn_item(
          table.get("greet-align"),
          path,
          source,
          warnings,
          "layout.greet-align must be left, center, or right",
        );
      }
    }
  }
  if let Some(table) = read_table(document.as_table(), "power", path, source, warnings) {
    const KEYS: &[&str] = &["shutdown", "reboot", "suspend", "hibernate", "setsid"];
    warn_unknown(table, KEYS, path, source, warnings, "power");
    layer.power_shutdown = read_power_command(table, "shutdown", path, source, warnings);
    layer.power_reboot = read_power_command(table, "reboot", path, source, warnings);
    layer.power_suspend = read_power_command(table, "suspend", path, source, warnings);
    layer.power_hibernate = read_power_command(table, "hibernate", path, source, warnings);
    layer.power_setsid = read_bool(table, "setsid", path, source, warnings, "power");
  }
  if let Some(table) = read_table(document.as_table(), "keybindings", path, source, warnings) {
    warn_unknown(
      table,
      &["command", "sessions", "power"],
      path,
      source,
      warnings,
      "keybindings",
    );
    layer.kb_command = read_u8(table, "command", (1, 12), path, source, warnings, "keybindings");
    layer.kb_sessions = read_u8(table, "sessions", (1, 12), path, source, warnings, "keybindings");
    layer.kb_power = read_u8(table, "power", (1, 12), path, source, warnings, "keybindings");
  }
  if let Some(table) = read_table(document.as_table(), "theme", path, source, warnings) {
    const KEYS: &[&str] = &[
      "border",
      "text",
      "time",
      "container",
      "title",
      "greet",
      "prompt",
      "input",
      "action",
      "button",
    ];
    warn_unknown(table, KEYS, path, source, warnings, "theme");
    macro_rules! read_theme {
      ($($field:ident),* $(,)?) => {
        $(layer.theme.$field = read_theme_color(table, stringify!($field), path, source, warnings);)*
      };
    }
    read_theme!(
      border, text, time, container, title, greet, prompt, input, action, button
    );
  }
  layer
}

fn read_table<'a>(
  root: &'a Table,
  key: &str,
  path: &Path,
  source: &str,
  warnings: &mut Vec<String>,
) -> Option<&'a Table> {
  let item = root.get(key)?;
  match item.as_table() {
    Some(table) => Some(table),
    None => {
      warn_item(Some(item), path, source, warnings, &format!("{key} must be a table"));
      None
    },
  }
}

fn warn_unknown(table: &Table, allowed: &[&str], path: &Path, source: &str, warnings: &mut Vec<String>, prefix: &str) {
  for (key, item) in table.iter() {
    if !allowed.contains(&key) {
      let field = if prefix.is_empty() {
        key.into()
      } else {
        format!("{prefix}.{key}")
      };
      warn_span(
        table.key(key).and_then(|key| key.span()).or_else(|| item.span()),
        path,
        source,
        warnings,
        &format!("unknown field '{field}'; ignoring it"),
      );
    }
  }
}

macro_rules! scalar_reader {
  ($name:ident, $ty:ty, $method:ident, $expected:literal) => {
    fn $name(
      table: &Table,
      key: &str,
      path: &Path,
      source: &str,
      warnings: &mut Vec<String>,
      prefix: &str,
    ) -> Option<$ty> {
      let item = table.get(key)?;
      match item.$method() {
        Some(value) => Some(value.into()),
        None => {
          warn_item(
            Some(item),
            path,
            source,
            warnings,
            &format!("{prefix}.{key} must be {}", $expected),
          );
          None
        },
      }
    }
  };
}

scalar_reader!(read_bool, bool, as_bool, "a boolean");
scalar_reader!(read_string, String, as_str, "a string");

fn read_optional_string(
  table: &Table,
  key: &str,
  path: &Path,
  source: &str,
  warnings: &mut Vec<String>,
  prefix: &str,
) -> Option<Option<String>> {
  let value = read_string(table, key, path, source, warnings, prefix)?;
  Some((!value.is_empty()).then_some(value))
}

fn read_power_command(
  table: &Table,
  key: &str,
  path: &Path,
  source: &str,
  warnings: &mut Vec<String>,
) -> Option<PowerCommand> {
  let item = table.get(key)?;
  if item.as_bool() == Some(false) {
    return Some(PowerCommand::Disabled);
  }

  let parsed = if let Some(array) = item.as_array() {
    let argv = array
      .iter()
      .map(|value| value.as_str().map(str::to_string))
      .collect::<Option<Vec<_>>>();
    match argv {
      Some(argv) => CommandLine::from_argv(argv),
      None => {
        warn_item(
          Some(item),
          path,
          source,
          warnings,
          &format!("power.{key} must contain only strings"),
        );
        return None;
      },
    }
  } else if let Some(value) = item.as_str() {
    CommandLine::parse(value)
  } else {
    warn_item(
      Some(item),
      path,
      source,
      warnings,
      &format!("power.{key} must be an argument array, a legacy command string, or false"),
    );
    return None;
  };

  match parsed {
    Ok(command) => Some(PowerCommand::Explicit(command)),
    Err(error) => {
      warn_item(
        Some(item),
        path,
        source,
        warnings,
        &format!("power.{key} is invalid: {error}; ignoring it"),
      );
      None
    },
  }
}

fn read_string_or_false(
  table: &Table,
  key: &str,
  path: &Path,
  source: &str,
  warnings: &mut Vec<String>,
  prefix: &str,
) -> Option<Option<String>> {
  let item = table.get(key)?;
  if item.as_bool() == Some(false) {
    Some(None)
  } else if let Some(value) = item.as_str() {
    Some((!value.is_empty()).then(|| value.to_string()))
  } else {
    warn_item(
      Some(item),
      path,
      source,
      warnings,
      &format!("{prefix}.{key} must be a command string or false"),
    );
    None
  }
}

fn read_strings(
  table: &Table,
  key: &str,
  path: &Path,
  source: &str,
  warnings: &mut Vec<String>,
  prefix: &str,
) -> Option<Vec<String>> {
  let item = table.get(key)?;
  let Some(array) = item.as_array() else {
    warn_item(
      Some(item),
      path,
      source,
      warnings,
      &format!("{prefix}.{key} must be an array of strings"),
    );
    return None;
  };
  let mut values = Vec::new();
  for value in array {
    if let Some(value) = value.as_str() {
      values.push(value.to_string());
    } else {
      warn_item(
        Some(item),
        path,
        source,
        warnings,
        &format!("{prefix}.{key} contains a non-string value; ignoring it"),
      );
    }
  }
  Some(values)
}

fn read_integer(
  table: &Table,
  key: &str,
  bounds: (u64, u64),
  path: &Path,
  source: &str,
  warnings: &mut Vec<String>,
  prefix: &str,
) -> Option<u64> {
  let (min, max) = bounds;
  let item = table.get(key)?;
  match item.as_integer().and_then(|value| u64::try_from(value).ok()) {
    Some(value) if (min..=max).contains(&value) => Some(value),
    _ => {
      warn_item(
        Some(item),
        path,
        source,
        warnings,
        &format!("{prefix}.{key} must be an integer in {min}..={max}"),
      );
      None
    },
  }
}

macro_rules! integer_reader {
  ($name:ident, $ty:ty) => {
    fn $name(
      table: &Table,
      key: &str,
      bounds: ($ty, $ty),
      path: &Path,
      source: &str,
      warnings: &mut Vec<String>,
      prefix: &str,
    ) -> Option<$ty> {
      read_integer(
        table,
        key,
        (bounds.0.into(), bounds.1.into()),
        path,
        source,
        warnings,
        prefix,
      )
      .map(|value| value as $ty)
    }
  };
}

integer_reader!(read_u8, u8);
integer_reader!(read_u16, u16);

fn read_u32(
  table: &Table,
  key: &str,
  path: &Path,
  source: &str,
  warnings: &mut Vec<String>,
  prefix: &str,
) -> Option<u32> {
  read_integer(table, key, (0, u64::from(u32::MAX)), path, source, warnings, prefix).map(|value| value as u32)
}

fn warn_item(item: Option<&Item>, path: &Path, source: &str, warnings: &mut Vec<String>, message: &str) {
  warn_span(item.and_then(Item::span), path, source, warnings, message);
}

fn warn_span(span: Option<Range<usize>>, path: &Path, source: &str, warnings: &mut Vec<String>, message: &str) {
  warnings.push(span.map_or_else(
    || format!("warning: {message}\n  --> {}", path.display()),
    |span| source_diagnostic("warning", "invalid configuration", path, source, span, message),
  ));
}

fn line_column(source: &str, offset: usize) -> (usize, usize) {
  let before = &source[..offset.min(source.len())];
  let line = before.bytes().filter(|byte| *byte == b'\n').count() + 1;
  let column = before
    .rsplit_once('\n')
    .map_or(before, |(_, line)| line)
    .chars()
    .count()
    + 1;
  (line, column)
}

fn valid_environment(values: Vec<String>, source: &str, warnings: &mut Vec<String>) -> Vec<String> {
  values
    .into_iter()
    .filter(|value| {
      let valid = value.split_once('=').is_some_and(|(key, _)| !key.is_empty());
      if !valid {
        warnings.push(format!("{source}: malformed environment entry '{value}'; ignoring it"));
      }
      valid
    })
    .collect()
}

fn valid_time_format(format: &str, field: &str, warnings: &mut Vec<String>) -> bool {
  use chrono::format::{Item as ChronoItem, StrftimeItems};

  if StrftimeItems::new(format).any(|item| item == ChronoItem::Error) {
    warnings.push(format!("invalid {field} value '{format}'; ignoring it"));
    false
  } else {
    true
  }
}

fn valid_color(value: &str) -> bool {
  Color::from_str(value).is_ok()
}

fn read_theme_color(
  table: &Table,
  key: &str,
  path: &Path,
  source: &str,
  warnings: &mut Vec<String>,
) -> Option<Option<String>> {
  let item = table.get(key)?;
  if item.as_bool() == Some(false) {
    return Some(None);
  }
  let Some(value) = item.as_str() else {
    warn_item(
      Some(item),
      path,
      source,
      warnings,
      &format!("theme.{key} must be a color string or false"),
    );
    return None;
  };
  if valid_color(value) {
    Some(Some(value.to_string()))
  } else {
    warn_item(
      Some(item),
      path,
      source,
      warnings,
      &format!("theme.{key} has invalid color '{value}'; ignoring it"),
    );
    None
  }
}

fn theme_specification(specification: &str, source: &str, warnings: &mut Vec<String>) -> ThemeLayer {
  let mut theme = ThemeLayer {
    border: Some(None),
    text: Some(None),
    time: Some(None),
    container: Some(None),
    title: Some(None),
    greet: Some(None),
    prompt: Some(None),
    input: Some(None),
    action: Some(None),
    button: Some(None),
  };
  for directive in specification.split(';').filter(|directive| !directive.is_empty()) {
    let Some((key, value)) = directive.split_once('=') else {
      warnings.push(format!(
        "{source}: malformed theme directive '{directive}'; ignoring it"
      ));
      continue;
    };
    if !valid_color(value) {
      warnings.push(format!("{source}: invalid color '{value}' for '{key}'; ignoring it"));
      continue;
    }
    let destination = match key {
      "border" => &mut theme.border,
      "text" => &mut theme.text,
      "time" => &mut theme.time,
      "container" => &mut theme.container,
      "title" => &mut theme.title,
      "greet" => &mut theme.greet,
      "prompt" => &mut theme.prompt,
      "input" => &mut theme.input,
      "action" => &mut theme.action,
      "button" => &mut theme.button,
      _ => {
        warnings.push(format!("{source}: unknown theme component '{key}'; ignoring it"));
        continue;
      },
    };
    *destination = Some(Some(value.to_string()));
  }
  theme
}

fn split_paths(value: &str) -> Vec<String> {
  env::split_paths(value)
    .map(|path| path.to_string_lossy().into_owned())
    .collect()
}

#[cfg(test)]
mod tests {
  use std::{fs, path::Path};

  use tempfile::tempdir;

  use super::{load_paths, reload_paths};
  use crate::{Greeter, power::PowerCommand};

  fn matches(args: &[&str]) -> getopts::Matches {
    Greeter::options().parse(args).unwrap()
  }

  fn write(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
  }

  #[test]
  fn command_editor_requires_explicit_opt_in() {
    let (defaults, warnings) = load_paths(None, None, &matches(&[]));
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(!defaults.allow_command_editor);

    let dir = tempdir().unwrap();
    let config = dir.path().join("config.toml");
    write(&config, "[session]\nallow-command-editor = true\n");
    let (from_file, warnings) = load_paths(Some(&config), None, &matches(&[]));
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(from_file.allow_command_editor);

    let (from_cli, warnings) = load_paths(None, None, &matches(&["--allow-command-editor"]));
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(from_cli.allow_command_editor);

    let (disabled_by_cli, warnings) = load_paths(Some(&config), None, &matches(&["--no-command-editor"]));
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(!disabled_by_cli.allow_command_editor);

    let (conflicting_cli, warnings) =
      load_paths(None, None, &matches(&["--allow-command-editor", "--no-command-editor"]));
    assert!(!conflicting_cli.allow_command_editor);
    assert!(warnings.iter().any(|warning| warning.contains("conflicts")));
  }

  #[test]
  fn layers_every_field_without_losing_false_or_zero() {
    let dir = tempdir().unwrap();
    let system = dir.path().join("system.toml");
    let explicit = dir.path().join("explicit.toml");
    write(
      &system,
      "[general]\ndebug = true\n[display]\nwidth = 40\ntime = true\n[layout]\nwindow-padding = 9\n",
    );
    write(
      &explicit,
      "[general]\ndebug = false\n[display]\nwidth = 70\n[layout]\nwindow-padding = 0\n",
    );
    let cli = matches(&["--width", "80"]);

    let (settings, warnings) = load_paths(Some(&system), Some(&explicit), &cli);

    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(!settings.debug);
    assert!(settings.time);
    assert_eq!(settings.window_padding, 0);
    assert_eq!(settings.width, 80);
  }

  #[test]
  fn bad_fields_do_not_discard_valid_siblings() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("config.toml");
    write(
      &config,
      "[display]\nwidth = 'wide'\ntime = true\nmystery = 1\n[secret]\ncharacters = ''\n",
    );

    let (settings, warnings) = load_paths(Some(&config), None, &matches(&[]));

    assert!(settings.time);
    assert_eq!(settings.width, 80);
    assert_eq!(settings.asterisks_chars, "*");
    assert!(
      warnings
        .iter()
        .any(|warning| warning.contains("display.width") && warning.contains("config.toml:"))
    );
    assert!(
      warnings
        .iter()
        .any(|warning| warning.contains("unknown field 'display.mystery'"))
    );
    assert!(
      warnings
        .iter()
        .any(|warning| warning.contains("secret.characters must not be empty"))
    );
  }

  #[test]
  fn invalid_relationships_keep_the_previous_layer() {
    let dir = tempdir().unwrap();
    let system = dir.path().join("system.toml");
    let explicit = dir.path().join("explicit.toml");
    write(
      &system,
      "[users]\nmin-uid = 1000\nmax-uid = 60000\n[keybindings]\ncommand = 1\nsessions = 2\npower = 3\n",
    );
    write(
      &explicit,
      "[users]\nmin-uid = 9000\nmax-uid = 8000\n[keybindings]\ncommand = 2\n[remember]\nuser-session = true\n",
    );

    let (settings, warnings) = load_paths(Some(&system), Some(&explicit), &matches(&[]));

    assert_eq!((settings.min_uid, settings.max_uid), (Some(1000), Some(60000)));
    assert_eq!(
      (settings.kb_command, settings.kb_sessions, settings.kb_power),
      (1, 2, 3)
    );
    assert!(settings.remember);
    assert!(settings.remember_user_session);
    assert!(warnings.iter().any(|warning| warning.contains("min-uid exceeds")));
    assert!(warnings.iter().any(|warning| warning.contains("duplicate keybinding")));
  }

  #[test]
  fn power_commands_support_argv_legacy_strings_and_explicit_disable() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("config.toml");
    write(
      &config,
      "[power]\nshutdown = ['sudo', 'systemctl', 'poweroff', 'two words', '']\nreboot = \"command 'quoted argument' '$HOME' '|'\"\nsuspend = false\nhibernate = ['hibernate-now']\n",
    );

    let (settings, warnings) = load_paths(Some(&config), None, &matches(&[]));

    assert!(warnings.is_empty(), "{warnings:?}");
    let PowerCommand::Explicit(shutdown) = settings.power_shutdown else {
      panic!("argv power command was not applied");
    };
    assert_eq!(shutdown.argv(), ["sudo", "systemctl", "poweroff", "two words", ""]);
    let PowerCommand::Explicit(reboot) = settings.power_reboot else {
      panic!("legacy power command was not applied");
    };
    assert_eq!(reboot.argv(), ["command", "quoted argument", "$HOME", "|"]);
    assert_eq!(settings.power_suspend, PowerCommand::Disabled);
    let PowerCommand::Explicit(hibernate) = settings.power_hibernate else {
      panic!("single-argument power command was not applied");
    };
    assert_eq!(hibernate.argv(), ["hibernate-now"]);
  }

  #[test]
  fn invalid_power_values_warn_and_preserve_the_previous_layer() {
    let dir = tempdir().unwrap();
    let system = dir.path().join("system.toml");
    let explicit = dir.path().join("explicit.toml");
    write(
      &system,
      "[power]\nshutdown = ['system-shutdown']\nreboot = ['system-reboot']\nsuspend = ['system-suspend']\nhibernate = ['system-hibernate']\n",
    );
    write(
      &explicit,
      "[power]\nshutdown = []\nreboot = ['explicit-reboot', 1]\nsuspend = \"unterminated '\"\nhibernate = true\n",
    );

    let (settings, warnings) = load_paths(
      Some(&system),
      Some(&explicit),
      &matches(&["--power-shutdown", "cli-command '"]),
    );

    for (command, expected) in [
      (&settings.power_shutdown, "system-shutdown"),
      (&settings.power_reboot, "system-reboot"),
      (&settings.power_suspend, "system-suspend"),
      (&settings.power_hibernate, "system-hibernate"),
    ] {
      let PowerCommand::Explicit(command) = command else {
        panic!("invalid higher layer replaced a valid lower layer");
      };
      assert_eq!(command.argv(), [expected]);
    }
    assert_eq!(warnings.len(), 5, "{warnings:?}");
    assert!(
      warnings
        .iter()
        .any(|warning| warning.contains("power.shutdown is invalid"))
    );
    assert!(
      warnings
        .iter()
        .any(|warning| warning.contains("power.reboot must contain only strings"))
    );
    assert!(
      warnings
        .iter()
        .any(|warning| warning.contains("power.suspend is invalid"))
    );
    assert!(
      warnings
        .iter()
        .any(|warning| warning.contains("power.hibernate must be an argument array"))
    );
    assert!(
      warnings
        .iter()
        .any(|warning| warning.contains("command line: invalid --power-shutdown"))
    );
  }

  #[test]
  fn command_line_power_values_use_the_legacy_string_parser() {
    let (settings, warnings) = load_paths(
      None,
      None,
      &matches(&["--power-reboot", "program 'two words' '$HOME' '|' ''"]),
    );

    assert!(warnings.is_empty(), "{warnings:?}");
    let PowerCommand::Explicit(command) = settings.power_reboot else {
      panic!("command-line power command was not applied");
    };
    assert_eq!(command.argv(), ["program", "two words", "$HOME", "|", ""]);
  }

  #[test]
  fn malformed_document_is_ignored() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("config.toml");
    write(&config, "[display\ntime = true");

    let (settings, warnings) = load_paths(Some(&config), None, &matches(&[]));

    assert!(!settings.time);
    assert!(warnings.iter().any(|warning| warning.contains("invalid TOML")));
    assert!(warnings.iter().any(|warning| warning.contains("-->")));
  }

  #[test]
  fn duplicate_keys_are_rejected_with_source_context() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("config.toml");
    write(&config, "[display]\nwidth = 80\nwidth = 100\n");

    let (settings, warnings) = load_paths(Some(&config), None, &matches(&[]));

    assert_eq!(settings.width, 80);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("config.toml:3:1"));
    assert!(warnings[0].contains("duplicate key"));
    assert!(warnings[0].contains("3 | width = 100"));
  }

  #[test]
  fn reload_rejects_malformed_files_and_preserves_cli_precedence() {
    let dir = tempdir().unwrap();
    let system = dir.path().join("system.toml");
    let explicit = dir.path().join("explicit.toml");
    write(&system, "[display]\nwidth = 40\ntime = true\n");
    write(&explicit, "[display]\nwidth = 60\n");
    let cli = matches(&["--width", "80"]);

    let (settings, warnings) = reload_paths(&system, Some(&explicit), &cli).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(settings.width, 80);
    assert!(settings.time);

    write(&explicit, "[display\nwidth = 60");
    assert!(reload_paths(&system, Some(&explicit), &cli).is_err());
  }

  #[test]
  fn array_entries_are_filtered_individually() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("config.toml");
    write(&config, "[session]\nenvironment = ['A=B', 1, 'INVALID', 'C=D=E']\n");

    let (settings, warnings) = load_paths(Some(&config), None, &matches(&[]));

    assert_eq!(settings.environment, ["A=B", "C=D=E"]);
    assert_eq!(warnings.len(), 2);
  }

  #[test]
  fn distributed_example_is_complete_and_valid() {
    let document = include_str!("../contrib/tuigreet.toml")
      .parse::<toml_edit::Document<String>>()
      .unwrap();
    let mut warnings = Vec::new();
    let layer = super::toml_layer(
      &document,
      Path::new("contrib/tuigreet.toml"),
      include_str!("../contrib/tuigreet.toml"),
      &mut warnings,
    );
    let mut settings = super::Settings::default();
    super::apply_layer(&mut settings, layer, "example", &mut warnings);

    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(settings, super::Settings::default());
  }

  #[test]
  fn theme_fields_merge_and_can_be_cleared() {
    let dir = tempdir().unwrap();
    let system = dir.path().join("system.toml");
    let explicit = dir.path().join("explicit.toml");
    write(&system, "[theme]\nborder = 'blue'\ntext = 'white'\n");
    write(&explicit, "[theme]\nborder = false\nprompt = 'green'\n");

    let (settings, warnings) = load_paths(Some(&system), Some(&explicit), &matches(&[]));

    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(settings.theme.border, None);
    assert_eq!(settings.theme.text.as_deref(), Some("white"));
    assert_eq!(settings.theme.prompt.as_deref(), Some("green"));
  }

  #[test]
  fn invalid_theme_fields_do_not_replace_valid_colors() {
    let dir = tempdir().unwrap();
    let system = dir.path().join("system.toml");
    let explicit = dir.path().join("explicit.toml");
    write(&system, "[theme]\nborder = 'blue'\n");
    write(&explicit, "[theme]\nborder = 'not-a-color'\nunknown = 'red'\n");

    let (settings, warnings) = load_paths(Some(&system), Some(&explicit), &matches(&[]));

    assert_eq!(settings.theme.border.as_deref(), Some("blue"));
    assert!(warnings.iter().any(|warning| warning.contains("invalid color")));
    assert!(warnings.iter().any(|warning| warning.contains("theme.unknown")));
  }
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ThemeSettings {
  pub border: Option<String>,
  pub text: Option<String>,
  pub time: Option<String>,
  pub container: Option<String>,
  pub title: Option<String>,
  pub greet: Option<String>,
  pub prompt: Option<String>,
  pub input: Option<String>,
  pub action: Option<String>,
  pub button: Option<String>,
}

impl ThemeSettings {
  pub fn specification(&self) -> String {
    let fields = [
      ("border", &self.border),
      ("text", &self.text),
      ("time", &self.time),
      ("container", &self.container),
      ("title", &self.title),
      ("greet", &self.greet),
      ("prompt", &self.prompt),
      ("input", &self.input),
      ("action", &self.action),
      ("button", &self.button),
    ];
    fields
      .into_iter()
      .filter_map(|(name, value)| value.as_ref().map(|value| format!("{name}={value}")))
      .collect::<Vec<_>>()
      .join(";")
  }
}
