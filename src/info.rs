use std::{
  collections::HashSet,
  env,
  error::Error,
  ffi::{OsStr, OsString},
  fs::{self, File},
  io::{self, BufRead, BufReader},
  os::unix::fs::PermissionsExt,
  path::{Path, PathBuf},
  process::Command,
  sync::LazyLock,
};

use chrono::Local;
use freedesktop_desktop_entry::{DesktopEntry, Line, parse_line};
use nix::sys::utsname;
use utmp_rs::{UtmpEntry, UtmpParser};
use uzers::os::unix::UserExt;

use crate::{
  Greeter,
  ui::{
    common::masked::MaskedString,
    sessions::{Session, SessionType},
    users::User,
  },
};

const LAST_USER_USERNAME: &str = "/var/cache/tuigreet/lastuser";
const LAST_USER_NAME: &str = "/var/cache/tuigreet/lastuser-name";
const LAST_COMMAND: &str = "/var/cache/tuigreet/lastsession";
const LAST_SESSION: &str = "/var/cache/tuigreet/lastsession-path";

const DEFAULT_MIN_UID: u32 = 1000;
const DEFAULT_MAX_UID: u32 = 60000;

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
  let (date, time) = {
    let now = Local::now();

    (
      now.format("%a %b %_d %Y").to_string(),
      now.format("%H:%M:%S").to_string(),
    )
  };

  let user_count = match UtmpParser::from_path("/var/run/utmp")
    .map(|utmp| {
      utmp.into_iter().fold(0, |acc, entry| match entry {
        Ok(UtmpEntry::UserProcess { .. }) => acc + 1,
        Ok(UtmpEntry::LoginProcess { .. }) => acc + 1,
        _ => acc,
      })
    })
    .unwrap_or(0)
  {
    n if n < 2 => format!("{n} user"),
    n => format!("{n} users"),
  };

  let vtnr: usize = env::var("XDG_VTNR")
    .unwrap_or_else(|_| "0".to_string())
    .parse()
    .unwrap_or(0);
  let uts = utsname::uname();

  if let Ok(issue) = fs::read_to_string("/etc/issue") {
    let issue = issue
      .replace("\\S", "Linux")
      .replace("\\l", &format!("tty{vtnr}"))
      .replace("\\d", &date)
      .replace("\\t", &time)
      .replace("\\U", &user_count);

    let issue = match uts {
      Ok(uts) => issue
        .replace("\\s", uts.sysname().to_str().unwrap_or(""))
        .replace("\\r", uts.release().to_str().unwrap_or(""))
        .replace("\\v", uts.version().to_str().unwrap_or(""))
        .replace("\\n", uts.nodename().to_str().unwrap_or(""))
        .replace("\\m", uts.machine().to_str().unwrap_or(""))
        .replace("\\o", uts.domainname().to_str().unwrap_or("")),

      _ => issue,
    };

    return Some(
      issue
        .replace("\\x1b", "\x1b")
        .replace("\\033", "\x1b")
        .replace("\\e", "\x1b")
        .replace(r"\\", r"\"),
    );
  }

  None
}

pub fn get_last_user_username() -> Option<String> {
  match fs::read_to_string(LAST_USER_USERNAME).ok() {
    None => None,
    Some(username) => {
      let username = username.trim();

      if username.is_empty() {
        None
      } else {
        Some(username.to_string())
      }
    },
  }
}

pub fn get_last_user_name() -> Option<String> {
  match fs::read_to_string(LAST_USER_NAME).ok() {
    None => None,
    Some(name) => {
      let name = name.trim();

      if name.is_empty() { None } else { Some(name.to_string()) }
    },
  }
}

pub fn write_last_username(username: &MaskedString) {
  let _ = fs::write(LAST_USER_USERNAME, &username.value);

  if let Some(ref name) = username.mask {
    let _ = fs::write(LAST_USER_NAME, name);
  } else {
    let _ = fs::remove_file(LAST_USER_NAME);
  }
}

pub fn get_last_session_path() -> Result<PathBuf, io::Error> {
  Ok(PathBuf::from(fs::read_to_string(LAST_SESSION)?.trim()))
}

pub fn get_last_command() -> Result<String, io::Error> {
  Ok(fs::read_to_string(LAST_COMMAND)?.trim().to_string())
}

pub fn write_last_session_path<P>(session: &P)
where
  P: AsRef<Path>,
{
  let _ = fs::write(LAST_SESSION, session.as_ref().to_string_lossy().as_bytes());
}

pub fn write_last_command(session: &str) {
  let _ = fs::write(LAST_COMMAND, session);
}

pub fn get_last_user_session(username: &str) -> Result<PathBuf, io::Error> {
  Ok(PathBuf::from(
    fs::read_to_string(format!("{LAST_SESSION}-{username}"))?.trim(),
  ))
}

pub fn get_last_user_command(username: &str) -> Result<String, io::Error> {
  Ok(
    fs::read_to_string(format!("{LAST_COMMAND}-{username}"))?
      .trim()
      .to_string(),
  )
}

pub fn write_last_user_session<P>(username: &str, session: P)
where
  P: AsRef<Path>,
{
  let _ = fs::write(
    format!("{LAST_SESSION}-{username}"),
    session.as_ref().to_string_lossy().as_bytes(),
  );
}

pub fn delete_last_session() {
  let _ = fs::remove_file(LAST_SESSION);
}

pub fn write_last_user_command(username: &str, session: &str) {
  let _ = fs::write(format!("{LAST_COMMAND}-{username}"), session);
}

pub fn delete_last_user_session(username: &str) {
  let _ = fs::remove_file(format!("{LAST_SESSION}-{username}"));
}

pub fn delete_last_command() {
  let _ = fs::remove_file(LAST_COMMAND);
}

pub fn delete_last_user_command(username: &str) {
  let _ = fs::remove_file(format!("{LAST_COMMAND}-{username}"));
}

pub fn get_users(min_uid: u32, max_uid: u32) -> Vec<User> {
  let users = unsafe { uzers::all_users() };

  let users: Vec<User> = users
    .filter(|user| user.uid() >= min_uid && user.uid() <= max_uid)
    .map(|user| User {
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
    })
    .collect();

  users
}

pub fn get_min_max_uids(min_uid: Option<u32>, max_uid: Option<u32>) -> (u32, u32) {
  if let (Some(min_uid), Some(max_uid)) = (min_uid, max_uid) {
    return (min_uid, max_uid);
  }

  let overrides = (min_uid, max_uid);
  let default = (min_uid.unwrap_or(DEFAULT_MIN_UID), max_uid.unwrap_or(DEFAULT_MAX_UID));

  match File::open("/etc/login.defs") {
    Err(_) => default,
    Ok(file) => {
      let file = BufReader::new(file);

      let uids: (u32, u32) = file.lines().fold(default, |acc, line| {
        line
          .map(|line| {
            let mut tokens = line.split_whitespace();

            match (overrides, tokens.next(), tokens.next()) {
              ((None, _), Some("UID_MIN"), Some(value)) => (value.parse::<u32>().unwrap_or(acc.0), acc.1),
              ((_, None), Some("UID_MAX"), Some(value)) => (acc.0, value.parse::<u32>().unwrap_or(acc.1)),
              _ => acc,
            }
          })
          .unwrap_or(acc)
      });

      uids
    },
  }
}

pub fn get_sessions(greeter: &Greeter) -> Result<Vec<Session>, Box<dyn Error>> {
  let paths = if greeter.session_paths.is_empty() {
    DEFAULT_SESSION_PATHS.as_ref()
  } else {
    &greeter.session_paths
  };

  let mut files = Vec::new();
  let mut seen = HashSet::<(SessionType, OsString)>::new();

  for (path, session_type) in paths.iter() {
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
    .filter(|exec| !exec.trim().is_empty())
    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing or empty Exec key"))?;
  let xdg_desktop_names = desktop.desktop_entry("DesktopNames").map(str::to_string);

  tracing::info!("got session '{}' in '{}'", name, path.display());

  Ok(Some(Session {
    slug: Some(desktop.id().to_string()),
    name: name.into_owned(),
    command: exec.to_string(),
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
  let mut command = Command::new("kbdinfo");
  command.args(["gkbled", "capslock"]);

  match command.output() {
    Ok(output) => output.status.code() == Some(0),
    Err(_) => false,
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

  use super::{get_sessions, load_desktop_file};
  use crate::{Greeter, ui::sessions::SessionType};

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
      &visible_session("My\\sSession", &format!("TryExec={}\n", executable.display())),
    );

    let session = load_desktop_file(&path, SessionType::Wayland).unwrap().unwrap();

    assert_eq!(session.slug.as_deref(), Some("example"));
    assert_eq!(session.name, "My Session");
    assert_eq!(session.command, "/usr/bin/session --flag");
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

    let mut greeter = Greeter::default();
    greeter.session_paths = vec![
      (high.path().into(), SessionType::Wayland),
      (low.path().into(), SessionType::Wayland),
    ];

    let sessions = get_sessions(&greeter).unwrap();
    let slugs = sessions
      .iter()
      .map(|session| session.slug.as_deref().unwrap())
      .collect::<Vec<_>>();

    assert_eq!(slugs, ["a", "z"]);
  }
}

#[cfg(feature = "nsswrapper")]
#[cfg(test)]
mod nsswrapper_tests {
  #[test]
  fn nsswrapper_get_users_from_nss() {
    use super::get_users;

    let users = get_users(1000, 2000);

    assert_eq!(users.len(), 2);
    assert_eq!(users[0].username, "joe");
    assert_eq!(users[0].name, Some("Joe".to_string()));
    assert_eq!(users[1].username, "bob");
    assert_eq!(users[1].name, None);
  }
}
