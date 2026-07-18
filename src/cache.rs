use std::{
  collections::BTreeMap,
  ffi::OsStr,
  fmt,
  fs::File,
  io::{self, Read, Write},
  os::{fd::OwnedFd, unix::ffi::OsStrExt},
  path::{Path, PathBuf},
  sync::Arc,
  time::{Duration, Instant},
};

use nix::{
  dir::Dir,
  errno::Errno,
  fcntl::{Flock, FlockArg, OFlag, open, openat, renameat},
  sys::stat::{Mode, SFlag, fchmod, fstat},
  unistd::{UnlinkatFlags, dup, fsync, unlinkat},
};
use serde::{Deserialize, Serialize};

use crate::ui::sessions::{Session, SessionType};

const SYSTEM_CACHE_DIRECTORY: &str = "/var/cache/tuigreet";
const STATE_FILE: &str = "state.json";
const LOCK_FILE: &str = ".state.lock";
const TEMP_FILE: &str = ".state.tmp";
const STATE_VERSION: u64 = 1;
const MAX_STATE_SIZE: u64 = 1024 * 1024;
const MAX_USERS: usize = 4096;
const MAX_USERNAME_SIZE: usize = 256;
const MAX_DISPLAY_NAME_SIZE: usize = 4096;
const MAX_SESSION_ID_SIZE: usize = 4096;
const MAX_COMMAND_SIZE: usize = 16 * 1024;
const LOCK_TIMEOUT: Duration = Duration::from_millis(250);
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(10);

const LEGACY_LAST_USER: &str = "lastuser";
const LEGACY_LAST_USER_NAME: &str = "lastuser-name";
const LEGACY_GLOBAL_COMMAND: &str = "lastsession";
const LEGACY_GLOBAL_SESSION: &str = "lastsession-path";
const LEGACY_USER_COMMAND_PREFIX: &str = "lastsession-";
const LEGACY_USER_SESSION_PREFIX: &str = "lastsession-path-";

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct CacheState {
  last_user: Option<RememberedUser>,
  global_selection: Option<RememberedSelection>,
  user_selections: BTreeMap<String, RememberedSelection>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RememberedUser {
  pub username: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub display_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) enum RememberedSelection {
  DesktopEntry {
    desktop_id: String,
    session_type: CachedSessionType,
  },
  Command {
    command: String,
  },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CachedSessionType {
  X11,
  Wayland,
}

#[derive(Clone, Debug)]
pub(crate) struct CacheUpdate {
  last_user: Change<RememberedUser>,
  global_selection: Change<RememberedSelection>,
  user_selection: Option<(String, Option<RememberedSelection>)>,
  purge_commands: bool,
}

#[derive(Clone, Debug)]
enum Change<T> {
  Keep,
  Set(Option<T>),
}

#[derive(Clone, Debug)]
pub(crate) struct CacheStore {
  root: Option<Arc<PathBuf>>,
}

#[derive(Debug, Default)]
pub(crate) struct CacheLoad {
  pub state: CacheState,
  pub warnings: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct CacheCommit {
  pub state: CacheState,
  pub warnings: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct CacheError(String);

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CacheFile {
  version: u64,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  last_user: Option<RememberedUser>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  global_selection: Option<RememberedSelection>,
  #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
  user_selections: BTreeMap<String, RememberedSelection>,
}

#[derive(Deserialize)]
struct VersionHeader {
  version: u64,
}

enum StoredState {
  Missing,
  Current(CacheState),
  Corrupt(String),
  Future(u64),
}

struct LockedDirectory {
  directory: OwnedFd,
  _lock: Flock<File>,
}

impl RememberedSelection {
  pub(crate) fn from_session(session: &Session) -> Option<Self> {
    let desktop_id = session.slug.clone()?;
    let session_type = CachedSessionType::from_session_type(session.session_type)?;

    Some(Self::DesktopEntry {
      desktop_id,
      session_type,
    })
  }

  pub(crate) fn command(command: String) -> Self {
    Self::Command { command }
  }

  pub(crate) fn resolve(&self, sessions: &[Session]) -> Option<usize> {
    let Self::DesktopEntry {
      desktop_id,
      session_type,
    } = self
    else {
      return None;
    };

    sessions.iter().position(|session| {
      session.slug.as_deref() == Some(desktop_id)
        && CachedSessionType::from_session_type(session.session_type) == Some(*session_type)
    })
  }

  pub(crate) fn command_value(&self) -> Option<&str> {
    match self {
      Self::Command { command } => Some(command),
      Self::DesktopEntry { .. } => None,
    }
  }

  fn is_command(&self) -> bool {
    matches!(self, Self::Command { .. })
  }
}

impl CachedSessionType {
  fn from_session_type(session_type: SessionType) -> Option<Self> {
    match session_type {
      SessionType::X11 => Some(Self::X11),
      SessionType::Wayland => Some(Self::Wayland),
      SessionType::Tty | SessionType::None => None,
    }
  }
}

impl CacheState {
  pub(crate) fn last_user(&self) -> Option<&RememberedUser> {
    self.last_user.as_ref()
  }

  pub(crate) fn global_selection(&self) -> Option<&RememberedSelection> {
    self.global_selection.as_ref()
  }

  pub(crate) fn user_selection(&self, username: &str) -> Option<&RememberedSelection> {
    self.user_selections.get(username)
  }

  pub(crate) fn purge_commands(&mut self) -> bool {
    self.remove_commands()
  }

  fn apply(&mut self, update: CacheUpdate) {
    if let Change::Set(last_user) = update.last_user {
      self.last_user = last_user;
    }
    if let Change::Set(selection) = update.global_selection {
      self.global_selection = selection;
    }
    if let Some((username, selection)) = update.user_selection {
      match selection {
        Some(selection) => {
          self.user_selections.insert(username, selection);
        },
        None => {
          self.user_selections.remove(&username);
        },
      }
    }
    if update.purge_commands {
      self.remove_commands();
    }
  }

  fn remove_commands(&mut self) -> bool {
    let mut changed = false;
    if self
      .global_selection
      .as_ref()
      .is_some_and(RememberedSelection::is_command)
    {
      self.global_selection = None;
      changed = true;
    }
    let old_len = self.user_selections.len();
    self.user_selections.retain(|_, selection| !selection.is_command());
    changed || self.user_selections.len() != old_len
  }

  fn validate(&self) -> Result<(), CacheError> {
    if self.user_selections.len() > MAX_USERS {
      return Err(CacheError(format!(
        "cache contains more than {MAX_USERS} per-user selections"
      )));
    }

    if let Some(user) = &self.last_user {
      validate_text("remembered username", &user.username, MAX_USERNAME_SIZE, false)?;
      if let Some(display_name) = &user.display_name {
        validate_text("remembered display name", display_name, MAX_DISPLAY_NAME_SIZE, true)?;
      }
    }
    if let Some(selection) = &self.global_selection {
      validate_selection(selection)?;
    }
    for (username, selection) in &self.user_selections {
      validate_text("per-user cache key", username, MAX_USERNAME_SIZE, false)?;
      validate_selection(selection)?;
    }

    Ok(())
  }
}

impl CacheUpdate {
  pub(crate) fn successful_login(
    user: RememberedUser,
    selection: Option<RememberedSelection>,
    remember_user: bool,
    remember_global_selection: bool,
    remember_user_selection: bool,
    allow_commands: bool,
  ) -> Self {
    let selection = match selection {
      Some(RememberedSelection::Command { .. }) if !allow_commands => None,
      selection => selection,
    };

    Self {
      last_user: if remember_user {
        Change::Set(Some(user.clone()))
      } else {
        Change::Keep
      },
      global_selection: if remember_global_selection {
        Change::Set(selection.clone())
      } else {
        Change::Keep
      },
      user_selection: remember_user_selection.then_some((user.username, selection)),
      purge_commands: !allow_commands,
    }
  }

  pub(crate) fn purge_commands() -> Self {
    Self {
      last_user: Change::Keep,
      global_selection: Change::Keep,
      user_selection: None,
      purge_commands: true,
    }
  }
}

impl CacheStore {
  pub(crate) fn disabled() -> Self {
    Self { root: None }
  }

  pub(crate) fn system() -> Self {
    Self {
      root: Some(Arc::new(PathBuf::from(SYSTEM_CACHE_DIRECTORY))),
    }
  }

  pub(crate) fn for_runtime(mock: bool) -> Self {
    if mock { Self::disabled() } else { Self::system() }
  }

  #[cfg(test)]
  pub(crate) fn at(path: impl Into<PathBuf>) -> Self {
    Self {
      root: Some(Arc::new(path.into())),
    }
  }

  pub(crate) fn is_enabled(&self) -> bool {
    self.root.is_some()
  }

  pub(crate) fn load(&self, sessions: &[Session], allow_commands: bool, warn_if_missing: bool) -> CacheLoad {
    let Some(root) = &self.root else {
      return CacheLoad::default();
    };

    match load_from_disk(root, sessions, allow_commands) {
      Ok(load) => load,
      Err(error) if error.kind() == io::ErrorKind::NotFound && !warn_if_missing => CacheLoad::default(),
      Err(error) => CacheLoad {
        state: CacheState::default(),
        warnings: vec![format!("failed to load cache: {error}")],
      },
    }
  }

  pub(crate) fn commit(&self, update: CacheUpdate) -> Result<CacheCommit, CacheError> {
    let Some(root) = &self.root else {
      let mut state = CacheState::default();
      state.apply(update);
      return Ok(CacheCommit {
        state,
        warnings: Vec::new(),
      });
    };

    let locked = open_locked_directory(root).map_err(CacheError::from_io)?;
    let mut warnings = Vec::new();
    let mut state = match read_state(&locked.directory).map_err(CacheError::from_io)? {
      StoredState::Missing => CacheState::default(),
      StoredState::Current(state) => state,
      StoredState::Corrupt(error) => {
        warnings.push(format!("cache state became corrupt and was replaced: {error}"));
        CacheState::default()
      },
      StoredState::Future(version) => {
        return Err(CacheError(format!(
          "cache uses unsupported version {version}; refusing to overwrite it"
        )));
      },
    };

    state.apply(update);
    state.validate()?;
    write_state(&locked.directory, &state).map_err(CacheError::from_io)?;
    Ok(CacheCommit { state, warnings })
  }

  pub(crate) fn purge_commands(&self) -> Result<CacheCommit, CacheError> {
    self.commit(CacheUpdate::purge_commands())
  }
}

impl fmt::Display for CacheError {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str(&self.0)
  }
}

impl CacheError {
  fn from_io(error: io::Error) -> Self {
    Self(error.to_string())
  }
}

fn validate_text(name: &str, value: &str, max_size: usize, allow_empty: bool) -> Result<(), CacheError> {
  if (!allow_empty && value.is_empty()) || value.as_bytes().contains(&0) {
    return Err(CacheError(format!("{name} is empty or contains a NUL byte")));
  }
  if value.len() > max_size {
    return Err(CacheError(format!("{name} exceeds {max_size} bytes")));
  }
  Ok(())
}

fn validate_selection(selection: &RememberedSelection) -> Result<(), CacheError> {
  match selection {
    RememberedSelection::DesktopEntry { desktop_id, .. } => {
      validate_text("remembered desktop ID", desktop_id, MAX_SESSION_ID_SIZE, false)
    },
    RememberedSelection::Command { command } => {
      validate_text("remembered command", command, MAX_COMMAND_SIZE, false)?;
      if command.trim().is_empty() {
        return Err(CacheError("remembered command is only whitespace".into()));
      }
      Ok(())
    },
  }
}

fn load_from_disk(root: &Path, sessions: &[Session], allow_commands: bool) -> Result<CacheLoad, io::Error> {
  let locked = open_locked_directory(root)?;
  let mut warnings = Vec::new();

  let mut state = match read_state(&locked.directory)? {
    StoredState::Missing => {
      let migration = migrate_legacy(&locked.directory, sessions, &mut warnings)?;
      if migration.seen {
        write_state(&locked.directory, &migration.state)?;
        remove_legacy(&locked.directory, &migration.files, &mut warnings);
        if let Err(error) = fsync(&locked.directory).map_err(nix_error) {
          warnings.push(format!("failed to persist legacy cache cleanup: {error}"));
        }
      }
      migration.state
    },
    StoredState::Current(state) => state,
    StoredState::Corrupt(error) => {
      warnings.push(format!("cache state is corrupt and was ignored: {error}"));
      CacheState::default()
    },
    StoredState::Future(version) => {
      warnings.push(format!(
        "cache state uses unsupported version {version} and was ignored without modifying it"
      ));
      return Ok(CacheLoad {
        state: CacheState::default(),
        warnings,
      });
    },
  };

  if let Err(error) = state.validate() {
    warnings.push(format!("cache state is invalid and was ignored: {error}"));
    state = CacheState::default();
  }

  if !allow_commands
    && state.remove_commands()
    && let Err(error) = write_state(&locked.directory, &state)
  {
    warnings.push(format!(
      "failed to remove disabled command entries from the cache: {error}"
    ));
  }

  Ok(CacheLoad { state, warnings })
}

fn open_locked_directory(root: &Path) -> Result<LockedDirectory, io::Error> {
  let directory = open(
    root,
    OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
    Mode::empty(),
  )
  .map_err(nix_error)?;
  validate_directory(&directory)?;

  let lock = openat(
    &directory,
    LOCK_FILE,
    OFlag::O_RDWR | OFlag::O_CREAT | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK,
    Mode::from_bits_truncate(0o600),
  )
  .map_err(nix_error)?;
  validate_file(&lock, LOCK_FILE, 0, true)?;
  let lock = lock_cache_file(File::from(lock))?;

  Ok(LockedDirectory { directory, _lock: lock })
}

fn lock_cache_file(mut file: File) -> Result<Flock<File>, io::Error> {
  let deadline = Instant::now() + LOCK_TIMEOUT;
  loop {
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
      Ok(lock) => return Ok(lock),
      Err((returned, error)) if matches!(error, Errno::EINTR | Errno::EAGAIN) && Instant::now() < deadline => {
        file = returned;
        std::thread::sleep(LOCK_RETRY_INTERVAL);
      },
      Err((_, Errno::EINTR | Errno::EAGAIN)) => {
        return Err(io::Error::new(
          io::ErrorKind::TimedOut,
          "timed out waiting for the cache lock",
        ));
      },
      Err((_, error)) => return Err(nix_error(error)),
    }
  }
}

fn validate_directory(directory: &OwnedFd) -> Result<(), io::Error> {
  let metadata = fstat(directory).map_err(nix_error)?;
  if metadata.st_mode & SFlag::S_IFMT.bits() != SFlag::S_IFDIR.bits() {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "cache path is not a directory",
    ));
  }
  if metadata.st_uid != effective_uid() {
    return Err(io::Error::new(
      io::ErrorKind::PermissionDenied,
      "cache directory is not owned by the greeter user",
    ));
  }

  let mode = metadata.st_mode & 0o7777;
  if mode & 0o022 != 0 {
    return Err(io::Error::new(
      io::ErrorKind::PermissionDenied,
      format!("cache directory mode {mode:#06o} permits writes by other users"),
    ));
  }
  if mode != 0o700 {
    fchmod(directory, Mode::from_bits_truncate(0o700)).map_err(nix_error)?;
    let mode = fstat(directory).map_err(nix_error)?.st_mode & 0o7777;
    if mode != 0o700 {
      return Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("could not restrict cache directory mode to 0700 (got {mode:#06o})"),
      ));
    }
  }

  Ok(())
}

fn validate_file(fd: &OwnedFd, name: &str, max_size: u64, allow_empty: bool) -> Result<(), io::Error> {
  let metadata = fstat(fd).map_err(nix_error)?;
  if metadata.st_mode & SFlag::S_IFMT.bits() != SFlag::S_IFREG.bits() {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      format!("cache file {name:?} is not a regular file"),
    ));
  }
  if metadata.st_nlink != 1 {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      format!("cache file {name:?} has more than one hard link"),
    ));
  }
  if metadata.st_uid != effective_uid() {
    return Err(io::Error::new(
      io::ErrorKind::PermissionDenied,
      format!("cache file {name:?} is not owned by the greeter user"),
    ));
  }
  let mode = metadata.st_mode & 0o7777;
  if mode & 0o022 != 0 {
    return Err(io::Error::new(
      io::ErrorKind::PermissionDenied,
      format!("cache file {name:?} mode {mode:#06o} permits writes by other users"),
    ));
  }
  if mode != 0o600 {
    fchmod(fd, Mode::from_bits_truncate(0o600)).map_err(nix_error)?;
  }
  let size = u64::try_from(metadata.st_size).unwrap_or(u64::MAX);
  if (!allow_empty && size == 0) || size > max_size {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      format!("cache file {name:?} has invalid size {size}"),
    ));
  }

  Ok(())
}

fn read_state(directory: &OwnedFd) -> Result<StoredState, io::Error> {
  let fd = match openat(
    directory,
    STATE_FILE,
    OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK,
    Mode::empty(),
  ) {
    Ok(fd) => fd,
    Err(Errno::ENOENT) => return Ok(StoredState::Missing),
    Err(error) => return Err(nix_error(error)),
  };
  if let Err(error) = validate_file(&fd, STATE_FILE, MAX_STATE_SIZE, false) {
    return Ok(StoredState::Corrupt(error.to_string()));
  }
  let bytes = read_fd(fd, MAX_STATE_SIZE)?;
  let header = match serde_json::from_slice::<VersionHeader>(&bytes) {
    Ok(header) => header,
    Err(error) => return Ok(StoredState::Corrupt(format!("invalid JSON: {error}"))),
  };
  if header.version != STATE_VERSION {
    return Ok(StoredState::Future(header.version));
  }
  let file = match serde_json::from_slice::<CacheFile>(&bytes) {
    Ok(file) => file,
    Err(error) => return Ok(StoredState::Corrupt(format!("invalid version 1 data: {error}"))),
  };

  let state = CacheState {
    last_user: file.last_user,
    global_selection: file.global_selection,
    user_selections: file.user_selections,
  };
  if let Err(error) = state.validate() {
    return Ok(StoredState::Corrupt(format!("invalid version 1 data: {error}")));
  }

  Ok(StoredState::Current(state))
}

fn write_state(directory: &OwnedFd, state: &CacheState) -> Result<(), io::Error> {
  state
    .validate()
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
  let file = CacheFile {
    version: STATE_VERSION,
    last_user: state.last_user.clone(),
    global_selection: state.global_selection.clone(),
    user_selections: state.user_selections.clone(),
  };
  let mut bytes =
    serde_json::to_vec_pretty(&file).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
  bytes.push(b'\n');
  if bytes.len() as u64 > MAX_STATE_SIZE {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "serialized cache exceeds the size limit",
    ));
  }

  match unlinkat(directory, TEMP_FILE, UnlinkatFlags::NoRemoveDir) {
    Ok(()) | Err(Errno::ENOENT) => {},
    Err(error) => return Err(nix_error(error)),
  }
  let temp = openat(
    directory,
    TEMP_FILE,
    OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
    Mode::from_bits_truncate(0o600),
  )
  .map_err(nix_error)?;
  validate_file(&temp, TEMP_FILE, MAX_STATE_SIZE, true)?;
  let mut temp = File::from(temp);

  let result = (|| {
    temp.write_all(&bytes)?;
    temp.sync_all()?;
    renameat(directory, TEMP_FILE, directory, STATE_FILE).map_err(nix_error)?;
    fsync(directory).map_err(nix_error)
  })();
  if result.is_err() {
    let _ = unlinkat(directory, TEMP_FILE, UnlinkatFlags::NoRemoveDir);
  }
  result
}

struct LegacyMigration {
  state: CacheState,
  files: Vec<Vec<u8>>,
  seen: bool,
}

fn migrate_legacy(
  directory: &OwnedFd,
  sessions: &[Session],
  warnings: &mut Vec<String>,
) -> Result<LegacyMigration, io::Error> {
  let names = directory_entries(directory)?;
  let mut files = Vec::new();
  let mut seen = false;
  let mut read = |name: &[u8]| {
    seen = true;
    files.push(name.to_vec());
    read_legacy_text(directory, name, warnings)
  };

  let has = |name: &[u8]| names.iter().any(|entry| entry.as_slice() == name);
  let username = has(LEGACY_LAST_USER.as_bytes())
    .then(|| read(LEGACY_LAST_USER.as_bytes()))
    .flatten();
  let display_name = has(LEGACY_LAST_USER_NAME.as_bytes())
    .then(|| read(LEGACY_LAST_USER_NAME.as_bytes()))
    .flatten();
  let global_path = has(LEGACY_GLOBAL_SESSION.as_bytes())
    .then(|| read(LEGACY_GLOBAL_SESSION.as_bytes()))
    .flatten();
  if has(LEGACY_GLOBAL_COMMAND.as_bytes()) {
    let _ = read(LEGACY_GLOBAL_COMMAND.as_bytes());
  }

  let mut user_paths = BTreeMap::new();
  for name in &names {
    if name.as_slice() == LEGACY_GLOBAL_SESSION.as_bytes() {
      continue;
    }
    if let Some(username) = name.strip_prefix(LEGACY_USER_SESSION_PREFIX.as_bytes()) {
      if !username.is_empty()
        && let Some(value) = read(name)
        && let Ok(username) = std::str::from_utf8(username)
      {
        user_paths.insert(username.to_string(), value);
      }
    } else if let Some(username) = name.strip_prefix(LEGACY_USER_COMMAND_PREFIX.as_bytes())
      && !username.is_empty()
    {
      let _ = read(name);
    }
  }

  let display_name = display_name.and_then(|display_name| {
    if let Err(error) = validate_text("legacy display name", &display_name, MAX_DISPLAY_NAME_SIZE, true) {
      warnings.push(format!("ignored invalid legacy display name: {error}"));
      None
    } else {
      Some(display_name)
    }
  });

  let mut state = CacheState::default();
  if let Some(username) = username {
    let user = RememberedUser { username, display_name };
    if let Err(error) = validate_text("legacy username", &user.username, MAX_USERNAME_SIZE, false) {
      warnings.push(format!("ignored invalid legacy username: {error}"));
    } else {
      state.last_user = Some(user);
    }
  }

  state.global_selection = global_path
    .as_deref()
    .and_then(|path| selection_for_path(path, sessions));

  for (username, path) in user_paths {
    if validate_text("legacy per-user key", &username, MAX_USERNAME_SIZE, false).is_err() {
      warnings.push("ignored a legacy per-user cache entry with an invalid username".into());
      continue;
    }
    if let Some(selection) = selection_for_path(&path, sessions) {
      state.user_selections.insert(username, selection);
    }
  }

  if let Err(error) = state.validate() {
    warnings.push(format!("legacy cache produced invalid state and was ignored: {error}"));
    state = CacheState::default();
  }

  Ok(LegacyMigration { state, files, seen })
}

fn selection_for_path(path: &str, sessions: &[Session]) -> Option<RememberedSelection> {
  let path = Path::new(path);
  sessions
    .iter()
    .find(|session| session.path.as_deref() == Some(path))
    .and_then(RememberedSelection::from_session)
}

fn directory_entries(directory: &OwnedFd) -> Result<Vec<Vec<u8>>, io::Error> {
  let duplicate = dup(directory).map_err(nix_error)?;
  let mut directory = Dir::from_fd(duplicate).map_err(nix_error)?;
  let mut names = Vec::new();
  for entry in directory.iter() {
    let entry = entry.map_err(nix_error)?;
    let name = entry.file_name().to_bytes();
    if name != b"." && name != b".." {
      names.push(name.to_vec());
    }
  }
  names.sort();
  Ok(names)
}

fn read_legacy_text(directory: &OwnedFd, name: &[u8], warnings: &mut Vec<String>) -> Option<String> {
  let name = OsStr::from_bytes(name);
  let fd = match openat(
    directory,
    name,
    OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK,
    Mode::empty(),
  ) {
    Ok(fd) => fd,
    Err(error) => {
      warnings.push(format!(
        "ignored unsafe legacy cache file {:?}: {}",
        name,
        nix_error(error)
      ));
      return None;
    },
  };
  if let Err(error) = validate_file(&fd, &name.to_string_lossy(), MAX_COMMAND_SIZE as u64, true) {
    warnings.push(format!("ignored unsafe legacy cache file {name:?}: {error}"));
    return None;
  }
  let bytes = match read_fd(fd, MAX_COMMAND_SIZE as u64) {
    Ok(bytes) => bytes,
    Err(error) => {
      warnings.push(format!("failed to read legacy cache file {name:?}: {error}"));
      return None;
    },
  };
  match String::from_utf8(bytes) {
    Ok(value) => {
      let value = value.trim();
      (!value.is_empty()).then(|| value.to_string())
    },
    Err(_) => {
      warnings.push(format!("ignored non-UTF-8 legacy cache file {name:?}"));
      None
    },
  }
}

fn remove_legacy(directory: &OwnedFd, files: &[Vec<u8>], warnings: &mut Vec<String>) {
  for name in files {
    let name = OsStr::from_bytes(name);
    match unlinkat(directory, name, UnlinkatFlags::NoRemoveDir) {
      Ok(()) | Err(Errno::ENOENT) => {},
      Err(error) => warnings.push(format!(
        "failed to remove migrated legacy cache file {:?}: {}",
        name,
        nix_error(error)
      )),
    }
  }
}

fn read_fd(fd: OwnedFd, max_size: u64) -> Result<Vec<u8>, io::Error> {
  let mut bytes = Vec::new();
  File::from(fd).take(max_size + 1).read_to_end(&mut bytes)?;
  if bytes.len() as u64 > max_size {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "cache file exceeds the size limit",
    ));
  }
  Ok(bytes)
}

fn effective_uid() -> u32 {
  // SAFETY: POSIX specifies geteuid() as infallible and it has no preconditions.
  unsafe { nix::libc::geteuid() }
}

fn nix_error(error: Errno) -> io::Error {
  io::Error::from_raw_os_error(error as i32)
}

#[cfg(test)]
mod tests {
  use std::{
    fs,
    os::unix::{
      fs::{PermissionsExt, symlink},
      net::UnixStream,
    },
    sync::Arc,
    thread,
  };

  use tempfile::tempdir;

  use super::*;

  fn store() -> (tempfile::TempDir, CacheStore) {
    let directory = tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let store = CacheStore::at(directory.path());
    (directory, store)
  }

  fn user(name: &str) -> RememberedUser {
    RememberedUser {
      username: name.into(),
      display_name: Some(format!("{name} display")),
    }
  }

  fn command(value: &str) -> RememberedSelection {
    RememberedSelection::command(value.into())
  }

  #[test]
  fn disabled_store_never_touches_disk() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("missing");
    let store = CacheStore::disabled();
    let update = CacheUpdate::successful_login(user("alice"), Some(command("start")), true, true, true, true);

    let state = store.commit(update).unwrap().state;

    assert_eq!(state.last_user().unwrap().username, "alice");
    assert!(!path.exists());
    assert!(store.load(&[], true, false).warnings.is_empty());
    assert!(!CacheStore::for_runtime(true).is_enabled());
    assert!(CacheStore::for_runtime(false).is_enabled());
  }

  #[test]
  fn round_trips_one_transactional_state_file() {
    let (directory, store) = store();
    let update = CacheUpdate::successful_login(user("alice"), Some(command("start session")), true, true, true, true);
    store.commit(update).unwrap();

    let load = store.load(&[], true, true);

    assert!(load.warnings.is_empty());
    assert_eq!(load.state.last_user().unwrap().username, "alice");
    assert_eq!(
      load.state.global_selection().unwrap().command_value(),
      Some("start session")
    );
    assert_eq!(
      load.state.user_selection("alice").unwrap().command_value(),
      Some("start session")
    );
    assert_eq!(
      fs::metadata(directory.path()).unwrap().permissions().mode() & 0o7777,
      0o700
    );
    assert_eq!(
      fs::metadata(directory.path().join(STATE_FILE))
        .unwrap()
        .permissions()
        .mode()
        & 0o7777,
      0o600
    );
    assert_eq!(
      fs::metadata(directory.path().join(LOCK_FILE))
        .unwrap()
        .permissions()
        .mode()
        & 0o7777,
      0o600
    );
  }

  #[test]
  fn disabled_commands_are_removed_from_every_slot() {
    let (_directory, store) = store();
    store
      .commit(CacheUpdate::successful_login(
        user("alice"),
        Some(command("start")),
        true,
        true,
        true,
        true,
      ))
      .unwrap();

    let state = store.purge_commands().unwrap().state;

    assert!(state.global_selection().is_none());
    assert!(state.user_selection("alice").is_none());
    assert_eq!(state.last_user().unwrap().username, "alice");
  }

  #[test]
  fn remembering_no_selection_clears_only_the_requested_slots() {
    let (_directory, store) = store();
    store
      .commit(CacheUpdate::successful_login(
        user("alice"),
        Some(command("start")),
        true,
        true,
        true,
        true,
      ))
      .unwrap();

    let state = store
      .commit(CacheUpdate::successful_login(
        user("alice"),
        None,
        false,
        true,
        true,
        true,
      ))
      .unwrap()
      .state;

    assert_eq!(state.last_user().unwrap().username, "alice");
    assert!(state.global_selection().is_none());
    assert!(state.user_selection("alice").is_none());
  }

  #[test]
  fn future_versions_are_never_overwritten() {
    let (directory, store) = store();
    let path = directory.path().join(STATE_FILE);
    fs::write(&path, b"{\"version\":2}\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let original = fs::read(&path).unwrap();

    let load = store.load(&[], true, true);
    let result = store.commit(CacheUpdate::purge_commands());

    assert_eq!(load.warnings.len(), 1);
    assert!(result.is_err());
    assert_eq!(fs::read(path).unwrap(), original);
  }

  #[test]
  fn corrupt_current_state_can_recover_on_the_next_commit() {
    let (directory, store) = store();
    let path = directory.path().join(STATE_FILE);
    fs::write(&path, b"not json").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(store.load(&[], true, true).warnings.len(), 1);

    let commit = store
      .commit(CacheUpdate::successful_login(
        user("alice"),
        None,
        true,
        false,
        false,
        false,
      ))
      .unwrap();

    assert_eq!(commit.warnings.len(), 1);
    assert_eq!(store.load(&[], true, true).state.last_user().unwrap().username, "alice");
  }

  #[test]
  fn semantically_invalid_current_state_can_recover_on_the_next_commit() {
    let (directory, store) = store();
    let path = directory.path().join(STATE_FILE);
    fs::write(&path, br#"{"version":1,"last_user":{"username":""}}"#).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(store.load(&[], true, true).warnings.len(), 1);

    let commit = store
      .commit(CacheUpdate::successful_login(
        user("alice"),
        None,
        true,
        false,
        false,
        false,
      ))
      .unwrap();

    assert_eq!(commit.warnings.len(), 1);
    assert_eq!(store.load(&[], true, true).state.last_user().unwrap().username, "alice");
  }

  #[test]
  fn state_symlinks_are_refused_without_touching_the_target() {
    let (directory, store) = store();
    let target = directory.path().join("target");
    fs::write(&target, b"do not touch").unwrap();
    symlink(&target, directory.path().join(STATE_FILE)).unwrap();

    assert_eq!(store.load(&[], true, true).warnings.len(), 1);
    assert!(store.commit(CacheUpdate::purge_commands()).is_err());
    assert_eq!(fs::read(target).unwrap(), b"do not touch");
  }

  #[test]
  fn lock_symlinks_are_refused_without_touching_the_target() {
    let (directory, store) = store();
    let target = directory.path().join("target");
    fs::write(&target, b"do not touch").unwrap();
    symlink(&target, directory.path().join(LOCK_FILE)).unwrap();

    assert_eq!(store.load(&[], true, true).warnings.len(), 1);
    assert!(store.commit(CacheUpdate::purge_commands()).is_err());
    assert_eq!(fs::read(target).unwrap(), b"do not touch");
  }

  #[test]
  fn non_regular_and_oversized_state_files_are_rejected_without_blocking() {
    let (directory, store) = store();
    let path = directory.path().join(STATE_FILE);
    nix::unistd::mkfifo(&path, Mode::from_bits_truncate(0o600)).unwrap();
    assert_eq!(store.load(&[], true, true).warnings.len(), 1);

    fs::remove_file(&path).unwrap();
    fs::write(&path, vec![b'x'; MAX_STATE_SIZE as usize + 1]).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(store.load(&[], true, true).warnings.len(), 1);
  }

  #[test]
  fn file_type_validation_requires_an_exact_regular_file() {
    let (stream, _peer) = UnixStream::pair().unwrap();
    let fd = OwnedFd::from(stream);

    assert!(validate_file(&fd, "socket", MAX_STATE_SIZE, true).is_err());
  }

  #[test]
  fn failed_atomic_replacement_preserves_the_previous_state() {
    let (directory, store) = store();
    store
      .commit(CacheUpdate::successful_login(
        user("alice"),
        None,
        true,
        false,
        false,
        false,
      ))
      .unwrap();
    let state_path = directory.path().join(STATE_FILE);
    let original = fs::read(&state_path).unwrap();
    fs::create_dir(directory.path().join(TEMP_FILE)).unwrap();

    let result = store.commit(CacheUpdate::successful_login(
      user("bob"),
      None,
      true,
      false,
      false,
      false,
    ));

    assert!(result.is_err());
    assert_eq!(fs::read(state_path).unwrap(), original);
  }

  #[test]
  fn unsafe_directory_permissions_are_not_trusted() {
    let (directory, store) = store();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o770)).unwrap();

    assert_eq!(store.load(&[], true, true).warnings.len(), 1);
    assert!(store.commit(CacheUpdate::purge_commands()).is_err());
    assert!(!directory.path().join(STATE_FILE).exists());
  }

  #[test]
  fn concurrent_updates_do_not_lose_user_entries() {
    let (_directory, store) = store();
    let store = Arc::new(store);
    let threads = (0..16)
      .map(|index| {
        let store = Arc::clone(&store);
        thread::spawn(move || {
          let username = format!("user-{index}");
          store
            .commit(CacheUpdate::successful_login(
              user(&username),
              Some(command(&format!("session-{index}"))),
              false,
              false,
              true,
              true,
            ))
            .unwrap();
        })
      })
      .collect::<Vec<_>>();
    for thread in threads {
      thread.join().unwrap();
    }

    let state = store.load(&[], true, true).state;
    assert_eq!(state.user_selections.len(), 16);
    for index in 0..16 {
      assert_eq!(
        state.user_selection(&format!("user-{index}")).unwrap().command_value(),
        Some(format!("session-{index}").as_str())
      );
    }
  }

  #[test]
  fn legacy_migration_prefers_desktop_entries_and_handles_hyphenated_users() {
    let (directory, store) = store();
    let desktop_path = directory.path().join("session.desktop");
    let session = Session {
      slug: Some("session.desktop".into()),
      name: "Session".into(),
      command: "start-session".into(),
      session_type: SessionType::Wayland,
      path: Some(desktop_path.clone()),
      xdg_desktop_names: None,
    };
    fs::write(directory.path().join(LEGACY_LAST_USER), "alice-user").unwrap();
    fs::write(directory.path().join(LEGACY_LAST_USER_NAME), "Alice User").unwrap();
    fs::write(
      directory.path().join(LEGACY_GLOBAL_SESSION),
      desktop_path.to_string_lossy().as_bytes(),
    )
    .unwrap();
    fs::write(directory.path().join(LEGACY_GLOBAL_COMMAND), "unsafe fallback").unwrap();
    fs::write(
      directory.path().join(format!("{LEGACY_USER_SESSION_PREFIX}alice-user")),
      desktop_path.to_string_lossy().as_bytes(),
    )
    .unwrap();
    fs::write(
      directory.path().join(format!("{LEGACY_USER_COMMAND_PREFIX}alice-user")),
      "unsafe fallback",
    )
    .unwrap();

    let state = store.load(&[session], true, true).state;

    assert_eq!(state.last_user().unwrap().username, "alice-user");
    assert!(matches!(
      state.global_selection(),
      Some(RememberedSelection::DesktopEntry { .. })
    ));
    assert!(matches!(
      state.user_selection("alice-user"),
      Some(RememberedSelection::DesktopEntry { .. })
    ));
    assert!(state.user_selection("path").is_none());
    assert!(directory.path().join(STATE_FILE).is_file());
    assert!(!directory.path().join(LEGACY_LAST_USER).exists());
    assert!(!directory.path().join(LEGACY_GLOBAL_COMMAND).exists());
  }

  #[test]
  fn legacy_free_form_commands_are_never_promoted_to_trusted_state() {
    let (directory, store) = store();
    fs::write(directory.path().join(LEGACY_GLOBAL_COMMAND), "untrusted global command").unwrap();
    fs::write(
      directory.path().join(format!("{LEGACY_USER_COMMAND_PREFIX}alice")),
      "untrusted user command",
    )
    .unwrap();

    let state = store.load(&[], true, true).state;

    assert!(state.global_selection().is_none());
    assert!(state.user_selection("alice").is_none());
    assert!(directory.path().join(STATE_FILE).is_file());
    assert!(!directory.path().join(LEGACY_GLOBAL_COMMAND).exists());
    assert!(
      !directory
        .path()
        .join(format!("{LEGACY_USER_COMMAND_PREFIX}alice"))
        .exists()
    );
  }

  #[test]
  fn failed_migration_keeps_every_legacy_file() {
    let (directory, store) = store();
    let legacy = directory.path().join(LEGACY_LAST_USER);
    fs::write(&legacy, "alice").unwrap();
    fs::create_dir(directory.path().join(TEMP_FILE)).unwrap();

    let load = store.load(&[], true, true);

    assert_eq!(load.warnings.len(), 1);
    assert!(legacy.exists());
    assert!(!directory.path().join(STATE_FILE).exists());
  }
}
