use std::{
  borrow::Borrow,
  collections::{HashMap, HashSet},
  env,
  error::Error,
  ffi::{OsStr, OsString},
  fs,
  io,
  os::{fd::AsRawFd, unix::fs::PermissionsExt},
  path::{Path, PathBuf},
  sync::LazyLock,
};

use chrono::Local;
use freedesktop_desktop_entry::{DesktopEntry, Line, parse_line};
use nix::sys::utsname;
use utmp_rs::{UtmpEntry, UtmpParser};
use uzers::os::unix::UserExt;

nix::ioctl_read_bad!(get_keyboard_led_flags, 0x4B64, u8);
nix::ioctl_write_int_bad!(set_keyboard_led_flags, 0x4B65);

use crate::{
  desktop_entry::{parse_exec, shell_join},
  ui::{
    sessions::{Session, SessionType},
    users::User,
  },
};

static XDG_DATA_DIRS: LazyLock<Vec<PathBuf>> = LazyLock::new(|| {
  let value = env::var("XDG_DATA_DIRS").unwrap_or("/usr/local/share:/usr/share".to_string());
  env::split_paths(&value).filter(|p| p.is_absolute()).collect()
});
static DEFAULT_SESSION_PATHS: LazyLock<Vec<(PathBuf, SessionType)>> = LazyLock::new(|| {
  XDG_DATA_DIRS
    .iter()
    .map(|p| (p.join("wayland-sessions"), SessionType::Wayland))
    .chain(XDG_DATA_DIRS.iter().map(|p| (p.join("xsessions"), SessionType::X11)))
    .collect()
});

pub fn get_hostname() -> String {
  match utsname::uname() {
    Ok(uts) => uts.nodename().to_str().unwrap_or("").to_string(),
    _ => String::new(),
  }
}

pub fn get_issue() -> Option<String> {
  let issue = fs::read_to_string("/etc/issue").ok()?;
  let (date, time) = {
    let now = Local::now();

    (
      now.format("%a %b %_d %Y").to_string(),
      now.format("%H:%M:%S").to_string(),
    )
  };

  let user_count = UtmpParser::from_path("/var/run/utmp")
    .map(|utmp| {
      utmp
        .into_iter()
        .filter(|entry| matches!(entry, Ok(UtmpEntry::UserProcess { .. })))
        .count()
    })
    .unwrap_or(0);
  let tty = tty_line(
    env::var_os("XDG_VTNR").as_deref(),
    nix::unistd::ttyname(io::stdin()).ok().as_deref(),
  );
  let uts = utsname::uname().ok();
  let field = |value: Option<&OsStr>| {
    value
      .map(|value| value.to_string_lossy().into_owned())
      .unwrap_or_default()
  };
  let system = IssueSystem {
    date,
    time,
    user_count,
    tty,
    sysname: field(uts.as_ref().map(|uts| uts.sysname())),
    release: field(uts.as_ref().map(|uts| uts.release())),
    version: field(uts.as_ref().map(|uts| uts.version())),
    nodename: field(uts.as_ref().map(|uts| uts.nodename())),
    machine: field(uts.as_ref().map(|uts| uts.machine())),
    domainname: field(uts.as_ref().map(|uts| uts.domainname())),
    os_release: read_os_release(&[Path::new("/etc/os-release"), Path::new("/usr/lib/os-release")]),
  };

  Some(expand_issue(&issue, &system))
}

#[derive(Debug, Default)]
struct IssueSystem {
  date: String,
  time: String,
  user_count: usize,
  tty: String,
  sysname: String,
  release: String,
  version: String,
  nodename: String,
  machine: String,
  domainname: String,
  os_release: HashMap<String, String>,
}

fn tty_line(vtnr: Option<&OsStr>, tty: Option<&Path>) -> String {
  if let Some(vtnr) = vtnr
    .and_then(OsStr::to_str)
    .and_then(|value| value.parse::<u32>().ok())
    .filter(|value| *value > 0)
  {
    return format!("tty{vtnr}");
  }

  tty
    .and_then(|path| path.strip_prefix("/dev").ok())
    .and_then(|path| path.to_str())
    .map(|path| path.trim_start_matches('/').to_string())
    .unwrap_or_default()
}

fn read_os_release(paths: &[&Path]) -> HashMap<String, String> {
  for path in paths {
    match fs::read_to_string(path) {
      Ok(source) => return parse_os_release(&source),
      Err(error) if error.kind() == io::ErrorKind::NotFound => {},
      Err(_) => return HashMap::new(),
    }
  }

  HashMap::new()
}

fn parse_os_release(source: &str) -> HashMap<String, String> {
  let mut values = HashMap::new();

  for line in source.lines() {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
      continue;
    }
    let Some((key, raw_value)) = line.split_once('=') else {
      continue;
    };
    if key.is_empty()
      || !key
        .bytes()
        .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
    {
      continue;
    }
    let Some(mut words) = shlex::split(raw_value) else {
      continue;
    };
    if words.len() != 1 {
      continue;
    }

    // os-release specifies that later duplicate assignments win, matching
    // shell sourcing behavior.
    values.insert(key.to_string(), words.pop().unwrap_or_default());
  }

  values
}

fn expand_issue(source: &str, system: &IssueSystem) -> String {
  let mut output = String::with_capacity(source.len());
  let mut chars = source.chars().peekable();

  while let Some(character) = chars.next() {
    if character != '\\' {
      output.push(character);
      continue;
    }

    let Some(escape) = chars.next() else {
      output.push('\\');
      break;
    };

    match escape {
      '\\' => output.push('\\'),
      'd' => output.push_str(&system.date),
      't' => output.push_str(&system.time),
      'u' => output.push_str(&system.user_count.to_string()),
      'U' => match system.user_count {
        1 => output.push_str("1 user"),
        count => output.push_str(&format!("{count} users")),
      },
      'l' => output.push_str(&system.tty),
      's' => output.push_str(&system.sysname),
      'r' => output.push_str(&system.release),
      'v' => output.push_str(&system.version),
      'n' => output.push_str(&system.nodename),
      'm' => output.push_str(&system.machine),
      'o' => output.push_str(&system.domainname),
      'S' => {
        if let Some(variable) = take_braced(&mut chars) {
          if variable == "ANSI_COLOR" {
            if let Some(color) = system.os_release.get(&variable).filter(|value| valid_sgr(value)) {
              output.push_str("\x1b[");
              output.push_str(color);
              output.push('m');
            }
          } else if let Some(value) = system.os_release.get(&variable) {
            output.push_str(value);
          }
        } else {
          output.push_str(
            system
              .os_release
              .get("PRETTY_NAME")
              .filter(|value| !value.is_empty())
              .map(String::as_str)
              .unwrap_or(&system.sysname),
          );
        }
      },
      'e' => {
        if let Some(name) = take_braced(&mut chars) {
          output.push_str(named_escape(&name).unwrap_or_default());
        } else {
          output.push('\x1b');
        }
      },
      '0' if take_exact(&mut chars, "33") => output.push('\x1b'),
      'x' if take_exact(&mut chars, "1b") || take_exact(&mut chars, "1B") => output.push('\x1b'),
      unsupported => {
        // Unsupported agetty escapes stay visible instead of being silently
        // reinterpreted or discarded.
        output.push('\\');
        output.push(unsupported);
      },
    }
  }

  output
}

fn take_exact(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, expected: &str) -> bool {
  let mut candidate = chars.clone();
  if expected.chars().all(|expected| candidate.next() == Some(expected)) {
    *chars = candidate;
    true
  } else {
    false
  }
}

fn take_braced(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<String> {
  if chars.peek() != Some(&'{') {
    return None;
  }

  let mut candidate = chars.clone();
  candidate.next();
  let mut value = String::new();
  for character in candidate.by_ref() {
    if character == '}' {
      *chars = candidate;
      return Some(value);
    }
    value.push(character);
  }

  None
}

fn valid_sgr(value: &str) -> bool {
  !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit() || byte == b';')
}

fn named_escape(name: &str) -> Option<&'static str> {
  Some(match name {
    "black" => "\x1b[30m",
    "blink" => "\x1b[5m",
    "blue" => "\x1b[34m",
    "bold" => "\x1b[1m",
    "brown" => "\x1b[33m",
    "cyan" => "\x1b[36m",
    "darkgray" => "\x1b[1;30m",
    "gray" | "lightgray" => "\x1b[37m",
    "green" => "\x1b[32m",
    "half-bright" | "halfbright" => "\x1b[2m",
    "lightblue" => "\x1b[1;34m",
    "lightcyan" => "\x1b[1;36m",
    "lightgreen" => "\x1b[1;32m",
    "lightmagenta" => "\x1b[1;35m",
    "lightred" => "\x1b[1;31m",
    "magenta" => "\x1b[35m",
    "red" => "\x1b[31m",
    "reset" => "\x1b[0m",
    "reverse" => "\x1b[7m",
    "yellow" => "\x1b[1;33m",
    "white" => "\x1b[1;37m",
    _ => return None,
  })
}

#[cfg(test)]
mod issue_tests {
  use std::{collections::HashMap, ffi::OsStr, fs, path::Path};

  use tempfile::tempdir;

  use super::{IssueSystem, expand_issue, parse_os_release, read_os_release, tty_line};

  fn system() -> IssueSystem {
    IssueSystem {
      date: "Sun Jul 20 2026".into(),
      time: "14:30:00".into(),
      user_count: 2,
      tty: "tty1".into(),
      sysname: "Linux".into(),
      release: "7.1.4-1-cachyos".into(),
      version: "#1 SMP PREEMPT_DYNAMIC".into(),
      nodename: "host".into(),
      machine: "x86_64".into(),
      domainname: "localdomain".into(),
      os_release: HashMap::from([
        ("PRETTY_NAME".into(), "CachyOS".into()),
        ("ANSI_COLOR".into(), "38;2;23;147;209".into()),
      ]),
    }
  }

  #[test]
  fn expands_the_cachyos_issue_like_agetty() {
    assert_eq!(
      expand_issue("\\S{PRETTY_NAME} \\r (\\l)\n\n", &system()),
      "CachyOS 7.1.4-1-cachyos (tty1)\n\n"
    );
  }

  #[test]
  fn expansion_is_single_pass_and_supports_the_documented_local_subset() {
    let mut system = system();
    system.os_release.insert("RECURSIVE".into(), r"name \r".into());
    let source = concat!(
      r"\S|\S{PRETTY_NAME}|\S{MISSING}|\S{RECURSIVE}|\\S{PRETTY_NAME}",
      "\n",
      r"\s|\r|\v|\n|\m|\o|\l|\d|\t|\u|\U",
      "\n",
      r"\e{red}red\e{reset}|\e|\033|\x1b|\q"
    );

    assert_eq!(
      expand_issue(source, &system),
      concat!(
        "CachyOS|CachyOS||name \\r|\\S{PRETTY_NAME}\n",
        "Linux|7.1.4-1-cachyos|#1 SMP PREEMPT_DYNAMIC|host|x86_64|localdomain|tty1|",
        "Sun Jul 20 2026|14:30:00|2|2 users\n",
        "\x1b[31mred\x1b[0m|\x1b|\x1b|\x1b|\\q"
      )
    );
  }

  #[test]
  fn expands_os_release_ansi_color_only_as_sgr() {
    let mut system = system();
    assert_eq!(expand_issue(r"\S{ANSI_COLOR}", &system), "\x1b[38;2;23;147;209m");

    system.os_release.insert("ANSI_COLOR".into(), "31mBAD".into());
    assert_eq!(expand_issue(r"\S{ANSI_COLOR}", &system), "");
  }

  #[test]
  fn os_release_parser_unquotes_values_and_uses_the_last_duplicate() {
    let values = parse_os_release(
      "# comment\nPRETTY_NAME=Old\nPRETTY_NAME=\"CachyOS Linux\"\nEMPTY=\"\"\nESCAPED=\"a \\\"quote\\\"\"\nBROKEN='unterminated\nTWO=words here\n",
    );

    assert_eq!(values.get("PRETTY_NAME").map(String::as_str), Some("CachyOS Linux"));
    assert_eq!(values.get("EMPTY").map(String::as_str), Some(""));
    assert_eq!(values.get("ESCAPED").map(String::as_str), Some("a \"quote\""));
    assert!(!values.contains_key("BROKEN"));
    assert!(!values.contains_key("TWO"));
  }

  #[test]
  fn os_release_falls_back_only_when_the_etc_file_is_missing() {
    let root = tempdir().unwrap();
    let missing = root.path().join("etc-os-release");
    let fallback = root.path().join("usr-lib-os-release");
    fs::write(&fallback, "PRETTY_NAME=Fallback\n").unwrap();

    assert_eq!(
      read_os_release(&[&missing, &fallback])
        .get("PRETTY_NAME")
        .map(String::as_str),
      Some("Fallback")
    );

    fs::write(&missing, "PRETTY_NAME=Preferred\n").unwrap();
    assert_eq!(
      read_os_release(&[&missing, &fallback])
        .get("PRETTY_NAME")
        .map(String::as_str),
      Some("Preferred")
    );
  }

  #[test]
  fn tty_prefers_a_valid_vt_and_otherwise_uses_the_actual_device() {
    assert_eq!(tty_line(Some(OsStr::new("7")), Some(Path::new("/dev/pts/3"))), "tty7");
    assert_eq!(tty_line(Some(OsStr::new("0")), Some(Path::new("/dev/pts/3"))), "pts/3");
    assert_eq!(
      tty_line(Some(OsStr::new("invalid")), Some(Path::new("/dev/tty2"))),
      "tty2"
    );
    assert_eq!(tty_line(None, None), "");
  }
}

pub fn get_users(min_uid: u32, max_uid: u32) -> Vec<User> {
  // SAFETY: uzers exposes NSS enumeration as unsafe because libc owns global
  // iteration state. This creates one iterator and consumes it completely
  // without starting another enumeration in between.
  users_in_range(unsafe { uzers::all_users() }, min_uid, max_uid)
}

fn users_in_range<I, U>(users: I, min_uid: u32, max_uid: u32) -> Vec<User>
where
  I: IntoIterator<Item = U>,
  U: Borrow<uzers::User>,
{
  let mut users = users
    .into_iter()
    .filter(|user| {
      let user = user.borrow();
      user.uid() >= min_uid && user.uid() <= max_uid
    })
    .map(|user| {
      let user = user.borrow();

      User {
        username: user.name().to_string_lossy().to_string(),
        name: match user.gecos() {
          name if name.is_empty() => None,
          name => {
            let name = name.to_string_lossy();

            match name.split_once(',') {
              Some((name, _)) => Some(name.to_string()),
              None => Some(name.to_string()),
            }
          },
        },
      }
    })
    .collect::<Vec<_>>();
  users.sort_by(|left, right| {
    left
      .username
      .cmp(&right.username)
      .then_with(|| left.name.cmp(&right.name))
  });
  users
}

pub fn session_paths(sessions: &[String], xsessions: &[String]) -> Vec<(PathBuf, SessionType)> {
  effective_session_paths(sessions, xsessions, &DEFAULT_SESSION_PATHS)
}

fn effective_session_paths(
  sessions: &[String],
  xsessions: &[String],
  defaults: &[(PathBuf, SessionType)],
) -> Vec<(PathBuf, SessionType)> {
  let configured = [(sessions, SessionType::Wayland), (xsessions, SessionType::X11)];
  let mut paths = Vec::new();

  for (configured, session_type) in configured {
    if configured.is_empty() {
      paths.extend(
        defaults
          .iter()
          .filter(|(_, default_type)| *default_type == session_type)
          .cloned(),
      );
    } else {
      paths.extend(configured.iter().map(|path| (PathBuf::from(path), session_type)));
    }
  }

  let mut seen = HashSet::new();
  paths.retain(|path| seen.insert(path.clone()));
  paths
}

pub fn get_sessions(paths: &[(PathBuf, SessionType)]) -> Result<Vec<Session>, Box<dyn Error>> {
  let mut files = Vec::new();
  let mut seen = HashSet::<(SessionType, OsString)>::new();

  for (path, session_type) in paths {
    tracing::info!("reading {:?} sessions from '{}'", session_type, path.display());

    let mut entries = match fs::read_dir(path) {
      Ok(entries) => entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>(),
      Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
      Err(error) => {
        tracing::warn!("failed to read session directory '{}': {error}", path.display());
        continue;
      },
    };
    entries.sort_unstable();

    for path in entries {
      if path.extension() != Some(OsStr::new("desktop")) || !path.metadata().is_ok_and(|metadata| metadata.is_file()) {
        continue;
      }

      let Some(desktop_id) = path.file_name().map(OsStr::to_owned) else {
        continue;
      };

      // Session paths are ordered from highest to lowest priority. A hidden,
      // invalid, or unavailable entry still masks the same desktop ID in a
      // lower-priority directory, as if the lower file did not exist.
      if !seen.insert((*session_type, desktop_id)) {
        continue;
      }

      match load_desktop_file(&path, *session_type) {
        Ok(Some(session)) => files.push(session),
        Ok(None) => {},
        Err(error) => tracing::warn!("ignoring invalid session '{}': {error}", path.display()),
      }
    }
  }

  files.sort_by(|a, b| {
    a.name
      .cmp(&b.name)
      .then_with(|| a.slug.cmp(&b.slug))
      .then_with(|| a.path.cmp(&b.path))
  });

  tracing::info!("found {} sessions", files.len());

  Ok(files)
}

fn load_desktop_file<P>(path: P, session_type: SessionType) -> Result<Option<Session>, Box<dyn Error>>
where
  P: AsRef<Path>,
{
  let path = path.as_ref();
  let source = fs::read_to_string(path)?;
  validate_desktop_structure(&source)?;
  let desktop = DesktopEntry::from_str(path, &source, Option::<&[&str]>::None)?;

  if desktop.groups.desktop_entry().is_none() {
    return Err(io::Error::new(io::ErrorKind::InvalidData, "missing Desktop Entry group").into());
  }
  if desktop.type_() != Some("Application") {
    return Err(io::Error::new(io::ErrorKind::InvalidData, "Type must be Application").into());
  }

  if desktop_bool(&desktop, "Hidden")? {
    tracing::info!("ignoring session in '{}': Hidden=true", path.display());
    return Ok(None);
  }
  if desktop_bool(&desktop, "NoDisplay")? {
    tracing::info!("ignoring session in '{}': NoDisplay=true", path.display());
    return Ok(None);
  }

  if let Some(command) = desktop.try_exec()
    && !try_exec_exists(command)
  {
    tracing::info!(
      "ignoring session in '{}': TryExec={command:?} is not executable",
      path.display()
    );
    return Ok(None);
  }

  let name = desktop
    .name::<&str>(&[])
    .filter(|name| !name.is_empty())
    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing or empty Name key"))?;
  let exec = desktop
    .exec()
    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing Exec key"))?;
  let command = shell_join(&parse_exec(exec, &name, desktop.icon(), path)?);
  let xdg_desktop_names = desktop.desktop_entry("DesktopNames").map(str::to_string);

  tracing::info!("got session '{}' in '{}'", name, path.display());

  Ok(Some(Session {
    slug: Some(desktop.id().to_string()),
    name: name.into_owned(),
    command,
    session_type,
    path: Some(path.into()),
    xdg_desktop_names,
  }))
}

fn validate_desktop_structure(source: &str) -> Result<(), io::Error> {
  let mut current_group = None::<String>;
  let mut groups = HashSet::<String>::new();
  let mut keys = HashSet::<(String, String)>::new();

  for (index, line) in source.lines().enumerate() {
    match parse_line(line).map_err(|error| {
      io::Error::new(
        io::ErrorKind::InvalidData,
        format!("line {} is not valid Desktop Entry syntax: {error}", index + 1),
      )
    })? {
      Line::Group(group) => {
        if !groups.insert(group.to_string()) {
          return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("line {} repeats group [{group}]", index + 1),
          ));
        }
        current_group = Some(group.to_string());
      },
      Line::Entry(key, _) => {
        let group = current_group.as_ref().ok_or_else(|| {
          io::Error::new(
            io::ErrorKind::InvalidData,
            format!("line {} defines {key:?} before any group", index + 1),
          )
        })?;
        let key = key.trim();
        if !keys.insert((group.clone(), key.to_string())) {
          return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("line {} repeats key {key:?} in group [{group}]", index + 1),
          ));
        }
      },
      Line::Comment(_) => {},
    }
  }

  Ok(())
}

fn desktop_bool(desktop: &DesktopEntry, key: &str) -> Result<bool, io::Error> {
  match desktop.desktop_entry(key) {
    None | Some("false") => Ok(false),
    Some("true") => Ok(true),
    Some(value) => Err(io::Error::new(
      io::ErrorKind::InvalidData,
      format!("{key} must be true or false, got {value:?}"),
    )),
  }
}

fn try_exec_exists(command: &str) -> bool {
  let is_executable = |path: &Path| {
    path
      .metadata()
      .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
  };
  let command = Path::new(command);

  if command.is_absolute() {
    return is_executable(command);
  }

  env::var_os("PATH")
    .map(|paths| env::split_paths(&paths).any(|directory| is_executable(&directory.join(command))))
    .unwrap_or(false)
}

pub fn capslock_status() -> bool {
  let mut flags = 0;
  // SAFETY: KDGKBLED writes exactly one byte to the supplied pointer. `flags`
  // is live and writable for the duration of this synchronous ioctl call.
  let result = unsafe { get_keyboard_led_flags(io::stdin().as_raw_fd(), &mut flags) };

  result.is_ok() && capslock_is_on(flags)
}

pub fn enable_numlock() -> io::Result<()> {
  let fd = io::stdin().as_raw_fd();
  let mut flags = 0;
  // SAFETY: KDGKBLED writes one byte to `flags`; KDSKBLED consumes the
  // resulting integer value. Both ioctls operate synchronously on the live
  // console descriptor and neither retains a pointer.
  unsafe {
    get_keyboard_led_flags(fd, &mut flags).map_err(io::Error::from)?;
    set_keyboard_led_flags(fd, numlock_flags(flags).into()).map_err(io::Error::from)?;
  }
  Ok(())
}

fn numlock_flags(flags: u8) -> u8 {
  const LED_NUM: u8 = 0x02;

  flags | LED_NUM | (LED_NUM << 4)
}

fn capslock_is_on(flags: u8) -> bool {
  const LED_CAP: u8 = 0x04;

  flags & LED_CAP != 0
}

#[cfg(test)]
mod capslock_tests {
  use super::{capslock_is_on, numlock_flags};

  #[test]
  fn reads_the_current_capslock_bit_only() {
    assert!(capslock_is_on(0x04));
    assert!(capslock_is_on(0x07));
    assert!(!capslock_is_on(0x00));
    assert!(!capslock_is_on(0x40));
  }

  #[test]
  fn enables_current_and_default_numlock_without_clearing_other_flags() {
    assert_eq!(numlock_flags(0x00), 0x22);
    assert_eq!(numlock_flags(0x55), 0x77);
    assert_eq!(numlock_flags(0xff), 0xff);
  }
}

#[cfg(test)]
mod session_tests {
  use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
  };

  use tempfile::tempdir;

  use super::{effective_session_paths, get_sessions, load_desktop_file};
  use crate::ui::sessions::SessionType;

  fn write_desktop(directory: &Path, name: &str, contents: &str) -> PathBuf {
    let path = directory.join(name);
    fs::write(&path, contents).unwrap();
    path
  }

  fn visible_session(name: &str, extra: &str) -> String {
    format!(
      "[Desktop Entry]\nType=Application\nName={name}\nExec=/usr/bin/session --flag\nDesktopNames=one;two;\n{extra}"
    )
  }

  #[test]
  fn parses_a_standard_session_entry() {
    let root = tempdir().unwrap();
    let executable = root.path().join("session");
    fs::write(&executable, "").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    let path = write_desktop(
      root.path(),
      "example.desktop",
      &format!(
        "[Desktop Entry]\nType=Application\nName=My\\sSession\nIcon=my-icon\nExec=/usr/bin/session \"two\\swords\" %% %F %i %c %k\nDesktopNames=one;two;\nTryExec={}\n",
        executable.display()
      ),
    );

    let session = load_desktop_file(&path, SessionType::Wayland).unwrap().unwrap();

    assert_eq!(session.slug.as_deref(), Some("example"));
    assert_eq!(session.name, "My Session");
    assert_eq!(
      session.command,
      format!(
        "'/usr/bin/session' 'two words' '%' '--icon' 'my-icon' 'My Session' '{}'",
        path.display()
      )
    );
    assert_eq!(session.session_type, SessionType::Wayland);
    assert_eq!(session.path.as_deref(), Some(path.as_path()));
    assert_eq!(session.xdg_desktop_names.as_deref(), Some("one;two;"));
  }

  #[test]
  fn rejects_non_application_and_invalid_boolean_entries() {
    let root = tempdir().unwrap();
    let link = write_desktop(
      root.path(),
      "link.desktop",
      "[Desktop Entry]\nType=Link\nName=Link\nExec=link\n",
    );
    let invalid_bool = write_desktop(
      root.path(),
      "invalid.desktop",
      &visible_session("Invalid", "Hidden=yes\n"),
    );

    assert!(load_desktop_file(link, SessionType::Wayland).is_err());
    assert!(load_desktop_file(invalid_bool, SessionType::Wayland).is_err());
  }

  #[test]
  fn rejects_duplicate_groups_and_keys() {
    let root = tempdir().unwrap();
    let duplicate_key = write_desktop(
      root.path(),
      "key.desktop",
      "[Desktop Entry]\nType=Application\nName=One\nName=Two\nExec=session\n",
    );
    let duplicate_group = write_desktop(
      root.path(),
      "group.desktop",
      "[Desktop Entry]\nType=Application\nName=One\nExec=session\n[Desktop Entry]\nName=Two\n",
    );

    assert!(load_desktop_file(duplicate_key, SessionType::Wayland).is_err());
    assert!(load_desktop_file(duplicate_group, SessionType::Wayland).is_err());
  }

  #[test]
  fn hidden_and_unavailable_sessions_are_ignored() {
    let root = tempdir().unwrap();
    let hidden = write_desktop(
      root.path(),
      "hidden.desktop",
      &visible_session("Hidden", "Hidden=true\n"),
    );
    let missing = write_desktop(
      root.path(),
      "missing.desktop",
      &visible_session("Missing", "TryExec=/definitely/missing/tuigreety-session\n"),
    );

    assert!(load_desktop_file(hidden, SessionType::Wayland).unwrap().is_none());
    assert!(load_desktop_file(missing, SessionType::Wayland).unwrap().is_none());
  }

  #[test]
  fn higher_priority_entries_mask_the_same_desktop_id() {
    let high = tempdir().unwrap();
    let low = tempdir().unwrap();
    write_desktop(
      high.path(),
      "same.desktop",
      &visible_session("Hidden override", "Hidden=true\n"),
    );
    write_desktop(low.path(), "same.desktop", &visible_session("Must stay hidden", ""));
    write_desktop(low.path(), "z.desktop", &visible_session("Same name", ""));
    write_desktop(low.path(), "a.desktop", &visible_session("Same name", ""));
    write_desktop(low.path(), "not-a-session.ini", &visible_session("Ignored", ""));
    fs::create_dir(low.path().join("directory.desktop")).unwrap();

    let paths = vec![
      (high.path().into(), SessionType::Wayland),
      (low.path().into(), SessionType::Wayland),
    ];

    let sessions = get_sessions(&paths).unwrap();
    let slugs = sessions
      .iter()
      .map(|session| session.slug.as_deref().unwrap())
      .collect::<Vec<_>>();

    assert_eq!(slugs, ["a", "z"]);
  }

  #[test]
  fn each_session_type_falls_back_independently() {
    let defaults = vec![
      (PathBuf::from("/default/wayland"), SessionType::Wayland),
      (PathBuf::from("/default/x11"), SessionType::X11),
    ];
    let custom_wayland = vec!["/custom/wayland".to_string()];

    assert_eq!(effective_session_paths(&custom_wayland, &[], &defaults), [
      (PathBuf::from("/custom/wayland"), SessionType::Wayland),
      (PathBuf::from("/default/x11"), SessionType::X11),
    ]);
    assert_eq!(effective_session_paths(&[], &["/custom/x11".to_string()], &defaults), [
      (PathBuf::from("/default/wayland"), SessionType::Wayland),
      (PathBuf::from("/custom/x11"), SessionType::X11),
    ]);
  }
}

#[cfg(test)]
mod user_tests {
  use uzers::{User, os::unix::UserExt};

  use super::users_in_range;

  #[test]
  fn filters_and_formats_injected_users() {
    let users = users_in_range(
      [
        User::new(0, "root", 0).with_gecos("Root"),
        User::new(1000, "joe", 1000).with_gecos("Joe Example,Room 1"),
        User::new(1500, "bob", 1500),
        User::new(2100, "postgres", 2100),
      ],
      1000,
      2000,
    );

    assert_eq!(users.len(), 2);
    assert_eq!(users[0].username, "bob");
    assert_eq!(users[0].name, None);
    assert_eq!(users[1].username, "joe");
    assert_eq!(users[1].name.as_deref(), Some("Joe Example"));
  }
}
