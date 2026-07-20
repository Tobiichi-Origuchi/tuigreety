use std::{
  io,
  panic::{AssertUnwindSafe, catch_unwind},
  sync::{
    Arc,
    Condvar,
    Mutex,
    atomic::{AtomicU64, Ordering},
  },
  thread,
};

use getopts::Matches;
use tokio::sync::mpsc as async_mpsc;

use crate::{
  ReloadPlan,
  ReloadSnapshot,
  config::{self, Diagnostic},
};

struct ReloadRequest {
  revision: u64,
  config: Arc<Matches>,
  snapshot: ReloadSnapshot,
}

enum WorkerOutcome {
  Ready {
    plan: Box<ReloadPlan>,
    warnings: Vec<Diagnostic>,
  },
  Rejected {
    warnings: Vec<Diagnostic>,
  },
  Failed,
}

struct WorkerResult {
  revision: u64,
  outcome: WorkerOutcome,
}

type Prepare = dyn Fn(ReloadRequest) -> WorkerOutcome + Send + Sync + 'static;

pub(crate) enum ReloadOutcome {
  Ready {
    plan: Box<ReloadPlan>,
    warnings: Vec<Diagnostic>,
  },
  Rejected {
    warnings: Vec<Diagnostic>,
  },
  Failed,
  WorkerStopped,
}

/// Serializes all potentially blocking configuration and discovery work on a
/// dedicated thread. New revisions may supersede old results, but NSS is never
/// enumerated concurrently and the async UI runtime is never blocked on it.
pub(crate) struct ReloadCoordinator {
  requests: Option<Arc<RequestSlot>>,
  results: async_mpsc::Receiver<WorkerResult>,
  latest: Arc<AtomicU64>,
  latest_revision: u64,
  pending: bool,
}

#[derive(Default)]
struct RequestState {
  latest: Option<ReloadRequest>,
  closed: bool,
}

#[derive(Default)]
struct RequestSlot {
  state: Mutex<RequestState>,
  changed: Condvar,
}

impl RequestSlot {
  fn replace(&self, request: ReloadRequest) -> Result<(), ()> {
    let mut state = self.state.lock().map_err(|_| ())?;
    if state.closed {
      return Err(());
    }
    state.latest = Some(request);
    self.changed.notify_one();
    Ok(())
  }

  fn take(&self) -> Option<ReloadRequest> {
    let mut state = self.state.lock().ok()?;
    loop {
      if let Some(request) = state.latest.take() {
        return Some(request);
      }
      if state.closed {
        return None;
      }
      state = self.changed.wait(state).ok()?;
    }
  }

  fn close(&self) {
    if let Ok(mut state) = self.state.lock() {
      state.closed = true;
      state.latest = None;
      self.changed.notify_one();
    }
  }
}

impl ReloadCoordinator {
  pub(crate) fn new() -> io::Result<Self> {
    Self::with_prepare(Arc::new(prepare))
  }

  fn with_prepare(prepare: Arc<Prepare>) -> io::Result<Self> {
    let requests = Arc::new(RequestSlot::default());
    let latest = Arc::new(AtomicU64::new(0));
    let (result_tx, result_rx) = async_mpsc::channel(1);

    thread::Builder::new().name("tuigreet-config-reload".into()).spawn({
      let requests = requests.clone();
      let latest = latest.clone();
      move || worker(requests, result_tx, latest, prepare)
    })?;

    Ok(Self {
      requests: Some(requests),
      results: result_rx,
      latest,
      latest_revision: 0,
      pending: false,
    })
  }

  pub(crate) fn request(&mut self, config: Arc<Matches>, snapshot: ReloadSnapshot) -> Result<(), String> {
    self.latest_revision = self.latest_revision.wrapping_add(1).max(1);
    let revision = self.latest_revision;
    let Some(requests) = &self.requests else {
      return Err("configuration reload worker is unavailable".into());
    };

    self.latest.store(revision, Ordering::Release);
    requests
      .replace(ReloadRequest {
        revision,
        config,
        snapshot,
      })
      .map_err(|_| "configuration reload worker stopped unexpectedly".to_string())?;
    self.pending = true;
    Ok(())
  }

  pub(crate) fn has_pending(&self) -> bool {
    self.pending
  }

  pub(crate) async fn next(&mut self) -> ReloadOutcome {
    loop {
      let Some(result) = self.results.recv().await else {
        self.pending = false;
        self.requests = None;
        return ReloadOutcome::WorkerStopped;
      };

      if result.revision != self.latest_revision {
        continue;
      }

      self.pending = false;
      return match result.outcome {
        WorkerOutcome::Ready { plan, warnings } => ReloadOutcome::Ready { plan, warnings },
        WorkerOutcome::Rejected { warnings } => ReloadOutcome::Rejected { warnings },
        WorkerOutcome::Failed => ReloadOutcome::Failed,
      };
    }
  }
}

impl Drop for ReloadCoordinator {
  fn drop(&mut self) {
    if let Some(requests) = self.requests.take() {
      requests.close();
    }
  }
}

fn worker(
  requests: Arc<RequestSlot>,
  results: async_mpsc::Sender<WorkerResult>,
  latest: Arc<AtomicU64>,
  prepare: Arc<Prepare>,
) {
  while let Some(request) = requests.take() {
    let revision = request.revision;
    let outcome = catch_unwind(AssertUnwindSafe(|| prepare(request))).unwrap_or(WorkerOutcome::Failed);
    if revision != latest.load(Ordering::Acquire) {
      continue;
    }
    if results.blocking_send(WorkerResult { revision, outcome }).is_err() {
      break;
    }
  }
}

fn prepare(request: ReloadRequest) -> WorkerOutcome {
  match config::reload(&request.config) {
    Ok((settings, warnings)) => WorkerOutcome::Ready {
      plan: Box::new(ReloadPlan::prepare(request.snapshot, settings)),
      warnings,
    },
    Err(warnings) => WorkerOutcome::Rejected { warnings },
  }
}

#[cfg(test)]
mod tests {
  use std::{
    fs,
    sync::{
      Arc,
      Condvar,
      Mutex,
      atomic::{AtomicUsize, Ordering},
      mpsc,
    },
  };

  use tempfile::tempdir;

  use super::{ReloadCoordinator, ReloadOutcome, WorkerOutcome};
  use crate::{Greeter, greeter::ReloadPlan};

  fn config_for(path: &std::path::Path) -> Arc<getopts::Matches> {
    Arc::new(Greeter::options().parse(["--config", path.to_str().unwrap()]).unwrap())
  }

  #[tokio::test]
  async fn only_the_latest_queued_revision_is_applied() {
    let directory = tempdir().unwrap();
    let first = directory.path().join("first.toml");
    let second = directory.path().join("second.toml");
    let latest = directory.path().join("latest.toml");
    fs::write(&first, "[display]\nwidth = 61\n").unwrap();
    fs::write(&second, "[display]\nwidth = 62\n").unwrap();
    fs::write(&latest, "[display]\nwidth = 63\n").unwrap();

    let mut coordinator = ReloadCoordinator::new().unwrap();
    let greeter = Greeter::default();
    coordinator
      .request(config_for(&first), greeter.reload_snapshot())
      .unwrap();
    coordinator
      .request(config_for(&second), greeter.reload_snapshot())
      .unwrap();
    coordinator
      .request(config_for(&latest), greeter.reload_snapshot())
      .unwrap();

    let ReloadOutcome::Ready { plan, .. } = coordinator.next().await else {
      panic!("latest reload was not prepared successfully");
    };
    let mut applied = Greeter::default();
    applied.apply_reload(*plan);
    assert_eq!(applied.settings.width, 63);
    assert!(!coordinator.has_pending());
  }

  #[test]
  fn reload_plan_remains_sendable_between_worker_and_runtime() {
    fn assert_send<T: Send>() {}
    assert_send::<ReloadPlan>();
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn blocked_discovery_is_serial_and_coalesces_to_the_latest_request() {
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let revisions = Arc::new(Mutex::new(Vec::new()));
    let (started_tx, started_rx) = mpsc::channel();
    let mut coordinator = ReloadCoordinator::with_prepare(Arc::new({
      let gate = gate.clone();
      let active = active.clone();
      let maximum = maximum.clone();
      let revisions = revisions.clone();
      move |request| {
        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
        maximum.fetch_max(now, Ordering::SeqCst);
        revisions.lock().unwrap().push(request.revision);
        if request.revision == 1 {
          started_tx.send(()).unwrap();
          let (lock, ready) = &*gate;
          let mut released = lock.lock().unwrap();
          while !*released {
            released = ready.wait(released).unwrap();
          }
        }
        active.fetch_sub(1, Ordering::SeqCst);
        WorkerOutcome::Failed
      }
    }))
    .unwrap();
    let config = Arc::new(Greeter::options().parse(std::iter::empty::<&str>()).unwrap());
    let greeter = Greeter::default();

    coordinator.request(config.clone(), greeter.reload_snapshot()).unwrap();
    started_rx.recv().unwrap();
    coordinator.request(config.clone(), greeter.reload_snapshot()).unwrap();
    coordinator.request(config, greeter.reload_snapshot()).unwrap();
    {
      let (lock, ready) = &*gate;
      *lock.lock().unwrap() = true;
      ready.notify_all();
    }

    assert!(matches!(coordinator.next().await, ReloadOutcome::Failed));
    assert_eq!(maximum.load(Ordering::SeqCst), 1);
    assert_eq!(*revisions.lock().unwrap(), [1, 3]);
  }
}
