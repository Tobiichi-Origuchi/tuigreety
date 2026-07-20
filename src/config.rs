use std::{
  env,
  fmt,
  fs,
  io::{self, Write},
  ops::Range,
  os::unix::fs::MetadataExt,
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
const LOGIN_DEFS: &str = "/etc/login.defs";
const DEFAULT_MIN_UID: u32 = 1000;
const DEFAULT_MAX_UID: u32 = 60000;
const DEFAULT_LOG_FILE: &str = "/tmp/tuigreet.log";
const DEFAULT_XSESSION_WRAPPER: &str = "startx /usr/bin/env";

const CONFIG_SECTIONS: &[&str] = &[
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
const GENERAL_FIELDS: &[&str] = &["debug", "log-file", "ipc-timeout", "mock"];
const SESSION_FIELDS: &[&str] = &[
  "command",
  "allow-command-editor",
  "environment",
  "sessions",
  "xsessions",
  "wrapper",
  "xsession-wrapper",
];
const DISPLAY_FIELDS: &[&str] = &[
  "width",
  "issue",
  "greeting",
  "time",
  "time-format",
  "refresh-rate",
  "theme",
];
const REMEMBER_FIELDS: &[&str] = &["username", "session", "user-session"];
const USER_FIELDS: &[&str] = &["menu", "autocomplete", "min-uid", "max-uid"];
const SECRET_FIELDS: &[&str] = &["asterisks", "characters"];
const LAYOUT_FIELDS: &[&str] = &["window-padding", "container-padding", "prompt-padding", "greet-align"];
const POWER_FIELDS: &[&str] = &["shutdown", "reboot", "suspend", "hibernate", "setsid"];
const KEYBINDING_FIELDS: &[&str] = &["command", "sessions", "power"];
const THEME_FIELDS: &[&str] = &[
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
#[cfg(test)]
const CONFIG_SCHEMA: &[(&str, &[&str])] = &[
  ("general", GENERAL_FIELDS),
  ("session", SESSION_FIELDS),
  ("display", DISPLAY_FIELDS),
  ("remember", REMEMBER_FIELDS),
  ("users", USER_FIELDS),
  ("secret", SECRET_FIELDS),
  ("layout", LAYOUT_FIELDS),
  ("power", POWER_FIELDS),
  ("keybindings", KEYBINDING_FIELDS),
  ("theme", THEME_FIELDS),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Severity {
  Warning,
  Error,
}

impl fmt::Display for Severity {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str(match self {
      Self::Warning => "warning",
      Self::Error => "error",
    })
  }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SourceMarker {
  line: usize,
  column: usize,
  source_line: String,
  underline_start: usize,
  underline_width: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DiagnosticSource {
  General,
  CommandLine,
  File {
    path: PathBuf,
    marker: Option<SourceMarker>,
  },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Diagnostic {
  severity: Severity,
  source: DiagnosticSource,
  field: Option<String>,
  title: &'static str,
  message: String,
}

impl Diagnostic {
  pub(crate) fn warning(message: impl Into<String>) -> Self {
    Self {
      severity: Severity::Warning,
      source: DiagnosticSource::General,
      field: None,
      title: "configuration warning",
      message: message.into(),
    }
  }

  fn command_line(field: Option<&str>, message: impl Into<String>) -> Self {
    Self {
      severity: Severity::Warning,
      source: DiagnosticSource::CommandLine,
      field: field.map(str::to_string),
      title: "invalid command-line configuration",
      message: message.into(),
    }
  }

  fn file(
    severity: Severity,
    title: &'static str,
    path: &Path,
    source: Option<&str>,
    span: Option<Range<usize>>,
    field: Option<&str>,
    message: impl Into<String>,
  ) -> Self {
    Self {
      severity,
      source: DiagnosticSource::File {
        path: path.to_path_buf(),
        marker: source.zip(span).map(|(source, span)| source_marker(source, span)),
      },
      field: field.map(str::to_string),
      title,
      message: message.into(),
    }
  }

  #[cfg(test)]
  fn contains(&self, needle: &str) -> bool {
    self.to_string().contains(needle)
  }
}

impl fmt::Display for Diagnostic {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match &self.source {
      DiagnosticSource::General => write!(formatter, "{}: {}", self.severity, self.message),
      DiagnosticSource::CommandLine => {
        write!(formatter, "{}: command line", self.severity)?;
        if let Some(field) = &self.field {
          write!(formatter, " ({field})")?;
        }
        write!(formatter, ": {}", self.message)
      },
      DiagnosticSource::File { path, marker } => {
        write!(formatter, "{}: {}", self.severity, self.title)?;
        if let Some(field) = &self.field {
          write!(formatter, " for `{field}`")?;
        }
        if let Some(marker) = marker {
          let gutter = marker.line.to_string().len();
          write!(
            formatter,
            "\n{space:>gutter$}--> {path}:{line}:{column}\n{space:>gutter$} |\n{line:>gutter$} | {source_line}\n{space:>gutter$} | {padding}{carets} {message}",
            space = "",
            path = path.display(),
            line = marker.line,
            column = marker.column,
            source_line = marker.source_line,
            padding = " ".repeat(marker.underline_start),
            carets = "^".repeat(marker.underline_width),
            message = self.message,
          )
        } else {
          write!(formatter, "\n  --> {}\n   = {}", path.display(), self.message)
        }
      },
    }
  }
}

#[derive(Clone, Copy)]
enum LayerContext<'a> {
  CommandLine,
  File { path: &'a Path, source: &'a str },
}

impl LayerContext<'_> {
  fn warning(self, span: Option<Range<usize>>, field: Option<&str>, message: impl Into<String>) -> Diagnostic {
    match self {
      Self::CommandLine => Diagnostic::command_line(field, message),
      Self::File { path, source } => Diagnostic::file(
        Severity::Warning,
        "invalid configuration",
        path,
        Some(source),
        span,
        field,
        message,
      ),
    }
  }
}

fn source_marker(source: &str, span: Range<usize>) -> SourceMarker {
  let offset = span.start.min(source.len());
  let (line, column) = line_column(source, offset);
  let line_start = source[..offset].rfind('\n').map_or(0, |index| index + 1);
  let line_end = source[offset..].find('\n').map_or(source.len(), |index| offset + index);
  let underline_start = source[line_start..offset].chars().count();
  let underline_end = span.end.min(line_end);
  let underline_width = source[offset..underline_end].chars().count().max(1);

  SourceMarker {
    line,
    column,
    source_line: source[line_start..line_end].to_string(),
    underline_start,
    underline_width,
  }
}

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
  pub(crate) uid_defaults: (u32, u32),
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
      uid_defaults: (DEFAULT_MIN_UID, DEFAULT_MAX_UID),
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

impl Settings {
  pub(crate) fn effective_uid_range(&self) -> (u32, u32) {
    (
      self.min_uid.unwrap_or(self.uid_defaults.0),
      self.max_uid.unwrap_or(self.uid_defaults.1),
    )
  }
}

#[derive(Default)]
struct Layer {
  spans: LayerSpans,
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
struct LayerSpans {
  display_identity: Option<Range<usize>>,
  uid_range: Option<Range<usize>>,
  remember_sessions: Option<Range<usize>>,
  keybindings: Option<Range<usize>>,
}

#[derive(Default)]
struct ThemeLayer {
  border: Option<ThemeColor>,
  text: Option<ThemeColor>,
  time: Option<ThemeColor>,
  container: Option<ThemeColor>,
  title: Option<ThemeColor>,
  greet: Option<ThemeColor>,
  prompt: Option<ThemeColor>,
  input: Option<ThemeColor>,
  action: Option<ThemeColor>,
  button: Option<ThemeColor>,
}

pub(crate) fn load_from(system: Option<&Path>, matches: &Matches) -> (Settings, Vec<Diagnostic>) {
  let explicit = matches.opt_str("config").map(PathBuf::from);
  load_paths_core(
    system,
    explicit.as_deref(),
    matches,
    read_uid_defaults(Path::new(LOGIN_DEFS)),
    true,
  )
}

pub fn reload(matches: &Matches) -> Result<(Settings, Vec<Diagnostic>), Vec<Diagnostic>> {
  let explicit = matches.opt_str("config").map(PathBuf::from);
  reload_paths_core(
    Path::new(SYSTEM_CONFIG),
    explicit.as_deref(),
    matches,
    read_uid_defaults(Path::new(LOGIN_DEFS)),
    true,
  )
}

#[cfg(test)]
fn reload_paths(
  system: &Path,
  explicit: Option<&Path>,
  matches: &Matches,
) -> Result<(Settings, Vec<Diagnostic>), Vec<Diagnostic>> {
  reload_paths_core(
    system,
    explicit,
    matches,
    read_uid_defaults(Path::new(LOGIN_DEFS)),
    false,
  )
}

fn reload_paths_core(
  system: &Path,
  explicit: Option<&Path>,
  matches: &Matches,
  uid_defaults: (u32, u32),
  check_trust: bool,
) -> Result<(Settings, Vec<Diagnostic>), Vec<Diagnostic>> {
  let mut settings = Settings {
    uid_defaults,
    ..Settings::default()
  };
  let mut warnings = Vec::new();
  let duplicate_explicit = explicit.is_some_and(|explicit| same_config_file(system, explicit));

  if !load_optional_strict(system, &mut settings, &mut warnings, check_trust) {
    return Err(warnings);
  }
  if let Some(path) = explicit
    && !duplicate_explicit
    && !load_required(path, &mut settings, &mut warnings, check_trust)
  {
    return Err(warnings);
  }

  apply_layer(
    &mut settings,
    cli_layer(matches, &mut warnings),
    LayerContext::CommandLine,
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

pub(crate) fn watched_paths(matches: &Matches) -> Vec<PathBuf> {
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

#[cfg(test)]
fn load_paths(system: Option<&Path>, explicit: Option<&Path>, matches: &Matches) -> (Settings, Vec<Diagnostic>) {
  load_paths_core(
    system,
    explicit,
    matches,
    read_uid_defaults(Path::new(LOGIN_DEFS)),
    false,
  )
}

#[cfg(test)]
fn load_paths_with_uid_defaults(
  system: Option<&Path>,
  explicit: Option<&Path>,
  matches: &Matches,
  uid_defaults: (u32, u32),
) -> (Settings, Vec<Diagnostic>) {
  load_paths_core(system, explicit, matches, uid_defaults, false)
}

fn load_paths_core(
  system: Option<&Path>,
  explicit: Option<&Path>,
  matches: &Matches,
  uid_defaults: (u32, u32),
  check_trust: bool,
) -> (Settings, Vec<Diagnostic>) {
  let mut settings = Settings {
    uid_defaults,
    ..Settings::default()
  };
  let mut warnings = Vec::new();
  let duplicate_explicit = system
    .zip(explicit)
    .is_some_and(|(system, explicit)| same_config_file(system, explicit));

  if let Some(path) = system {
    load_optional(path, &mut settings, &mut warnings, check_trust);
  }
  if let Some(path) = explicit
    && !duplicate_explicit
  {
    load_required(path, &mut settings, &mut warnings, check_trust);
  }

  apply_layer(
    &mut settings,
    cli_layer(matches, &mut warnings),
    LayerContext::CommandLine,
    &mut warnings,
  );
  (settings, warnings)
}

fn same_config_file(left: &Path, right: &Path) -> bool {
  if left == right {
    return true;
  }
  match (fs::metadata(left), fs::metadata(right)) {
    (Ok(left), Ok(right)) => left.dev() == right.dev() && left.ino() == right.ino(),
    _ => false,
  }
}

fn read_uid_defaults(path: &Path) -> (u32, u32) {
  let fallback = (DEFAULT_MIN_UID, DEFAULT_MAX_UID);
  let Ok(source) = fs::read_to_string(path) else {
    return fallback;
  };

  let mut parsed = (None, None);
  for line in source.lines() {
    let mut fields = line.split_whitespace();
    match (fields.next(), fields.next()) {
      (Some("UID_MIN"), Some(value)) => {
        if let Ok(value) = value.parse() {
          parsed.0 = Some(value);
        }
      },
      (Some("UID_MAX"), Some(value)) => {
        if let Ok(value) = value.parse() {
          parsed.1 = Some(value);
        }
      },
      _ => {},
    }
  }

  let candidate = (parsed.0.unwrap_or(fallback.0), parsed.1.unwrap_or(fallback.1));
  if candidate.0 <= candidate.1 {
    candidate
  } else {
    fallback
  }
}

fn config_trust_message(uid: u32, mode: u32) -> Option<String> {
  let permissions = mode & 0o7777;
  let mut problems = Vec::new();
  if uid != 0 {
    problems.push(format!("owned by UID {uid} instead of root (UID 0)"));
  }
  if permissions & 0o022 != 0 {
    problems.push(format!("writable by group or other users (mode {permissions:#06o})"));
  }
  (!problems.is_empty()).then(|| {
    format!(
      "{}; this file can select commands executed after authentication, so keep it root-owned and remove group/other write permissions",
      problems.join(" and ")
    )
  })
}

fn warn_config_trust(path: &Path, warnings: &mut Vec<Diagnostic>) {
  match fs::metadata(path) {
    Ok(metadata) => {
      if let Some(message) = config_trust_message(metadata.uid(), metadata.mode()) {
        warnings.push(Diagnostic::file(
          Severity::Warning,
          "unsafe configuration ownership or permissions",
          path,
          None,
          None,
          None,
          message,
        ));
      }
    },
    Err(error) => warnings.push(Diagnostic::file(
      Severity::Warning,
      "cannot verify configuration ownership or permissions",
      path,
      None,
      None,
      None,
      error.to_string(),
    )),
  }
}

fn load_optional(path: &Path, settings: &mut Settings, warnings: &mut Vec<Diagnostic>, check_trust: bool) {
  match path.try_exists() {
    Ok(true) => {
      load_required(path, settings, warnings, check_trust);
    },
    Ok(false) => {},
    Err(error) => warnings.push(Diagnostic::file(
      Severity::Warning,
      "cannot access configuration",
      path,
      None,
      None,
      None,
      error.to_string(),
    )),
  }
}

fn load_optional_strict(
  path: &Path,
  settings: &mut Settings,
  warnings: &mut Vec<Diagnostic>,
  check_trust: bool,
) -> bool {
  match path.try_exists() {
    Ok(true) => load_required(path, settings, warnings, check_trust),
    Ok(false) => true,
    Err(error) => {
      warnings.push(Diagnostic::file(
        Severity::Warning,
        "cannot access configuration",
        path,
        None,
        None,
        None,
        error.to_string(),
      ));
      false
    },
  }
}

fn load_required(path: &Path, settings: &mut Settings, warnings: &mut Vec<Diagnostic>, check_trust: bool) -> bool {
  let content = match fs::read_to_string(path) {
    Ok(content) => content,
    Err(error) => {
      warnings.push(Diagnostic::file(
        Severity::Warning,
        "cannot read configuration",
        path,
        None,
        None,
        None,
        error.to_string(),
      ));
      return false;
    },
  };
  if check_trust {
    warn_config_trust(path, warnings);
  }
  let document = match content.parse::<Document<String>>() {
    Ok(document) => document,
    Err(error) => {
      warnings.push(toml_diagnostic(path, &content, &error));
      return false;
    },
  };

  let layer = toml_layer(&document, path, &content, warnings);
  apply_layer(settings, layer, LayerContext::File { path, source: &content }, warnings);
  true
}

fn toml_diagnostic(path: &Path, source: &str, error: &toml_edit::TomlError) -> Diagnostic {
  Diagnostic::file(
    Severity::Error,
    "invalid TOML",
    path,
    Some(source),
    error.span(),
    None,
    error.message(),
  )
}

fn apply_layer(settings: &mut Settings, layer: Layer, context: LayerContext<'_>, warnings: &mut Vec<Diagnostic>) {
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
    warnings.push(context.warning(
      layer.spans.display_identity.clone(),
      Some("display.issue/display.greeting"),
      "the fields conflict; using the greeting",
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
  let effective_min = proposed_min.unwrap_or(settings.uid_defaults.0);
  let effective_max = proposed_max.unwrap_or(settings.uid_defaults.1);
  if effective_min > effective_max {
    warnings.push(context.warning(
      layer.spans.uid_range.clone(),
      Some("users.min-uid/users.max-uid"),
      format!(
        "users.min-uid exceeds users.max-uid after applying this layer ({effective_min} > {effective_max}); ignoring UID fields from this layer"
      ),
    ));
  } else {
    settings.min_uid = proposed_min;
    settings.max_uid = proposed_max;
  }

  let proposed_session = layer.remember_session.unwrap_or(settings.remember_session);
  let proposed_user_session = layer.remember_user_session.unwrap_or(settings.remember_user_session);
  if proposed_session && proposed_user_session {
    warnings.push(context.warning(
      layer.spans.remember_sessions.clone(),
      Some("remember.session/remember.user-session"),
      "the fields cannot both be true; ignoring both fields from this layer",
    ));
  } else {
    settings.remember_session = proposed_session;
    settings.remember_user_session = proposed_user_session;
  }
  if settings.remember_user_session && !settings.remember {
    warnings.push(context.warning(
      layer.spans.remember_sessions.clone(),
      Some("remember.user-session"),
      "remember.user-session requires remember.username; enabling remember.username",
    ));
    settings.remember = true;
  }

  let key_layer = [layer.kb_command, layer.kb_sessions, layer.kb_power];
  if key_layer.iter().any(Option::is_some) {
    let keys = [
      layer.kb_command.unwrap_or(settings.kb_command),
      layer.kb_sessions.unwrap_or(settings.kb_sessions),
      layer.kb_power.unwrap_or(settings.kb_power),
    ];
    if keys[0] == keys[1] || keys[0] == keys[2] || keys[1] == keys[2] {
      warnings.push(context.warning(
        layer.spans.keybindings.clone(),
        Some("keybindings"),
        format!(
          "duplicate keybindings in candidate (command=F{}, sessions=F{}, power=F{}); ignoring all keybinding fields from this layer",
          keys[0], keys[1], keys[2]
        ),
      ));
    } else {
      [settings.kb_command, settings.kb_sessions, settings.kb_power] = keys;
    }
  }
}

fn cli_layer(matches: &Matches, warnings: &mut Vec<Diagnostic>) -> Layer {
  let mut layer = Layer::default();
  let string = |name: &str| matches.opt_str(name);

  layer.debug = cli_bool(matches, "debug", "no-debug", warnings);
  if let Some(path) = string("debug") {
    layer.logfile = Some(path);
  }
  layer.ipc_timeout = cli_number(matches, "ipc-timeout", 1, MAX_IPC_TIMEOUT, warnings);
  layer.command = string("cmd").map(optional_command);
  layer.allow_command_editor = cli_bool(matches, "allow-command-editor", "no-command-editor", warnings);
  if matches.opt_present("env") {
    layer.environment = Some(valid_environment(
      matches.opt_strs("env"),
      LayerContext::CommandLine,
      None,
      warnings,
    ));
  }
  layer.sessions = string("sessions").map(|value| split_paths(&value));
  layer.xsessions = string("xsessions").map(|value| split_paths(&value));
  layer.session_wrapper = string("session-wrapper").map(optional_command);
  layer.xsession_wrapper = if matches.opt_present("no-xsession-wrapper") {
    Some(None)
  } else {
    string("xsession-wrapper").map(optional_command)
  };
  layer.width = cli_number(matches, "width", 1, u16::MAX, warnings);
  layer.issue = cli_bool(matches, "issue", "no-issue", warnings);
  layer.greeting = string("greeting").map(Some);
  layer.time = cli_bool(matches, "time", "no-time", warnings);
  layer.time_format = string("time-format").and_then(|value| {
    valid_time_format(&value, LayerContext::CommandLine, None, "--time-format", warnings).then_some(Some(value))
  });
  layer.refresh_rate = cli_number(matches, "refresh-rate", 1, MAX_REFRESH_RATE, warnings);
  layer.remember = cli_bool(matches, "remember", "no-remember", warnings);
  layer.remember_session = cli_bool(matches, "remember-session", "no-remember-session", warnings);
  layer.remember_user_session = cli_bool(matches, "remember-user-session", "no-remember-user-session", warnings);
  layer.user_menu = cli_bool(matches, "user-menu", "no-user-menu", warnings);
  layer.user_autocomplete = cli_bool(matches, "user-autocomplete", "no-user-autocomplete", warnings);
  layer.min_uid = cli_number::<u32>(matches, "user-menu-min-uid", 0, u32::MAX, warnings).map(Some);
  layer.max_uid = cli_number::<u32>(matches, "user-menu-max-uid", 0, u32::MAX, warnings).map(Some);
  if let Some(specification) = string("theme") {
    layer.theme = parse_theme_layer(&specification, LayerContext::CommandLine, None, "--theme", warnings);
  }
  layer.asterisks = cli_bool(matches, "asterisks", "no-asterisks", warnings);
  if let Some(value) = string("asterisks-char") {
    if value.is_empty() {
      warnings.push(Diagnostic::command_line(
        Some("--asterisks-char"),
        "the value is empty; ignoring it",
      ));
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
      warnings.push(Diagnostic::command_line(
        Some("--greet-align"),
        format!("invalid value '{value}'; expected left, center, or right"),
      ));
    }
  }
  layer.power_shutdown = cli_power_command(matches, "power-shutdown", warnings);
  layer.power_reboot = cli_power_command(matches, "power-reboot", warnings);
  layer.power_suspend = cli_power_command(matches, "power-suspend", warnings);
  layer.power_hibernate = cli_power_command(matches, "power-hibernate", warnings);
  layer.power_setsid = cli_bool(matches, "power-setsid", "power-no-setsid", warnings);
  layer.mock = cli_bool(matches, "mock", "no-mock", warnings);
  layer.kb_command = cli_number(matches, "kb-command", 1, 12, warnings);
  layer.kb_sessions = cli_number(matches, "kb-sessions", 1, 12, warnings);
  layer.kb_power = cli_number(matches, "kb-power", 1, 12, warnings);
  layer
}

fn cli_bool(matches: &Matches, enable: &str, disable: &str, warnings: &mut Vec<Diagnostic>) -> Option<bool> {
  let enabled = matches.opt_present(enable);
  let disabled = matches.opt_present(disable);
  if enabled && disabled {
    warnings.push(Diagnostic::command_line(
      Some(&format!("--{enable}/--{disable}")),
      format!("the options conflict; --{disable} takes precedence"),
    ));
  }

  if disabled {
    Some(false)
  } else if enabled {
    Some(true)
  } else {
    None
  }
}

fn cli_power_command(matches: &Matches, name: &str, warnings: &mut Vec<Diagnostic>) -> Option<PowerCommand> {
  let value = matches.opt_str(name)?;
  match CommandLine::parse(&value) {
    Ok(command) => Some(PowerCommand::Explicit(command)),
    Err(error) => {
      warnings.push(Diagnostic::command_line(
        Some(&format!("--{name}")),
        format!("invalid value: {error}; ignoring it"),
      ));
      None
    },
  }
}

fn cli_number<T>(matches: &Matches, name: &str, min: T, max: T, warnings: &mut Vec<Diagnostic>) -> Option<T>
where
  T: std::str::FromStr + PartialOrd + Copy + std::fmt::Display,
{
  let value = matches.opt_str(name)?;
  match value.parse::<T>() {
    Ok(parsed) if parsed >= min && parsed <= max => Some(parsed),
    _ => {
      warnings.push(Diagnostic::command_line(
        Some(&format!("--{name}")),
        format!("invalid value '{value}'; expected {min}..={max}"),
      ));
      None
    },
  }
}

fn toml_layer(document: &Document<String>, path: &Path, source: &str, warnings: &mut Vec<Diagnostic>) -> Layer {
  warn_unknown(document.as_table(), CONFIG_SECTIONS, path, source, warnings, "");
  let mut layer = Layer::default();

  if let Some(table) = read_table(document.as_table(), "general", path, source, warnings) {
    warn_unknown(table, GENERAL_FIELDS, path, source, warnings, "general");
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
    warn_unknown(table, SESSION_FIELDS, path, source, warnings, "session");
    layer.command = read_optional_command(table, "command", path, source, warnings, "session");
    layer.allow_command_editor = read_bool(table, "allow-command-editor", path, source, warnings, "session");
    layer.environment = read_strings(table, "environment", path, source, warnings, "session").map(|values| {
      valid_environment(
        values,
        LayerContext::File { path, source },
        table.get("environment").and_then(Item::span),
        warnings,
      )
    });
    layer.sessions = read_strings(table, "sessions", path, source, warnings, "session");
    layer.xsessions = read_strings(table, "xsessions", path, source, warnings, "session");
    layer.session_wrapper = read_optional_command(table, "wrapper", path, source, warnings, "session");
    layer.xsession_wrapper = read_string_or_false(table, "xsession-wrapper", path, source, warnings, "session");
  }
  if let Some(table) = read_table(document.as_table(), "display", path, source, warnings) {
    layer.spans.display_identity = combined_item_span(table, &["issue", "greeting"]);
    warn_unknown(table, DISPLAY_FIELDS, path, source, warnings, "display");
    layer.width = read_u16(table, "width", (1, u16::MAX), path, source, warnings, "display");
    layer.issue = read_bool(table, "issue", path, source, warnings, "display");
    layer.greeting = read_optional_string(table, "greeting", path, source, warnings, "display");
    layer.time = read_bool(table, "time", path, source, warnings, "display");
    layer.time_format =
      read_optional_string(table, "time-format", path, source, warnings, "display").and_then(|value| {
        value.map_or(Some(None), |format| {
          valid_time_format(
            &format,
            LayerContext::File { path, source },
            table.get("time-format").and_then(Item::span),
            "display.time-format",
            warnings,
          )
          .then_some(Some(format))
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
      layer.theme = parse_theme_layer(
        &specification,
        LayerContext::File { path, source },
        table.get("theme").and_then(Item::span),
        "display.theme",
        warnings,
      );
    }
  }
  if let Some(table) = read_table(document.as_table(), "remember", path, source, warnings) {
    layer.spans.remember_sessions = combined_item_span(table, &["username", "session", "user-session"]);
    warn_unknown(table, REMEMBER_FIELDS, path, source, warnings, "remember");
    layer.remember = read_bool(table, "username", path, source, warnings, "remember");
    layer.remember_session = read_bool(table, "session", path, source, warnings, "remember");
    layer.remember_user_session = read_bool(table, "user-session", path, source, warnings, "remember");
  }
  if let Some(table) = read_table(document.as_table(), "users", path, source, warnings) {
    layer.spans.uid_range = combined_item_span(table, &["min-uid", "max-uid"]);
    warn_unknown(table, USER_FIELDS, path, source, warnings, "users");
    layer.user_menu = read_bool(table, "menu", path, source, warnings, "users");
    layer.user_autocomplete = read_bool(table, "autocomplete", path, source, warnings, "users");
    layer.min_uid = read_u32(table, "min-uid", path, source, warnings, "users").map(Some);
    layer.max_uid = read_u32(table, "max-uid", path, source, warnings, "users").map(Some);
  }
  if let Some(table) = read_table(document.as_table(), "secret", path, source, warnings) {
    warn_unknown(table, SECRET_FIELDS, path, source, warnings, "secret");
    layer.asterisks = read_bool(table, "asterisks", path, source, warnings, "secret");
    if let Some(value) = read_string(table, "characters", path, source, warnings, "secret") {
      if value.is_empty() {
        warn_field_item(
          table.get("characters"),
          path,
          source,
          warnings,
          "secret.characters",
          "secret.characters must not be empty",
        );
      } else {
        layer.asterisks_chars = Some(value);
      }
    }
  }
  if let Some(table) = read_table(document.as_table(), "layout", path, source, warnings) {
    warn_unknown(table, LAYOUT_FIELDS, path, source, warnings, "layout");
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
        warn_field_item(
          table.get("greet-align"),
          path,
          source,
          warnings,
          "layout.greet-align",
          "the value must be left, center, or right",
        );
      }
    }
  }
  if let Some(table) = read_table(document.as_table(), "power", path, source, warnings) {
    warn_unknown(table, POWER_FIELDS, path, source, warnings, "power");
    layer.power_shutdown = read_power_command(table, "shutdown", path, source, warnings);
    layer.power_reboot = read_power_command(table, "reboot", path, source, warnings);
    layer.power_suspend = read_power_command(table, "suspend", path, source, warnings);
    layer.power_hibernate = read_power_command(table, "hibernate", path, source, warnings);
    layer.power_setsid = read_bool(table, "setsid", path, source, warnings, "power");
  }
  if let Some(table) = read_table(document.as_table(), "keybindings", path, source, warnings) {
    layer.spans.keybindings = combined_item_span(table, &["command", "sessions", "power"]);
    warn_unknown(table, KEYBINDING_FIELDS, path, source, warnings, "keybindings");
    layer.kb_command = read_u8(table, "command", (1, 12), path, source, warnings, "keybindings");
    layer.kb_sessions = read_u8(table, "sessions", (1, 12), path, source, warnings, "keybindings");
    layer.kb_power = read_u8(table, "power", (1, 12), path, source, warnings, "keybindings");
  }
  if let Some(table) = read_table(document.as_table(), "theme", path, source, warnings) {
    warn_unknown(table, THEME_FIELDS, path, source, warnings, "theme");
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

fn combined_item_span(table: &Table, keys: &[&str]) -> Option<Range<usize>> {
  keys
    .iter()
    .filter_map(|key| table.get(key).and_then(Item::span))
    .reduce(|left, right| left.start.min(right.start)..left.end.max(right.end))
}

fn read_table<'a>(
  root: &'a Table,
  key: &str,
  path: &Path,
  source: &str,
  warnings: &mut Vec<Diagnostic>,
) -> Option<&'a Table> {
  let item = root.get(key)?;
  match item.as_table() {
    Some(table) => Some(table),
    None => {
      warn_field_item(Some(item), path, source, warnings, key, "the value must be a table");
      None
    },
  }
}

fn warn_unknown(
  table: &Table,
  allowed: &[&str],
  path: &Path,
  source: &str,
  warnings: &mut Vec<Diagnostic>,
  prefix: &str,
) {
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
        Some(&field),
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
      warnings: &mut Vec<Diagnostic>,
      prefix: &str,
    ) -> Option<$ty> {
      let item = table.get(key)?;
      match item.$method() {
        Some(value) => Some(value.into()),
        None => {
          let field = format!("{prefix}.{key}");
          warn_field_item(
            Some(item),
            path,
            source,
            warnings,
            &field,
            &format!("the value must be {}", $expected),
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
  warnings: &mut Vec<Diagnostic>,
  prefix: &str,
) -> Option<Option<String>> {
  let value = read_string(table, key, path, source, warnings, prefix)?;
  Some((!value.is_empty()).then_some(value))
}

fn read_optional_command(
  table: &Table,
  key: &str,
  path: &Path,
  source: &str,
  warnings: &mut Vec<Diagnostic>,
  prefix: &str,
) -> Option<Option<String>> {
  let value = read_string(table, key, path, source, warnings, prefix)?;
  Some(optional_command(value))
}

fn optional_command(value: String) -> Option<String> {
  (!value.trim().is_empty()).then_some(value)
}

fn read_power_command(
  table: &Table,
  key: &str,
  path: &Path,
  source: &str,
  warnings: &mut Vec<Diagnostic>,
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
        let field = format!("power.{key}");
        warn_field_item(
          Some(item),
          path,
          source,
          warnings,
          &field,
          &format!("power.{key} must contain only strings"),
        );
        return None;
      },
    }
  } else if let Some(value) = item.as_str() {
    CommandLine::parse(value)
  } else {
    let field = format!("power.{key}");
    warn_field_item(
      Some(item),
      path,
      source,
      warnings,
      &field,
      &format!("power.{key} must be an argument array, a legacy command string, or false"),
    );
    return None;
  };

  match parsed {
    Ok(command) => Some(PowerCommand::Explicit(command)),
    Err(error) => {
      let field = format!("power.{key}");
      warn_field_item(
        Some(item),
        path,
        source,
        warnings,
        &field,
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
  warnings: &mut Vec<Diagnostic>,
  prefix: &str,
) -> Option<Option<String>> {
  let item = table.get(key)?;
  if item.as_bool() == Some(false) {
    Some(None)
  } else if let Some(value) = item.as_str() {
    Some(optional_command(value.to_string()))
  } else {
    let field = format!("{prefix}.{key}");
    warn_field_item(
      Some(item),
      path,
      source,
      warnings,
      &field,
      "the value must be a command string or false",
    );
    None
  }
}

fn read_strings(
  table: &Table,
  key: &str,
  path: &Path,
  source: &str,
  warnings: &mut Vec<Diagnostic>,
  prefix: &str,
) -> Option<Vec<String>> {
  let item = table.get(key)?;
  let field = format!("{prefix}.{key}");
  let Some(array) = item.as_array() else {
    warn_field_item(
      Some(item),
      path,
      source,
      warnings,
      &field,
      "the value must be an array of strings",
    );
    return None;
  };
  let mut values = Vec::new();
  for value in array {
    if let Some(value) = value.as_str() {
      values.push(value.to_string());
    } else {
      warn_field_item(
        Some(item),
        path,
        source,
        warnings,
        &field,
        "the array contains a non-string value; ignoring it",
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
  warnings: &mut Vec<Diagnostic>,
  prefix: &str,
) -> Option<u64> {
  let (min, max) = bounds;
  let item = table.get(key)?;
  match item.as_integer().and_then(|value| u64::try_from(value).ok()) {
    Some(value) if (min..=max).contains(&value) => Some(value),
    _ => {
      let field = format!("{prefix}.{key}");
      warn_field_item(
        Some(item),
        path,
        source,
        warnings,
        &field,
        &format!("the value must be an integer in {min}..={max}"),
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
      warnings: &mut Vec<Diagnostic>,
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
  warnings: &mut Vec<Diagnostic>,
  prefix: &str,
) -> Option<u32> {
  read_integer(table, key, (0, u64::from(u32::MAX)), path, source, warnings, prefix).map(|value| value as u32)
}

fn warn_field_item(
  item: Option<&Item>,
  path: &Path,
  source: &str,
  warnings: &mut Vec<Diagnostic>,
  field: &str,
  message: &str,
) {
  warn_span(item.and_then(Item::span), path, source, warnings, Some(field), message);
}

fn warn_span(
  span: Option<Range<usize>>,
  path: &Path,
  source: &str,
  warnings: &mut Vec<Diagnostic>,
  field: Option<&str>,
  message: &str,
) {
  warnings.push(Diagnostic::file(
    Severity::Warning,
    "invalid configuration",
    path,
    Some(source),
    span,
    field,
    message,
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

fn valid_environment(
  values: Vec<String>,
  context: LayerContext<'_>,
  span: Option<Range<usize>>,
  warnings: &mut Vec<Diagnostic>,
) -> Vec<String> {
  values
    .into_iter()
    .filter(|value| {
      let valid = value.split_once('=').is_some_and(|(key, _)| !key.is_empty());
      if !valid {
        let field = match context {
          LayerContext::CommandLine => "--env",
          LayerContext::File { .. } => "session.environment",
        };
        warnings.push(context.warning(
          span.clone(),
          Some(field),
          format!("malformed environment entry '{value}'; ignoring it"),
        ));
      }
      valid
    })
    .collect()
}

fn valid_time_format(
  format: &str,
  context: LayerContext<'_>,
  span: Option<Range<usize>>,
  field: &str,
  warnings: &mut Vec<Diagnostic>,
) -> bool {
  use chrono::format::{Item as ChronoItem, StrftimeItems};

  if StrftimeItems::new(format).any(|item| item == ChronoItem::Error) {
    warnings.push(context.warning(span, Some(field), format!("invalid value '{format}'; ignoring it")));
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
  warnings: &mut Vec<Diagnostic>,
) -> Option<ThemeColor> {
  let item = table.get(key)?;
  if item.as_bool() == Some(false) {
    return Some(ThemeColor::Clear);
  }
  let Some(value) = item.as_str() else {
    let field = format!("theme.{key}");
    warn_field_item(
      Some(item),
      path,
      source,
      warnings,
      &field,
      "the value must be a color string or false",
    );
    return None;
  };
  if valid_color(value) {
    Some(ThemeColor::Value(value.to_string()))
  } else {
    let field = format!("theme.{key}");
    warn_field_item(
      Some(item),
      path,
      source,
      warnings,
      &field,
      &format!("invalid color '{value}'; ignoring it"),
    );
    None
  }
}

fn parse_theme_layer(
  specification: &str,
  context: LayerContext<'_>,
  span: Option<Range<usize>>,
  field: &str,
  warnings: &mut Vec<Diagnostic>,
) -> ThemeLayer {
  let mut theme = ThemeLayer::default();
  for directive in specification.split(';').filter(|directive| !directive.is_empty()) {
    let Some((key, value)) = directive.split_once('=') else {
      warnings.push(context.warning(
        span.clone(),
        Some(field),
        format!("malformed theme directive '{directive}'; ignoring it"),
      ));
      continue;
    };
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
        warnings.push(context.warning(
          span.clone(),
          Some(field),
          format!("unknown theme component '{key}'; ignoring it"),
        ));
        continue;
      },
    };
    if !valid_color(value) {
      warnings.push(context.warning(
        span.clone(),
        Some(field),
        format!("invalid color '{value}' for '{key}'; ignoring it"),
      ));
      continue;
    }
    *destination = Some(ThemeColor::Value(value.to_string()));
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
  use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
  };

  use ratatui::style::Color;
  use tempfile::tempdir;

  use super::{ThemeColor, load_paths, load_paths_with_uid_defaults, read_uid_defaults, reload_paths};
  use crate::{
    Greeter,
    power::{CommandLine, PowerCommand},
    ui::common::style::{Theme, Themed},
  };

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
    assert!(warnings.iter().any(|warning| {
      warning.contains("--allow-command-editor/--no-command-editor") && warning.contains("conflict")
    }));
  }

  #[test]
  fn paired_cli_booleans_override_configuration_in_both_directions() {
    let dir = tempdir().unwrap();
    let enabled = dir.path().join("enabled.toml");
    write(
      &enabled,
      "[general]\ndebug = true\nmock = true\n\
       [display]\nissue = true\ntime = true\n\
       [remember]\nusername = true\nsession = true\n\
       [users]\nmenu = true\nautocomplete = true\n\
       [secret]\nasterisks = true\n\
       [power]\nsetsid = false\n",
    );
    let (disabled, warnings) = load_paths(
      Some(&enabled),
      None,
      &matches(&[
        "--no-debug",
        "--no-mock",
        "--no-issue",
        "--no-time",
        "--no-remember",
        "--no-remember-session",
        "--no-user-menu",
        "--no-user-autocomplete",
        "--no-asterisks",
        "--power-setsid",
      ]),
    );
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(!disabled.debug);
    assert!(!disabled.mock);
    assert!(!disabled.issue);
    assert!(!disabled.time);
    assert!(!disabled.remember);
    assert!(!disabled.remember_session);
    assert!(!disabled.user_menu);
    assert!(!disabled.user_autocomplete);
    assert!(!disabled.asterisks);
    assert!(disabled.power_setsid);

    let user_session = dir.path().join("user-session.toml");
    write(&user_session, "[remember]\nusername = true\nuser-session = true\n");
    let (disabled, warnings) = load_paths(
      Some(&user_session),
      None,
      &matches(&["--no-remember", "--no-remember-user-session"]),
    );
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(!disabled.remember);
    assert!(!disabled.remember_user_session);

    let disabled_file = dir.path().join("disabled.toml");
    write(
      &disabled_file,
      "[general]\ndebug = false\nmock = false\n\
       [display]\nissue = false\ntime = false\n\
       [power]\nsetsid = true\n",
    );
    let (enabled, warnings) = load_paths(
      Some(&disabled_file),
      None,
      &matches(&["--debug", "--mock", "--issue", "--time", "--power-no-setsid"]),
    );
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(enabled.debug);
    assert!(enabled.mock);
    assert!(enabled.issue);
    assert!(enabled.time);
    assert!(!enabled.power_setsid);
  }

  #[test]
  fn negative_cli_boolean_wins_a_reported_conflict() {
    let (settings, warnings) = load_paths(None, None, &matches(&["--time", "--no-time", "--mock", "--no-mock"]));

    assert!(!settings.time);
    assert!(!settings.mock);
    assert_eq!(warnings.len(), 2);
    assert!(warnings.iter().all(|warning| warning.contains("takes precedence")));
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
  fn the_same_configuration_file_is_applied_only_once() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("config.toml");
    let alias = dir.path().join("alias.toml");
    write(&config, "[display]\ntime = true\nunknown = true\n");
    symlink(&config, &alias).unwrap();

    for explicit in [&config, &alias] {
      let (settings, warnings) = load_paths(Some(&config), Some(explicit), &matches(&[]));
      assert!(settings.time);
      assert_eq!(warnings.len(), 1, "{warnings:?}");
      assert!(warnings[0].contains("display.unknown"));
    }
  }

  #[test]
  fn blank_session_commands_clear_lower_layers_without_changing_power_arguments() {
    let dir = tempdir().unwrap();
    let system = dir.path().join("system.toml");
    let explicit = dir.path().join("explicit.toml");
    write(
      &system,
      "[session]\ncommand = 'system-command'\nwrapper = 'system-wrapper'\nxsession-wrapper = 'system-xwrapper'\n[power]\nshutdown = ['power-command', '']\n",
    );
    write(
      &explicit,
      "[session]\ncommand = '   '\nwrapper = \"\\t  \"\nxsession-wrapper = '  '\n",
    );

    let (from_toml, warnings) = load_paths(Some(&system), Some(&explicit), &matches(&[]));

    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(from_toml.command, None);
    assert_eq!(from_toml.session_wrapper, None);
    assert_eq!(from_toml.xsession_wrapper, None);
    let PowerCommand::Explicit(command) = from_toml.power_shutdown else {
      panic!("unrelated power command was not preserved");
    };
    assert_eq!(command.argv(), ["power-command", ""]);

    write(
      &explicit,
      "[session]\ncommand = ''\nwrapper = ''\nxsession-wrapper = ''\n",
    );
    let (from_empty_toml, warnings) = load_paths(Some(&system), Some(&explicit), &matches(&[]));

    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(from_empty_toml.command, None);
    assert_eq!(from_empty_toml.session_wrapper, None);
    assert_eq!(from_empty_toml.xsession_wrapper, None);

    let cli = matches(&["--cmd", "  ", "--session-wrapper", "\t", "--xsession-wrapper", " \n "]);
    let (from_cli, warnings) = load_paths(Some(&system), None, &cli);

    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(from_cli.command, None);
    assert_eq!(from_cli.session_wrapper, None);
    assert_eq!(from_cli.xsession_wrapper, None);
    let PowerCommand::Explicit(command) = from_cli.power_shutdown else {
      panic!("unrelated power command was not preserved");
    };
    assert_eq!(command.argv(), ["power-command", ""]);
  }

  #[test]
  fn nonblank_session_commands_preserve_their_original_whitespace() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("config.toml");
    write(
      &config,
      "[session]\ncommand = '  toml-command --flag  '\nwrapper = '  toml-wrapper  '\nxsession-wrapper = '  toml-xwrapper  '\n",
    );

    let (from_toml, warnings) = load_paths(Some(&config), None, &matches(&[]));

    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(from_toml.command.as_deref(), Some("  toml-command --flag  "));
    assert_eq!(from_toml.session_wrapper.as_deref(), Some("  toml-wrapper  "));
    assert_eq!(from_toml.xsession_wrapper.as_deref(), Some("  toml-xwrapper  "));

    let cli = matches(&[
      "--cmd",
      "  cli-command --flag  ",
      "--session-wrapper",
      "  cli-wrapper  ",
      "--xsession-wrapper",
      "  cli-xwrapper  ",
    ]);
    let (from_cli, warnings) = load_paths(None, None, &cli);

    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(from_cli.command.as_deref(), Some("  cli-command --flag  "));
    assert_eq!(from_cli.session_wrapper.as_deref(), Some("  cli-wrapper  "));
    assert_eq!(from_cli.xsession_wrapper.as_deref(), Some("  cli-xwrapper  "));
  }

  #[test]
  fn toml_keybinding_layer_accepts_an_atomic_swap() {
    let dir = tempdir().unwrap();
    let system = dir.path().join("system.toml");
    let explicit = dir.path().join("explicit.toml");
    write(&system, "[keybindings]\ncommand = 1\nsessions = 2\npower = 3\n");
    write(&explicit, "[keybindings]\ncommand = 2\nsessions = 1\n");

    let (settings, warnings) = load_paths(Some(&system), Some(&explicit), &matches(&[]));

    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(
      (settings.kb_command, settings.kb_sessions, settings.kb_power),
      (2, 1, 3)
    );
  }

  #[test]
  fn cli_keybinding_layer_accepts_an_atomic_cycle() {
    let dir = tempdir().unwrap();
    let system = dir.path().join("system.toml");
    write(&system, "[keybindings]\ncommand = 1\nsessions = 2\npower = 3\n");
    let cli = matches(&["--kb-command", "2", "--kb-sessions", "3", "--kb-power", "1"]);

    let (settings, warnings) = load_paths(Some(&system), None, &cli);

    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(
      (settings.kb_command, settings.kb_sessions, settings.kb_power),
      (2, 3, 1)
    );
  }

  #[test]
  fn duplicate_keybinding_candidates_roll_back_the_whole_layer() {
    let dir = tempdir().unwrap();
    let system = dir.path().join("system.toml");
    let explicit = dir.path().join("explicit.toml");
    write(&system, "[keybindings]\ncommand = 1\nsessions = 2\npower = 3\n");
    // The unchanged power binding participates in validation. Although
    // command=4 is independently valid, sessions=3 conflicts with power=3,
    // so neither update may be committed.
    write(&explicit, "[keybindings]\ncommand = 4\nsessions = 3\n");

    let (from_toml, warnings) = load_paths(Some(&system), Some(&explicit), &matches(&[]));

    assert_eq!(
      (from_toml.kb_command, from_toml.kb_sessions, from_toml.kb_power),
      (1, 2, 3)
    );
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("command=F4, sessions=F3, power=F3"));
    assert!(warnings[0].contains("ignoring all keybinding fields"));

    let cli = matches(&["--kb-command", "4", "--kb-sessions", "3"]);
    let (from_cli, warnings) = load_paths(Some(&system), None, &cli);

    assert_eq!(
      (from_cli.kb_command, from_cli.kb_sessions, from_cli.kb_power),
      (1, 2, 3)
    );
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("command line"));
    assert!(warnings[0].contains("ignoring all keybinding fields"));
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
    assert!(warnings.iter().all(|warning| !warning.contains("warning: warning:")));
  }

  #[test]
  fn unsafe_configuration_permissions_warn_without_discarding_values() {
    assert!(super::config_trust_message(0, 0o100644).is_none());
    let message = super::config_trust_message(1000, 0o100664).unwrap();
    assert!(message.contains("UID 1000"));
    assert!(message.contains("mode 0o0664"));

    let dir = tempdir().unwrap();
    let config = dir.path().join("config.toml");
    write(&config, "[display]\ntime = true\n");
    fs::set_permissions(&config, fs::Permissions::from_mode(0o666)).unwrap();

    let (settings, warnings) = super::load_paths_core(Some(&config), None, &matches(&[]), (1000, 60000), true);

    assert!(settings.time);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("unsafe configuration ownership or permissions"));
    assert!(warnings[0].contains("root-owned"));
    assert!(warnings[0].contains("group/other write permissions"));
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
    assert!(
      warnings
        .iter()
        .any(|warning| warning.contains("min-uid exceeds") && warning.contains("explicit.toml:2:"))
    );
    assert!(warnings.iter().any(|warning| warning.contains("duplicate keybinding")));
  }

  #[test]
  fn login_defs_uses_valid_bounds_and_falls_back_as_a_pair() {
    let dir = tempdir().unwrap();
    let login_defs = dir.path().join("login.defs");

    assert_eq!(read_uid_defaults(&login_defs), (1000, 60000));

    write(
      &login_defs,
      "# system accounts\nUID_MIN 500\nUID_MAX 65000\nUID_MIN invalid\n",
    );
    assert_eq!(read_uid_defaults(&login_defs), (500, 65000));

    write(&login_defs, "UID_MIN 70000\nUID_MAX 60000\n");
    assert_eq!(read_uid_defaults(&login_defs), (1000, 60000));

    write(&login_defs, "UID_MAX 999\n");
    assert_eq!(read_uid_defaults(&login_defs), (1000, 60000));
  }

  #[test]
  fn one_sided_uid_layers_are_validated_against_effective_defaults() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("config.toml");

    write(&config, "[users]\nmin-uid = 70000\n");
    let (invalid_min, warnings) = load_paths_with_uid_defaults(Some(&config), None, &matches(&[]), (1000, 60000));
    assert_eq!((invalid_min.min_uid, invalid_min.max_uid), (None, None));
    assert_eq!(invalid_min.effective_uid_range(), (1000, 60000));
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("min-uid exceeds users.max-uid"));
    assert!(warnings[0].contains("70000 > 60000"));

    write(&config, "[users]\nmax-uid = 999\n");
    let (invalid_max, warnings) = load_paths_with_uid_defaults(Some(&config), None, &matches(&[]), (1000, 60000));
    assert_eq!((invalid_max.min_uid, invalid_max.max_uid), (None, None));
    assert_eq!(invalid_max.effective_uid_range(), (1000, 60000));
    assert_eq!(warnings.len(), 1, "{warnings:?}");

    write(&config, "[users]\nmin-uid = 2000\n");
    let (valid_min, warnings) = load_paths_with_uid_defaults(Some(&config), None, &matches(&[]), (1000, 60000));
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(valid_min.effective_uid_range(), (2000, 60000));

    write(&config, "[users]\nmax-uid = 50000\n");
    let (valid_max, warnings) = load_paths_with_uid_defaults(Some(&config), None, &matches(&[]), (1000, 60000));
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(valid_max.effective_uid_range(), (1000, 50000));
  }

  #[test]
  fn invalid_higher_uid_layer_preserves_the_lower_pair() {
    let dir = tempdir().unwrap();
    let system = dir.path().join("system.toml");
    let explicit = dir.path().join("explicit.toml");
    write(&system, "[users]\nmin-uid = 2000\nmax-uid = 50000\n");

    write(&explicit, "[users]\nmin-uid = 60000\n");
    let (invalid_min, warnings) =
      load_paths_with_uid_defaults(Some(&system), Some(&explicit), &matches(&[]), (1000, 60000));
    assert_eq!((invalid_min.min_uid, invalid_min.max_uid), (Some(2000), Some(50000)));
    assert_eq!(warnings.len(), 1, "{warnings:?}");

    write(&explicit, "[users]\nmax-uid = 1000\n");
    let (invalid_max, warnings) =
      load_paths_with_uid_defaults(Some(&system), Some(&explicit), &matches(&[]), (1000, 60000));
    assert_eq!((invalid_max.min_uid, invalid_max.max_uid), (Some(2000), Some(50000)));
    assert_eq!(warnings.len(), 1, "{warnings:?}");
  }

  #[test]
  fn one_configuration_revision_keeps_its_resolved_uid_defaults() {
    let dir = tempdir().unwrap();
    let login_defs = dir.path().join("login.defs");
    write(&login_defs, "UID_MIN 1500\nUID_MAX 55000\n");

    let (settings, warnings) = load_paths_with_uid_defaults(None, None, &matches(&[]), read_uid_defaults(&login_defs));
    assert!(warnings.is_empty(), "{warnings:?}");

    write(&login_defs, "UID_MIN 3000\nUID_MAX 40000\n");
    assert_eq!(settings.effective_uid_range(), (1500, 55000));
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
    assert!(warnings.iter().any(|warning| warning.contains("command line")
      && warning.contains("--power-shutdown")
      && warning.contains("invalid value")));
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
    assert!(warnings.iter().all(|warning| warning.contains("session.environment")));
    assert!(warnings.iter().all(|warning| warning.contains("config.toml:2:")));
    assert!(warnings.iter().all(|warning| warning.contains("2 | environment =")));
  }

  #[test]
  fn distributed_example_is_complete_and_valid() {
    let source = include_str!("../contrib/tuigreet.toml");
    let mut uncommented = String::new();
    let mut documented = BTreeSet::new();
    let mut section = None;
    for line in source.lines() {
      let trimmed = line.trim();
      if let Some(name) = trimmed.strip_prefix('[').and_then(|line| line.strip_suffix(']')) {
        section = Some(name);
      }
      let assignment = trimmed.strip_prefix("# ").and_then(|line| {
        let (key, _) = line.split_once(" = ")?;
        key
          .chars()
          .all(|character| character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-')
          .then_some((key, line))
      });
      if let Some((key, assignment)) = assignment {
        let section = section.expect("documented assignment before a section");
        documented.insert(format!("{section}.{key}"));
        uncommented.push_str(assignment);
      } else {
        uncommented.push_str(line);
      }
      uncommented.push('\n');
    }

    let expected_fields = super::CONFIG_SCHEMA
      .iter()
      .flat_map(|(section, fields)| fields.iter().map(move |field| format!("{section}.{field}")))
      .collect::<BTreeSet<_>>();
    assert_eq!(
      documented, expected_fields,
      "the distributed example and parser schema differ"
    );

    let document = uncommented.parse::<toml_edit::Document<String>>().unwrap();
    let mut warnings = Vec::new();
    let layer = super::toml_layer(
      &document,
      Path::new("contrib/tuigreet.toml"),
      &uncommented,
      &mut warnings,
    );
    let mut settings = super::Settings::default();
    super::apply_layer(
      &mut settings,
      layer,
      super::LayerContext::File {
        path: Path::new("contrib/tuigreet.toml"),
        source: &uncommented,
      },
      &mut warnings,
    );

    assert!(warnings.is_empty(), "{warnings:?}");
    let command = |argv: &[&str]| {
      PowerCommand::Explicit(
        CommandLine::from_argv(argv.iter().map(|argument| (*argument).to_string()).collect()).unwrap(),
      )
    };
    assert_eq!(settings, super::Settings {
      debug: false,
      logfile: "/tmp/tuigreet.log".into(),
      ipc_timeout: 120,
      command: Some("sway".into()),
      allow_command_editor: false,
      environment: vec!["XDG_CURRENT_DESKTOP=sway".into()],
      sessions: Vec::new(),
      xsessions: Vec::new(),
      session_wrapper: Some("dbus-run-session".into()),
      xsession_wrapper: Some("startx /usr/bin/env".into()),
      width: 80,
      issue: false,
      greeting: Some("Welcome".into()),
      time: false,
      time_format: Some("%Y-%m-%d %H:%M".into()),
      refresh_rate: 2,
      remember: false,
      remember_session: false,
      remember_user_session: false,
      user_menu: false,
      user_autocomplete: false,
      min_uid: Some(1000),
      max_uid: Some(60000),
      uid_defaults: (1000, 60000),
      theme: super::ThemeSettings {
        border: super::ThemeColor::Value("blue".into()),
        text: super::ThemeColor::Value("white".into()),
        time: super::ThemeColor::Value("white".into()),
        container: super::ThemeColor::Value("black".into()),
        title: super::ThemeColor::Value("blue".into()),
        greet: super::ThemeColor::Value("white".into()),
        prompt: super::ThemeColor::Value("white".into()),
        input: super::ThemeColor::Value("white".into()),
        action: super::ThemeColor::Value("white".into()),
        button: super::ThemeColor::Value("white".into()),
      },
      asterisks: false,
      asterisks_chars: "*".into(),
      window_padding: 0,
      container_padding: 1,
      prompt_padding: 1,
      greet_align: "center".into(),
      power_shutdown: command(&["shutdown", "-h", "now"]),
      power_reboot: command(&["shutdown", "-r", "now"]),
      power_suspend: command(&["systemctl", "suspend"]),
      power_hibernate: command(&["systemctl", "hibernate"]),
      power_setsid: true,
      mock: false,
      kb_command: 2,
      kb_sessions: 3,
      kb_power: 12,
    });
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
    assert_eq!(settings.theme.border, ThemeColor::Clear);
    assert_eq!(settings.theme.text, ThemeColor::Value("white".into()));
    assert_eq!(settings.theme.prompt, ThemeColor::Value("green".into()));
  }

  #[test]
  fn invalid_theme_fields_do_not_replace_valid_colors() {
    let dir = tempdir().unwrap();
    let system = dir.path().join("system.toml");
    let explicit = dir.path().join("explicit.toml");
    write(&system, "[theme]\nborder = 'blue'\n");
    write(&explicit, "[theme]\nborder = 'not-a-color'\nunknown = 'red'\n");

    let (settings, warnings) = load_paths(Some(&system), Some(&explicit), &matches(&[]));

    assert_eq!(settings.theme.border, ThemeColor::Value("blue".into()));
    assert!(warnings.iter().any(|warning| warning.contains("invalid color")));
    assert!(warnings.iter().any(|warning| warning.contains("theme.unknown")));
  }

  #[test]
  fn partial_cli_theme_overlays_only_named_valid_components() {
    let dir = tempdir().unwrap();
    let system = dir.path().join("system.toml");
    write(&system, "[theme]\nborder = 'blue'\ntext = 'white'\ntime = 'yellow'\n");

    let (settings, warnings) = load_paths(Some(&system), None, &matches(&["--theme", "prompt=red"]));

    assert!(warnings.is_empty(), "{warnings:?}");
    let theme = Theme::from_settings(&settings.theme);
    assert_eq!(theme.of(&[Themed::Border]).fg, Some(Color::Blue));
    assert_eq!(theme.of(&[Themed::Text]).fg, Some(Color::White));
    assert_eq!(theme.of(&[Themed::Time]).fg, Some(Color::Yellow));
    assert_eq!(theme.of(&[Themed::Prompt]).fg, Some(Color::Red));
  }

  #[test]
  fn invalid_cli_theme_directives_do_not_mutate_lower_colors() {
    let dir = tempdir().unwrap();
    let system = dir.path().join("system.toml");
    write(&system, "[theme]\nborder = 'blue'\ntext = 'white'\n");

    let (settings, warnings) = load_paths(
      Some(&system),
      None,
      &matches(&["--theme", "border=not-a-color;unknown=green;broken"]),
    );

    assert_eq!(warnings.len(), 3, "{warnings:?}");
    let theme = Theme::from_settings(&settings.theme);
    assert_eq!(theme.of(&[Themed::Border]).fg, Some(Color::Blue));
    assert_eq!(theme.of(&[Themed::Text]).fg, Some(Color::White));
    assert_eq!(settings.theme.prompt, ThemeColor::Unset);
  }

  #[test]
  fn explicit_theme_clears_block_fallback_while_unset_components_inherit() {
    let dir = tempdir().unwrap();
    let system = dir.path().join("system.toml");
    let explicit = dir.path().join("explicit.toml");
    write(&system, "[theme]\ntext = 'red'\nborder = 'blue'\naction = 'green'\n");

    let (inherited, warnings) = load_paths(Some(&system), None, &matches(&[]));

    assert!(warnings.is_empty(), "{warnings:?}");
    let inherited = Theme::from_settings(&inherited.theme);
    assert_eq!(inherited.of(&[Themed::Time]).fg, Some(Color::Red));
    assert_eq!(inherited.of(&[Themed::Greet]).fg, Some(Color::Red));
    assert_eq!(inherited.of(&[Themed::Title]).fg, Some(Color::Blue));
    assert_eq!(inherited.of(&[Themed::ActionButton]).fg, Some(Color::Green));

    write(
      &explicit,
      "[theme]\ntime = false\ngreet = false\ntitle = false\nbutton = false\n",
    );
    let (cleared, warnings) = load_paths(Some(&system), Some(&explicit), &matches(&[]));

    assert!(warnings.is_empty(), "{warnings:?}");
    let cleared = Theme::from_settings(&cleared.theme);
    assert_eq!(cleared.of(&[Themed::Text]).fg, Some(Color::Red));
    assert_eq!(cleared.of(&[Themed::Border]).fg, Some(Color::Blue));
    assert_eq!(cleared.of(&[Themed::Action]).fg, Some(Color::Green));
    assert_eq!(cleared.of(&[Themed::Time]).fg, None);
    assert_eq!(cleared.of(&[Themed::Greet]).fg, None);
    assert_eq!(cleared.of(&[Themed::Title]).fg, None);
    assert_eq!(cleared.of(&[Themed::ActionButton]).fg, None);
  }
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum ThemeColor {
  #[default]
  Unset,
  Value(String),
  Clear,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ThemeSettings {
  pub border: ThemeColor,
  pub text: ThemeColor,
  pub time: ThemeColor,
  pub container: ThemeColor,
  pub title: ThemeColor,
  pub greet: ThemeColor,
  pub prompt: ThemeColor,
  pub input: ThemeColor,
  pub action: ThemeColor,
  pub button: ThemeColor,
}
