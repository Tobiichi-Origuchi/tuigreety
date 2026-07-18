use std::{
  fs::OpenOptions,
  io::{self, Read},
  os::unix::fs::OpenOptionsExt,
  path::{Path, PathBuf},
  time::Duration,
};

use nix::fcntl::OFlag;
use tokio::{
  sync::{oneshot, watch},
  task::JoinHandle,
  time::{Instant, MissedTickBehavior, interval_at, sleep_until, timeout},
};

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
  // Tokio worker. Symlinks remain supported and are validated after opening.
  let mut file = OpenOptions::new()
    .read(true)
    .custom_flags((OFlag::O_NONBLOCK | OFlag::O_CLOEXEC).bits())
    .open(path)?;
  if !file.metadata()?.file_type().is_file() {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "configuration path is not a regular file",
    ));
  }
  let mut contents = Vec::new();
  file.read_to_end(&mut contents)?;
  Ok(contents)
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
}

pub(crate) struct WatchBaseline {
  paths: Vec<PathBuf>,
  fingerprints: Vec<Fingerprint>,
}

struct WatchWorker {
  shutdown: Option<oneshot::Sender<()>>,
  handle: JoinHandle<()>,
  shutdown_timeout: Duration,
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
    }
  }

  fn spawn_with_timings(baseline: WatchBaseline, timings: WatchTimings) -> Self {
    let WatchBaseline { paths, fingerprints } = baseline;
    let (changes_tx, changes) = watch::channel(0_u64);
    let (shutdown, shutdown_rx) = oneshot::channel();
    let handle = tokio::spawn(watch_files(paths, fingerprints, changes_tx, shutdown_rx, timings));
    Self {
      changes: Some(changes),
      worker: Some(WatchWorker {
        shutdown: Some(shutdown),
        handle,
        shutdown_timeout: timings.shutdown_timeout,
      }),
    }
  }

  pub(crate) fn has_active(&self) -> bool {
    self.worker.is_some()
  }

  /// Wait for a coalesced change or unexpected worker termination.
  ///
  /// Both `watch::Receiver::changed` and `&mut JoinHandle` are cancel-safe, so
  /// an outer `tokio::select!` may freely drop this future.
  pub(crate) async fn next(&mut self) -> WatchOutcome {
    let Some(_) = self.changes.as_mut() else {
      return WatchOutcome::Stopped("configuration watcher is disabled".into());
    };
    let Some(_) = self.worker.as_mut() else {
      return WatchOutcome::Stopped("configuration watcher has already stopped".into());
    };

    enum Ready {
      Worker(Result<(), tokio::task::JoinError>),
      Changed(Result<(), tokio::sync::watch::error::RecvError>),
    }

    let ready = {
      let changes = self.changes.as_mut().unwrap();
      let worker = self.worker.as_mut().unwrap();
      tokio::select! {
        biased;
        result = &mut worker.handle => Ready::Worker(result),
        changed = changes.changed() => Ready::Changed(changed),
      }
    };

    match ready {
      Ready::Changed(Ok(())) => WatchOutcome::Changed,
      Ready::Worker(result) => {
        self.worker = None;
        self.changes = None;
        stopped_outcome(result)
      },
      Ready::Changed(Err(_)) => {
        self.changes = None;
        if let Some(mut worker) = self.worker.take() {
          if let Ok(result) = timeout(worker.shutdown_timeout, &mut worker.handle).await {
            return stopped_outcome(result);
          }
          worker.handle.abort();
          let _ = timeout(worker.shutdown_timeout, &mut worker.handle).await;
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
    if timeout(worker.shutdown_timeout, &mut worker.handle).await.is_err() {
      worker.handle.abort();
      let _ = timeout(worker.shutdown_timeout, &mut worker.handle).await;
    }
    self.changes = None;
  }
}

fn stopped_outcome(result: Result<(), tokio::task::JoinError>) -> WatchOutcome {
  WatchOutcome::Stopped(match result {
    Ok(()) => "configuration watcher stopped unexpectedly".into(),
    Err(error) => format!("configuration watcher failed: {error}"),
  })
}

impl Drop for ConfigWatcher {
  fn drop(&mut self) {
    if let Some(worker) = self.worker.as_mut() {
      if let Some(shutdown) = worker.shutdown.take() {
        let _ = shutdown.send(());
      }
      worker.handle.abort();
    }
  }
}

async fn watch_files(
  paths: Vec<PathBuf>,
  mut known: Vec<Fingerprint>,
  changes: watch::Sender<u64>,
  mut shutdown: oneshot::Receiver<()>,
  timings: WatchTimings,
) {
  let first_poll = Instant::now() + timings.poll_interval;
  let mut interval = interval_at(first_poll, timings.poll_interval);
  interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

  loop {
    tokio::select! {
      biased;
      _ = &mut shutdown => return,
      _ = interval.tick() => {},
    }

    let candidate = fingerprints(&paths);
    if candidate == known {
      continue;
    }

    let Some(settled) = settle(&paths, candidate, &mut shutdown, timings).await else {
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

async fn settle(
  paths: &[PathBuf],
  mut candidate: Vec<Fingerprint>,
  shutdown: &mut oneshot::Receiver<()>,
  timings: WatchTimings,
) -> Option<Vec<Fingerprint>> {
  let deadline = Instant::now() + timings.max_settle_time;
  loop {
    let sample_at = (Instant::now() + timings.settle_delay).min(deadline);
    tokio::select! {
      biased;
      _ = &mut *shutdown => return None,
      _ = sleep_until(sample_at) => {},
    }

    let sampled = fingerprints(paths);
    if sampled == candidate || Instant::now() >= deadline {
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
    let directory = tempdir().unwrap();
    let mut watcher = spawn(vec![directory.path().join("config.toml")]);
    watcher.worker.as_ref().unwrap().handle.abort();

    let outcome = timeout(Duration::from_secs(1), watcher.next())
      .await
      .expect("aborted watcher did not report its termination");
    assert!(matches!(outcome, WatchOutcome::Stopped(message) if message.contains("failed")));
    assert!(!watcher.has_active());
  }

  #[tokio::test]
  async fn a_closed_notification_channel_preserves_the_worker_panic() {
    let (changes_tx, changes) = watch::channel(0_u64);
    let handle = tokio::spawn(async move {
      drop(changes_tx);
      tokio::task::yield_now().await;
      panic!("injected watcher panic");
    });
    let mut watcher = ConfigWatcher {
      changes: Some(changes),
      worker: Some(WatchWorker {
        shutdown: None,
        handle,
        shutdown_timeout: test_timings().shutdown_timeout,
      }),
    };

    let outcome = timeout(Duration::from_secs(1), watcher.next())
      .await
      .expect("panicked watcher did not report its termination");
    assert!(matches!(outcome, WatchOutcome::Stopped(message) if message.contains("panicked")));
    assert!(!watcher.has_active());
  }

  #[tokio::test]
  async fn a_change_that_reverts_while_settling_is_ignored() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(&path, b"original").unwrap();
    let mut timings = test_timings();
    timings.settle_delay = Duration::from_millis(80);
    timings.max_settle_time = Duration::from_millis(160);
    let baseline = ConfigWatcher::capture(vec![path.clone()]);
    let mut watcher = ConfigWatcher::spawn_with_timings(baseline, timings);

    fs::write(&path, b"temporary").unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    fs::write(&path, b"original").unwrap();
    assert!(timeout(Duration::from_millis(220), watcher.next()).await.is_err());

    fs::write(&path, b"final").unwrap();
    changed(&mut watcher).await;
    watcher.shutdown().await;
  }

  #[tokio::test]
  async fn shutdown_interrupts_settling() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(&path, b"original").unwrap();
    let mut timings = test_timings();
    timings.settle_delay = Duration::from_secs(5);
    let baseline = ConfigWatcher::capture(vec![path.clone()]);
    let mut watcher = ConfigWatcher::spawn_with_timings(baseline, timings);
    fs::write(&path, b"changed").unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    timeout(Duration::from_millis(150), watcher.shutdown())
      .await
      .expect("watcher shutdown was blocked by settling");
  }

  #[test]
  fn a_non_regular_path_is_not_treated_as_missing() {
    let directory = tempdir().unwrap();
    assert!(matches!(Fingerprint::read(directory.path()), Fingerprint::Unreadable {
      kind: io::ErrorKind::InvalidData,
      ..
    }));
  }
}
