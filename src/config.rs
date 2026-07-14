use std::{
  env,
  fs,
  path::{Path, PathBuf},
};

use getopts::Matches;
use toml_edit::{DocumentMut, Item, Table};

use crate::event::{DEFAULT_REFRESH_RATE, MAX_REFRESH_RATE};

pub const SYSTEM_CONFIG: &str = "/etc/tuigreet/config.toml";
const DEFAULT_LOG_FILE: &str = "/tmp/tuigreet.log";
const DEFAULT_XSESSION_WRAPPER: &str = "startx /usr/bin/env";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Settings {
  pub debug: bool,
  pub logfile: String,
  pub command: Option<String>,
  pub environment: Vec<String>,
  pub sessions: Vec<String>,
  pub xsessions: Vec<String>,
  pub session_wrapper: Option<String>,
  pub xsession_wrapper: Option<String>,
  pub width: u16,
  pub issue: bool,
  pub greeting: Option<String>,
  pub text_config: bool,
  pub text_config_file: Option<PathBuf>,
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
  pub theme: Option<String>,
  pub asterisks: bool,
  pub asterisks_chars: String,
  pub window_padding: u16,
  pub container_padding: u16,
  pub prompt_padding: u16,
  pub greet_align: String,
  pub power_shutdown: Option<String>,
  pub power_reboot: Option<String>,
  pub power_suspend: Option<String>,
  pub power_hibernate: Option<String>,
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
      command: None,
      environment: Vec::new(),
      sessions: Vec::new(),
      xsessions: Vec::new(),
      session_wrapper: None,
      xsession_wrapper: Some(DEFAULT_XSESSION_WRAPPER.into()),
      width: 80,
      issue: false,
      greeting: None,
      text_config: false,
      text_config_file: None,
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
      theme: None,
      asterisks: false,
      asterisks_chars: "*".into(),
      window_padding: 0,
      container_padding: 1,
      prompt_padding: 1,
      greet_align: "center".into(),
      power_shutdown: None,
      power_reboot: None,
      power_suspend: None,
      power_hibernate: None,
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
  command: Option<Option<String>>,
  environment: Option<Vec<String>>,
  sessions: Option<Vec<String>>,
  xsessions: Option<Vec<String>>,
  session_wrapper: Option<Option<String>>,
  xsession_wrapper: Option<Option<String>>,
  width: Option<u16>,
  issue: Option<bool>,
  greeting: Option<Option<String>>,
  text_config: Option<bool>,
  text_config_file: Option<Option<PathBuf>>,
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
  theme: Option<Option<String>>,
  asterisks: Option<bool>,
  asterisks_chars: Option<String>,
  window_padding: Option<u16>,
  container_padding: Option<u16>,
  prompt_padding: Option<u16>,
  greet_align: Option<String>,
  power_shutdown: Option<Option<String>>,
  power_reboot: Option<Option<String>>,
  power_suspend: Option<Option<String>>,
  power_hibernate: Option<Option<String>>,
  power_setsid: Option<bool>,
  mock: Option<bool>,
  kb_command: Option<u8>,
  kb_sessions: Option<u8>,
  kb_power: Option<u8>,
}

pub fn load(matches: &Matches) -> (Settings, Vec<String>) {
  let user = user_config_path();
  let explicit = matches.opt_str("config").map(PathBuf::from);
  load_paths(
    Some(Path::new(SYSTEM_CONFIG)),
    user.as_deref(),
    explicit.as_deref(),
    matches,
  )
}

fn load_paths(
  system: Option<&Path>,
  user: Option<&Path>,
  explicit: Option<&Path>,
  matches: &Matches,
) -> (Settings, Vec<String>) {
  let mut settings = Settings::default();
  let mut warnings = Vec::new();

  if let Some(path) = system {
    load_optional(path, &mut settings, &mut warnings);
  }
  if let Some(path) = user {
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
    Ok(true) => load_required(path, settings, warnings),
    Ok(false) => {},
    Err(error) => warnings.push(format!("{}: cannot access configuration: {error}", path.display())),
  }
}

fn load_required(path: &Path, settings: &mut Settings, warnings: &mut Vec<String>) {
  let content = match fs::read_to_string(path) {
    Ok(content) => content,
    Err(error) => {
      warnings.push(format!("{}: cannot read configuration: {error}", path.display()));
      return;
    },
  };
  let document = match content.parse::<DocumentMut>() {
    Ok(document) => document,
    Err(error) => {
      warnings.push(format!("{}: invalid TOML: {error}", path.display()));
      return;
    },
  };

  let layer = toml_layer(&document, path, &content, warnings);
  apply_layer(settings, layer, &path.display().to_string(), warnings);
}

fn user_config_path() -> Option<PathBuf> {
  env::var_os("XDG_CONFIG_HOME")
    .map(PathBuf::from)
    .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
    .map(|path| path.join("tuigreet/config.toml"))
}

fn apply_layer(settings: &mut Settings, layer: Layer, source: &str, warnings: &mut Vec<String>) {
  macro_rules! apply {
    ($($field:ident),* $(,)?) => { $(if let Some(value) = layer.$field { settings.$field = value; })* };
  }

  apply!(
    debug,
    logfile,
    command,
    environment,
    sessions,
    xsessions,
    session_wrapper,
    xsession_wrapper,
    width,
    text_config,
    text_config_file,
    time,
    time_format,
    refresh_rate,
    remember,
    user_menu,
    user_autocomplete,
    theme,
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
  layer.command = string("cmd").map(Some);
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
  layer.text_config = flag("text-config");
  layer.text_config_file = string("text-config-file").map(|path| Some(path.into()));
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
  layer.theme = string("theme").map(Some);
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
  layer.power_shutdown = string("power-shutdown").map(Some);
  layer.power_reboot = string("power-reboot").map(Some);
  layer.power_suspend = string("power-suspend").map(Some);
  layer.power_hibernate = string("power-hibernate").map(Some);
  if matches.opt_present("power-no-setsid") {
    layer.power_setsid = Some(false);
  }
  layer.mock = flag("mock");
  layer.kb_command = cli_number(matches, "kb-command", 1, 12, warnings);
  layer.kb_sessions = cli_number(matches, "kb-sessions", 1, 12, warnings);
  layer.kb_power = cli_number(matches, "kb-power", 1, 12, warnings);
  layer
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

fn toml_layer(document: &DocumentMut, path: &Path, source: &str, warnings: &mut Vec<String>) -> Layer {
  const ROOT: &[&str] = &[
    "general",
    "session",
    "display",
    "text",
    "remember",
    "users",
    "secret",
    "layout",
    "power",
    "keybindings",
  ];
  warn_unknown(document.as_table(), ROOT, path, source, warnings, "");
  let mut layer = Layer::default();

  if let Some(table) = read_table(document.as_table(), "general", path, source, warnings) {
    warn_unknown(table, &["debug", "log-file", "mock"], path, source, warnings, "general");
    layer.debug = read_bool(table, "debug", path, source, warnings, "general");
    layer.logfile = read_string(table, "log-file", path, source, warnings, "general");
    layer.mock = read_bool(table, "mock", path, source, warnings, "general");
  }
  if let Some(table) = read_table(document.as_table(), "session", path, source, warnings) {
    const KEYS: &[&str] = &[
      "command",
      "environment",
      "sessions",
      "xsessions",
      "wrapper",
      "xsession-wrapper",
    ];
    warn_unknown(table, KEYS, path, source, warnings, "session");
    layer.command = read_optional_string(table, "command", path, source, warnings, "session");
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
    layer.theme = read_optional_string(table, "theme", path, source, warnings, "display");
  }
  if let Some(table) = read_table(document.as_table(), "text", path, source, warnings) {
    warn_unknown(table, &["enabled", "file"], path, source, warnings, "text");
    layer.text_config = read_bool(table, "enabled", path, source, warnings, "text");
    layer.text_config_file =
      read_optional_string(table, "file", path, source, warnings, "text").map(|value| value.map(PathBuf::from));
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
    layer.power_shutdown = read_optional_string(table, "shutdown", path, source, warnings, "power");
    layer.power_reboot = read_optional_string(table, "reboot", path, source, warnings, "power");
    layer.power_suspend = read_optional_string(table, "suspend", path, source, warnings, "power");
    layer.power_hibernate = read_optional_string(table, "hibernate", path, source, warnings, "power");
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
      warn_item(
        Some(item),
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
  let location = item
    .and_then(Item::span)
    .map(|span| line_column(source, span.start))
    .map_or_else(
      || path.display().to_string(),
      |(line, column)| format!("{}:{line}:{column}", path.display()),
    );
  warnings.push(format!("{location}: {message}"));
}

fn line_column(source: &str, offset: usize) -> (usize, usize) {
  let before = &source[..offset.min(source.len())];
  let line = before.bytes().filter(|byte| *byte == b'\n').count() + 1;
  let column = before.rsplit_once('\n').map_or(before.len(), |(_, line)| line.len()) + 1;
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

fn split_paths(value: &str) -> Vec<String> {
  env::split_paths(value)
    .map(|path| path.to_string_lossy().into_owned())
    .collect()
}

#[cfg(test)]
mod tests {
  use std::{fs, path::Path};

  use tempfile::tempdir;

  use super::load_paths;
  use crate::Greeter;

  fn matches(args: &[&str]) -> getopts::Matches {
    Greeter::options().parse(args).unwrap()
  }

  fn write(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
  }

  #[test]
  fn layers_every_field_without_losing_false_or_zero() {
    let dir = tempdir().unwrap();
    let system = dir.path().join("system.toml");
    let user = dir.path().join("user.toml");
    let explicit = dir.path().join("explicit.toml");
    write(
      &system,
      "[general]\ndebug = true\n[display]\nwidth = 40\ntime = true\n[layout]\nwindow-padding = 9\n",
    );
    write(
      &user,
      "[general]\ndebug = false\n[display]\nwidth = 60\n[layout]\nwindow-padding = 0\n",
    );
    write(&explicit, "[display]\nwidth = 70\n");
    let cli = matches(&["--width", "80"]);

    let (settings, warnings) = load_paths(Some(&system), Some(&user), Some(&explicit), &cli);

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

    let (settings, warnings) = load_paths(Some(&config), None, None, &matches(&[]));

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
    let user = dir.path().join("user.toml");
    write(
      &system,
      "[users]\nmin-uid = 1000\nmax-uid = 60000\n[keybindings]\ncommand = 1\nsessions = 2\npower = 3\n",
    );
    write(
      &user,
      "[users]\nmin-uid = 9000\nmax-uid = 8000\n[keybindings]\ncommand = 2\n[remember]\nuser-session = true\n",
    );

    let (settings, warnings) = load_paths(Some(&system), Some(&user), None, &matches(&[]));

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
  fn malformed_document_is_ignored() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("config.toml");
    write(&config, "[display\ntime = true");

    let (settings, warnings) = load_paths(Some(&config), None, None, &matches(&[]));

    assert!(!settings.time);
    assert!(warnings.iter().any(|warning| warning.contains("invalid TOML")));
  }

  #[test]
  fn array_entries_are_filtered_individually() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("config.toml");
    write(&config, "[session]\nenvironment = ['A=B', 1, 'INVALID', 'C=D=E']\n");

    let (settings, warnings) = load_paths(Some(&config), None, None, &matches(&[]));

    assert_eq!(settings.environment, ["A=B", "C=D=E"]);
    assert_eq!(warnings.len(), 2);
  }

  #[test]
  fn distributed_example_is_complete_and_valid() {
    let document = include_str!("../contrib/tuigreet.toml")
      .parse::<toml_edit::DocumentMut>()
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
}
