use std::{
  fs::OpenOptions,
  io::{self, Read},
  os::unix::fs::OpenOptionsExt,
  panic::{AssertUnwindSafe, catch_unwind},
  path::{Path, PathBuf},
  sync::mpsc,
  thread::{self, JoinHandle},
  time::Duration,
};

use nix::fcntl::OFlag;
use tokio::{
  sync::{oneshot, watch},
  time::timeout,
};

use crate::config::MAX_CONFIG_SIZE;

const POLL_INTERVAL: Duration = Duration::from_secs(1);
const SETTLE_DELAY: Duration = Duration::from_millis(100);
const MAX_SETTLE_TIME: Duration = Duration::from_secs(1);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Clone, Copy)]
struct WatchTimings {
  poll_interval: Duration,
  settle_delay: Duration,
  max_settle_time: Duration,
  shutdown_timeout: Duration,
}

impl Default for WatchTimings {
  fn default() -> Self {
    Self {
      poll_interval: POLL_INTERVAL,
      settle_delay: SETTLE_DELAY,
      max_settle_time: MAX_SETTLE_TIME,
      shutdown_timeout: SHUTDOWN_TIMEOUT,
    }
  }
}

#[derive(Debug, Eq, PartialEq)]
enum Fingerprint {
  Missing,
  Contents(Vec<u8>),
  Unreadable {
    kind: io::ErrorKind,
    raw_os_error: Option<i32>,
  },
}

impl Fingerprint {
  fn read(path: &Path) -> Self {
    match read_regular_file(path) {
      Ok(contents) => Self::Contents(contents),
      Err(error) if error.kind() == io::ErrorKind::NotFound => Self::Missing,
      Err(error) => Self::from_error(error),
    }
  }

  fn from_error(error: io::Error) -> Self {
    Self::Unreadable {
      kind: error.kind(),
      raw_os_error: error.raw_os_error(),
    }
  }
}

fn read_regular_file(path: &Path) -> io::Result<Vec<u8>> {
  // O_NONBLOCK prevents a mistaken FIFO/device config path from pinning a
  // worker. Symlinks remain supported and are validated after opening.
  let mut file = OpenOptions::new()
    .read(true)
    .custom_flags((OFlag::O_NONBLOCK | OFlag::O_CLOEXEC | OFlag::O_NOCTTY).bits())
    .open(path)?;
  let metadata = file.metadata()?;
  if !metadata.file_type().is_file() {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "configuration path is not a regular file",
    ));
  }
  if metadata.len() > MAX_CONFIG_SIZE as u64 {
    return Err(config_too_large_error());
  }
  let mut contents = Vec::with_capacity(metadata.len() as usize);
  (&mut file)
    .take((MAX_CONFIG_SIZE + 1) as u64)
    .read_to_end(&mut contents)?;
  if contents.len() > MAX_CONFIG_SIZE {
    return Err(config_too_large_error());
  }
  Ok(contents)
}

fn config_too_large_error() -> io::Error {
  io::Error::new(
    io::ErrorKind::InvalidData,
    format!("configuration exceeds the {MAX_CONFIG_SIZE}-byte size limit"),
  )
}

pub(crate) enum WatchOutcome {
  Changed,
  Stopped(String),
}

/// Owns the polling task and its coalesced change notification.
///
/// A watch channel is deliberate here: configuration reloads always read the
/// latest complete files, so retaining more than one unhandled notification
/// cannot add information and must never exert backpressure on terminal input.
pub(crate) struct ConfigWatcher {
  changes: Option<watch::Receiver<u64>>,
  worker: Option<WatchWorker>,
  startup_failure: Option<String>,
}

pub(crate) struct WatchBaseline {
  paths: Vec<PathBuf>,
  fingerprints: Vec<Fingerprint>,
}

struct WatchWorker {
  shutdown: Option<mpsc::Sender<()>>,
  stopped: oneshot::Receiver<WorkerExit>,
  handle: Option<JoinHandle<()>>,
  shutdown_timeout: Duration,
}

enum WorkerExit {
  Stopped,
  Panicked(String),
}

impl ConfigWatcher {
  pub(crate) fn capture(paths: Vec<PathBuf>) -> WatchBaseline {
    let fingerprints = fingerprints(&paths);
    WatchBaseline { paths, fingerprints }
  }

  pub(crate) fn spawn(baseline: WatchBaseline) -> Self {
    Self::spawn_with_timings(baseline, WatchTimings::default())
  }

  #[cfg(test)]
  pub(crate) fn disabled() -> Self {
    Self {
      changes: None,
      worker: None,
      startup_failure: None,
    }
  }

  fn spawn_with_timings(baseline: WatchBaseline, timings: WatchTimings) -> Self {
    let WatchBaseline { paths, fingerprints } = baseline;
    let (changes_tx, changes) = watch::channel(0_u64);
    let (shutdown, shutdown_rx) = mpsc::channel();
    let (stopped_tx, stopped) = oneshot::channel();
    let handle = thread::Builder::new()
      .name("tuigreet-config-watch".into())
      .spawn(move || {
        let exit = match catch_unwind(AssertUnwindSafe(|| {
          watch_files(paths, fingerprints, changes_tx, &shutdown_rx, timings);
        })) {
          Ok(()) => WorkerExit::Stopped,
          Err(payload) => WorkerExit::Panicked(panic_message(payload)),
        };
        let _ = stopped_tx.send(exit);
      });

    let Ok(handle) = handle else {
      return Self {
        changes: None,
        worker: None,
        startup_failure: Some("could not start the configuration watcher thread".into()),
      };
    };
    Self {
      changes: Some(changes),
      worker: Some(WatchWorker {
        shutdown: Some(shutdown),
        stopped,
        handle: Some(handle),
        shutdown_timeout: timings.shutdown_timeout,
      }),
      startup_failure: None,
    }
  }

  pub(crate) fn has_active(&self) -> bool {
    self.worker.is_some() || self.startup_failure.is_some()
  }

  /// Wait for a coalesced change or unexpected worker termination.
  ///
  /// Both `watch::Receiver::changed` and `&mut JoinHandle` are cancel-safe, so
  /// an outer `tokio::select!` may freely drop this future.
  pub(crate) async fn next(&mut self) -> WatchOutcome {
    if let Some(message) = self.startup_failure.take() {
      return WatchOutcome::Stopped(message);
    }
    let Some(_) = self.changes.as_mut() else {
      return WatchOutcome::Stopped("configuration watcher is disabled".into());
    };
    let Some(_) = self.worker.as_mut() else {
      return WatchOutcome::Stopped("configuration watcher has already stopped".into());
    };

    enum Ready {
      Worker(Result<WorkerExit, oneshot::error::RecvError>),
      Changed(Result<(), tokio::sync::watch::error::RecvError>),
    }

    let ready = {
      let changes = self.changes.as_mut().unwrap();
      let worker = self.worker.as_mut().unwrap();
      tokio::select! {
        biased;
        result = &mut worker.stopped => Ready::Worker(result),
        changed = changes.changed() => Ready::Changed(changed),
      }
    };

    match ready {
      Ready::Changed(Ok(())) => WatchOutcome::Changed,
      Ready::Worker(result) => {
        let worker = self.worker.take().unwrap();
        self.changes = None;
        join_finished(worker);
        stopped_outcome(result)
      },
      Ready::Changed(Err(_)) => {
        self.changes = None;
        if let Some(mut worker) = self.worker.take() {
          if let Ok(result) = timeout(worker.shutdown_timeout, &mut worker.stopped).await {
            join_finished(worker);
            return stopped_outcome(result);
          }
        }
        WatchOutcome::Stopped("configuration watcher notification channel closed".into())
      },
    }
  }

  pub(crate) async fn shutdown(&mut self) {
    let Some(mut worker) = self.worker.take() else {
      return;
    };
    if let Some(shutdown) = worker.shutdown.take() {
      let _ = shutdown.send(());
    }
    if timeout(worker.shutdown_timeout, &mut worker.stopped).await.is_ok() {
      join_finished(worker);
    }
    self.changes = None;
  }
}

fn stopped_outcome(result: Result<WorkerExit, oneshot::error::RecvError>) -> WatchOutcome {
  WatchOutcome::Stopped(match result {
    Ok(WorkerExit::Stopped) => "configuration watcher stopped unexpectedly".into(),
    Ok(WorkerExit::Panicked(message)) => format!("configuration watcher panicked: {message}"),
    Err(error) => format!("configuration watcher status channel failed: {error}"),
  })
}

fn join_finished(mut worker: WatchWorker) {
  if let Some(handle) = worker.handle.take() {
    let _ = handle.join();
  }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
  payload
    .downcast_ref::<&str>()
    .map(|message| (*message).to_string())
    .or_else(|| payload.downcast_ref::<String>().cloned())
    .unwrap_or_else(|| "unknown panic payload".into())
}

impl Drop for ConfigWatcher {
  fn drop(&mut self) {
    if let Some(worker) = self.worker.as_mut() {
      if let Some(shutdown) = worker.shutdown.take() {
        let _ = shutdown.send(());
      }
    }
  }
}

fn watch_files(
  paths: Vec<PathBuf>,
  mut known: Vec<Fingerprint>,
  changes: watch::Sender<u64>,
  shutdown: &mpsc::Receiver<()>,
  timings: WatchTimings,
) {
  loop {
    match shutdown.recv_timeout(timings.poll_interval) {
      Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
      Err(mpsc::RecvTimeoutError::Timeout) => {},
    }

    let candidate = fingerprints(&paths);
    if candidate == known {
      continue;
    }

    let Some(settled) = settle(&paths, candidate, shutdown, timings) else {
      return;
    };
    if settled == known {
      continue;
    }

    known = settled;
    let next = (*changes.borrow()).wrapping_add(1);
    if changes.send(next).is_err() {
      return;
    }
  }
}

fn settle(
  paths: &[PathBuf],
  mut candidate: Vec<Fingerprint>,
  shutdown: &mpsc::Receiver<()>,
  timings: WatchTimings,
) -> Option<Vec<Fingerprint>> {
  let deadline = std::time::Instant::now() + timings.max_settle_time;
  loop {
    let delay = timings
      .settle_delay
      .min(deadline.saturating_duration_since(std::time::Instant::now()));
    match shutdown.recv_timeout(delay) {
      Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return None,
      Err(mpsc::RecvTimeoutError::Timeout) => {},
    }

    let sampled = fingerprints(paths);
    if sampled == candidate || std::time::Instant::now() >= deadline {
      return Some(sampled);
    }
    candidate = sampled;
  }
}

fn fingerprints(paths: &[PathBuf]) -> Vec<Fingerprint> {
  paths.iter().map(|path| Fingerprint::read(path)).collect()
}

#[cfg(test)]
mod tests {
  use std::fs;

  use tempfile::tempdir;

  use super::*;

  fn test_timings() -> WatchTimings {
    WatchTimings {
      poll_interval: Duration::from_millis(10),
      settle_delay: Duration::from_millis(15),
      max_settle_time: Duration::from_millis(75),
      shutdown_timeout: Duration::from_millis(100),
    }
  }

  fn spawn(paths: Vec<PathBuf>) -> ConfigWatcher {
    ConfigWatcher::spawn_with_timings(ConfigWatcher::capture(paths), test_timings())
  }

  async fn changed(watcher: &mut ConfigWatcher) {
    assert!(matches!(
      timeout(Duration::from_secs(1), watcher.next()).await,
      Ok(WatchOutcome::Changed)
    ));
  }

  #[tokio::test]
  async fn create_delete_and_atomic_replace_are_observed() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let mut watcher = spawn(vec![path.clone()]);

    fs::write(&path, b"one").unwrap();
    changed(&mut watcher).await;

    let replacement = directory.path().join("replacement.toml");
    fs::write(&replacement, b"two").unwrap();
    fs::rename(replacement, &path).unwrap();
    changed(&mut watcher).await;

    fs::remove_file(&path).unwrap();
    changed(&mut watcher).await;
    watcher.shutdown().await;
  }

  #[tokio::test]
  async fn a_change_between_startup_capture_and_worker_spawn_is_observed() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let baseline = ConfigWatcher::capture(vec![path.clone()]);

    fs::write(&path, b"created during startup").unwrap();
    let mut watcher = ConfigWatcher::spawn_with_timings(baseline, test_timings());

    changed(&mut watcher).await;
    watcher.shutdown().await;
  }

  #[tokio::test]
  async fn invalid_then_valid_contents_produce_distinct_changes() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(&path, b"valid = true").unwrap();
    let mut watcher = spawn(vec![path.clone()]);

    fs::write(&path, b"value = = broken").unwrap();
    changed(&mut watcher).await;
    fs::write(&path, b"valid = false").unwrap();
    changed(&mut watcher).await;
    watcher.shutdown().await;
  }

  #[tokio::test]
  async fn unexpected_worker_termination_is_reported_once() {
    let (_changes_tx, changes) = watch::channel(0_u64);
    let (stopped_tx, stopped) = oneshot::channel();
    stopped_tx.send(WorkerExit::Stopped).ok().unwrap();
    let mut watcher = ConfigWatcher {
      changes: Some(changes),
      worker: Some(WatchWorker {
        shutdown: None,
        stopped,
        handle: None,
        shutdown_timeout: test_timings().shutdown_timeout,
      }),
      startup_failure: None,
    };

    let outcome = timeout(Duration::from_secs(1), watcher.next())
      .await
      .expect("stopped watcher did not report its termination");
    assert!(matches!(outcome, WatchOutcome::Stopped(message) if message.contains("stopped unexpectedly")));
    assert!(!watcher.has_active());
  }

  #[tokio::test]
  async fn a_closed_notification_channel_preserves_the_worker_panic() {
    let (changes_tx, changes) = watch::channel(0_u64);
    drop(changes_tx);
    let (stopped_tx, stopped) = oneshot::channel();
    stopped_tx
      .send(WorkerExit::Panicked("injected watcher panic".into()))
      .ok()
      .unwrap();
    let mut watcher = ConfigWatcher {
      changes: Some(changes),
      worker: Some(WatchWorker {
        shutdown: None,
        stopped,
        handle: None,
        shutdown_timeout: test_timings().shutdown_timeout,
      }),
      startup_failure: None,
    };

    let outcome = timeout(Duration::from_secs(1), watcher.next())
      .await
      .expect("panicked watcher did not report its termination");
    assert!(matches!(outcome, WatchOutcome::Stopped(message) if message.contains("panicked")));
    assert!(!watcher.has_active());
  }

  #[test]
  fn a_change_that_reverts_while_settling_is_ignored() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(&path, b"original").unwrap();
    fs::write(&path, b"temporary").unwrap();
    let candidate = fingerprints(std::slice::from_ref(&path));
    fs::write(&path, b"original").unwrap();
    let expected = fingerprints(std::slice::from_ref(&path));
    assert_ne!(candidate, expected);

    let mut timings = test_timings();
    timings.settle_delay = Duration::ZERO;
    let (_shutdown, shutdown) = mpsc::channel();
    let settled =
      settle(std::slice::from_ref(&path), candidate, &shutdown, timings).expect("settling stopped unexpectedly");

    assert_eq!(settled, expected);
  }

  #[test]
  fn shutdown_interrupts_settling() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(&path, b"candidate").unwrap();
    let candidate = fingerprints(std::slice::from_ref(&path));
    let mut timings = test_timings();
    timings.settle_delay = Duration::from_secs(5);
    timings.max_settle_time = Duration::from_secs(5);
    let (shutdown, shutdown_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    let worker_path = path.clone();
    let worker = thread::spawn(move || {
      result_tx
        .send(settle(&[worker_path], candidate, &shutdown_rx, timings))
        .unwrap();
    });
    thread::sleep(Duration::from_millis(20));
    shutdown.send(()).unwrap();

    let settled = result_rx
      .recv_timeout(Duration::from_millis(150))
      .expect("shutdown was blocked by settling");
    assert!(settled.is_none());
    worker.join().unwrap();
  }

  #[test]
  fn a_non_regular_path_is_not_treated_as_missing() {
    let directory = tempdir().unwrap();
    assert!(matches!(Fingerprint::read(directory.path()), Fingerprint::Unreadable {
      kind: io::ErrorKind::InvalidData,
      ..
    }));
  }

  #[test]
  fn an_oversized_path_is_fingerprinted_without_reading_its_body() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("oversized.toml");
    let file = fs::File::create(&path).unwrap();
    file.set_len((MAX_CONFIG_SIZE + 1) as u64).unwrap();

    assert!(matches!(Fingerprint::read(&path), Fingerprint::Unreadable {
      kind: io::ErrorKind::InvalidData,
      ..
    }));
  }
}
