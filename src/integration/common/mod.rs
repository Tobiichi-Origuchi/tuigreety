mod backend;
mod output;

use std::{
  fmt::Debug,
  fs,
  io,
  os::unix::fs::FileTypeExt,
  path::Path,
  sync::{Arc, Mutex},
  time::Duration,
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use libgreetd_stub::SessionOptions;
use ratatui::buffer::Buffer;
use tempfile::{NamedTempFile, TempPath};
use tokio::{
  net::UnixStream,
  sync::{RwLock, mpsc::Sender, watch::Receiver},
  task::{JoinError, JoinHandle},
  time::Instant,
};

pub(super) use self::{
  backend::{TestBackend, output},
  output::*,
};
use crate::{
  AuthStatus,
  CliInvocation,
  Greeter,
  event::{Event, Events},
  ui::sessions::SessionSource,
  watcher::ConfigWatcher,
};

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

type ClientResult = Result<AuthStatus, String>;

pub(super) struct IntegrationRunner(Arc<RwLock<_IntegrationRunner>>);

struct _IntegrationRunner {
  server: Option<JoinHandle<()>>,
  client: Option<JoinHandle<ClientResult>>,

  pub buffer: Arc<Mutex<Buffer>>,
  pub sender: Sender<Event>,
  pub tick: Receiver<u64>,

  // Keep ownership of the temporary path until every task has stopped. The
  // TempPath removes the Unix socket when the last runner clone is dropped.
  _socket: TempPath,
}

impl Clone for IntegrationRunner {
  fn clone(&self) -> Self {
    IntegrationRunner(Arc::clone(&self.0))
  }
}

impl IntegrationRunner {
  pub async fn new(opts: SessionOptions, builder: Option<fn(&mut Greeter)>) -> IntegrationRunner {
    IntegrationRunner::new_with_size(opts, builder, (200, 40)).await
  }

  pub async fn new_with_size(
    opts: SessionOptions,
    builder: Option<fn(&mut Greeter)>,
    size: (u16, u16),
  ) -> IntegrationRunner {
    let socket = NamedTempFile::new()
      .expect("could not reserve a path for the greetd test socket")
      .into_temp_path();
    let socket_path = socket.to_path_buf();

    let (backend, buffer, tick) = TestBackend::new(size.0, size.1);
    let events = Events::testing().await;
    let sender = events.sender();

    let mut server = tokio::task::spawn({
      let socket = socket_path.clone();

      async move {
        libgreetd_stub::start(&socket, &opts).await;
      }
    });

    wait_for_server(&socket_path, &mut server).await;

    let client = tokio::task::spawn(async move {
      let invocation = CliInvocation::parse(["tuigreet"]);
      let mut greeter = Greeter::new_isolated(invocation.matches()).await;
      greeter.session_source = SessionSource::DefaultCommand("uname".to_string(), None);

      if let Some(builder) = builder {
        builder(&mut greeter);
      }

      greeter.logfile = "/tmp/tuigreet.log".to_string();
      greeter.socket = socket_path.to_str().unwrap().to_string();
      match crate::run(backend, greeter, events, ConfigWatcher::disabled()).await {
        Ok(()) => Err("tuigreet returned without an authentication status".to_string()),
        Err(error) => match error.downcast_ref::<AuthStatus>() {
          Some(status) => Ok(*status),
          None => Err(format!("tuigreet returned an error: {error}")),
        },
      }
    });

    IntegrationRunner(Arc::new(RwLock::new(_IntegrationRunner {
      server: Some(server),
      client: Some(client),
      buffer,
      sender,
      tick,
      _socket: socket,
    })))
  }

  /// Wait until both the event-driving task and tuigreet exit successfully.
  /// The greetd stub must stay alive for the whole scenario.
  pub async fn join_until_client_exit(&mut self, mut events: JoinHandle<()>, expected_status: AuthStatus) {
    let (mut server, mut client) = self.take_tasks().await;
    let deadline = Instant::now() + TEST_TIMEOUT;
    let mut client_done = false;
    let mut events_done = false;

    while !client_done || !events_done {
      tokio::select! {
        result = &mut server => {
          let mut failures = vec![Some(unexpected_server(result))];
          if !client_done {
            failures.push(abort_and_await("tuigreet client", client).await);
          }
          if !events_done {
            failures.push(abort_and_await("event driver", events).await);
          }
          fail("integration server exited before the scenario completed", failures);
        },
        result = &mut client, if !client_done => {
          client_done = true;
          if let Some(failure) = expected_client_failure(result, expected_status) {
            let mut failures = vec![Some(failure), abort_and_await("greetd stub", server).await];
            if !events_done {
              failures.push(abort_and_await("event driver", events).await);
            }
            fail("tuigreet did not exit as expected", failures);
          }
        },
        result = &mut events, if !events_done => {
          events_done = true;
          if let Some(failure) = task_failure("event driver", result) {
            let mut failures = vec![Some(failure), abort_and_await("greetd stub", server).await];
            if !client_done {
              failures.push(abort_and_await("tuigreet client", client).await);
            }
            fail("event driver failed", failures);
          }
        },
        _ = tokio::time::sleep_until(deadline) => {
          let mut failures = vec![abort_and_await("greetd stub", server).await];
          if !client_done {
            failures.push(abort_and_await("tuigreet client", client).await);
          }
          if !events_done {
            failures.push(abort_and_await("event driver", events).await);
          }
          fail_with_cleanup("integration scenario timed out", failures);
        },
      }
    }

    fail_if_any(
      "greetd stub did not stay alive for the complete integration scenario",
      vec![abort_and_await("greetd stub", server).await],
    );
  }

  /// Wait for the event-driving task to finish its assertions. The application
  /// and greetd stub are expected to still be running and are cancelled only
  /// after the scenario completes.
  pub async fn join_until_end(&mut self, mut events: JoinHandle<()>) {
    let (mut server, mut client) = self.take_tasks().await;

    enum Outcome {
      Server(Result<(), JoinError>),
      Client(Result<ClientResult, JoinError>),
      Events(Result<(), JoinError>),
      Timeout,
    }

    let outcome = tokio::select! {
      result = &mut server => Outcome::Server(result),
      result = &mut client => Outcome::Client(result),
      result = &mut events => Outcome::Events(result),
      _ = tokio::time::sleep(TEST_TIMEOUT) => Outcome::Timeout,
    };

    match outcome {
      Outcome::Events(result) => {
        let failures = vec![
          task_failure("event driver", result),
          abort_and_await("greetd stub", server).await,
          abort_and_await("tuigreet client", client).await,
        ];
        fail_if_any("integration scenario failed", failures);
      },
      Outcome::Server(result) => {
        let failures = vec![
          Some(unexpected_server(result)),
          abort_and_await("tuigreet client", client).await,
          abort_and_await("event driver", events).await,
        ];
        fail_if_any("integration server exited before the scenario completed", failures);
      },
      Outcome::Client(result) => {
        let failures = vec![
          Some(unexpected_client(result)),
          abort_and_await("greetd stub", server).await,
          abort_and_await("event driver", events).await,
        ];
        fail_if_any("tuigreet exited before the scenario completed", failures);
      },
      Outcome::Timeout => {
        let failures = vec![
          abort_and_await("greetd stub", server).await,
          abort_and_await("tuigreet client", client).await,
          abort_and_await("event driver", events).await,
        ];
        fail_with_cleanup("integration scenario timed out", failures);
      },
    }
  }

  async fn take_tasks(&self) -> (JoinHandle<()>, JoinHandle<ClientResult>) {
    let mut runner = self.0.write().await;
    let server = runner.server.take().expect("integration server was already joined");
    let client = runner.client.take().expect("integration client was already joined");
    (server, client)
  }

  #[allow(unused)]
  pub async fn wait_until_buffer_contains(&mut self, needle: &str) {
    loop {
      if output(&self.0.read().await.buffer).contains(needle) {
        return;
      }

      self.wait_for_render().await;
    }
  }

  #[allow(unused)]
  pub async fn send_key(&self, key: KeyCode) {
    self
      .send_event(Event::Key(KeyEvent::new(key, KeyModifiers::empty())))
      .await;
    self.send_event(Event::Render).await;
  }

  #[allow(unused)]
  pub async fn send_modified_key(&self, key: KeyCode, modifiers: KeyModifiers) {
    self.send_event(Event::Key(KeyEvent::new(key, modifiers))).await;
    self.send_event(Event::Render).await;
  }

  #[allow(unused)]
  pub async fn send_text(&self, text: &str) {
    for char in text.chars() {
      self.send_key(KeyCode::Char(char)).await;
    }

    self.send_key(KeyCode::Enter).await;
  }

  async fn send_event(&self, event: Event) {
    let sender = self.0.read().await.sender.clone();
    sender
      .send(event)
      .await
      .expect("tuigreet event receiver closed before the test input was delivered");
  }

  #[allow(unused)]
  pub async fn wait_for_render(&mut self) {
    let mut runner = self.0.write().await;

    // Ignore a notification that predates this wait. Assertions must observe
    // a frame produced after they explicitly started waiting, rather than a
    // stale item left in a bounded queue by an earlier draw.
    runner.tick.borrow_and_update();
    runner
      .tick
      .changed()
      .await
      .expect("test backend stopped rendering before the expected frame");
  }

  pub async fn output(&self) -> Output {
    Output(output(&self.0.read().await.buffer))
  }
}

async fn wait_for_server(socket: &Path, server: &mut JoinHandle<()>) {
  let readiness = async {
    loop {
      match fs::metadata(socket) {
        Ok(metadata) if metadata.file_type().is_socket() => match UnixStream::connect(socket).await {
          Ok(probe) => {
            drop(probe);
            return Ok(());
          },
          Err(error) if matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused) => {},
          Err(error) => return Err(error),
        },
        Ok(_) => {},
        Err(error) if error.kind() == io::ErrorKind::NotFound => {},
        Err(error) => return Err(error),
      }

      tokio::time::sleep(Duration::from_millis(1)).await;
    }
  };

  tokio::select! {
    result = &mut *server => {
      panic!("greetd stub exited before its socket was ready: {}", describe_join(result));
    },
    result = tokio::time::timeout(TEST_TIMEOUT, readiness) => match result {
      Ok(Ok(())) => {},
      Ok(Err(error)) => {
        server.abort();
        let cleanup = server.await;
        panic!("could not connect to the greetd stub socket: {error}; cleanup: {}", describe_join(cleanup));
      },
      Err(_) => {
        server.abort();
        let cleanup = server.await;
        panic!("greetd stub socket was not ready within {TEST_TIMEOUT:?}; cleanup: {}", describe_join(cleanup));
      },
    },
  }
}

fn expected_client_failure(result: Result<ClientResult, JoinError>, expected_status: AuthStatus) -> Option<String> {
  match result {
    Ok(Ok(actual)) if std::mem::discriminant(&actual) == std::mem::discriminant(&expected_status) => None,
    Ok(Ok(actual)) => Some(format!("tuigreet exited with {actual}, expected {expected_status}")),
    Ok(Err(error)) => Some(error),
    Err(error) => Some(format!("tuigreet client task failed: {error}")),
  }
}

fn unexpected_client(result: Result<ClientResult, JoinError>) -> String {
  match result {
    Ok(Ok(status)) => format!("tuigreet exited early with {status}"),
    Ok(Err(error)) => error,
    Err(error) => format!("tuigreet client task failed: {error}"),
  }
}

fn unexpected_server(result: Result<(), JoinError>) -> String {
  match result {
    Ok(()) => "greetd stub returned unexpectedly".to_string(),
    Err(error) => format!("greetd stub task failed: {error}"),
  }
}

fn task_failure(name: &str, result: Result<(), JoinError>) -> Option<String> {
  result.err().map(|error| format!("{name} task failed: {error}"))
}

async fn abort_and_await<T>(name: &str, handle: JoinHandle<T>) -> Option<String>
where
  T: Debug,
{
  handle.abort();
  match handle.await {
    Err(error) if error.is_cancelled() => None,
    Err(error) => Some(format!("{name} failed while being stopped: {error}")),
    Ok(output) => Some(format!("{name} completed unexpectedly before cancellation: {output:?}")),
  }
}

fn describe_join<T>(result: Result<T, JoinError>) -> String {
  match result {
    Ok(_) => "task returned unexpectedly".to_string(),
    Err(error) => error.to_string(),
  }
}

fn fail_if_any(context: &str, failures: Vec<Option<String>>) {
  let failures = failures.into_iter().flatten().collect::<Vec<_>>();
  if !failures.is_empty() {
    panic!("{context}: {}", failures.join("; "));
  }
}

fn fail_with_cleanup(context: &str, failures: Vec<Option<String>>) -> ! {
  let failures = failures.into_iter().flatten().collect::<Vec<_>>();
  if failures.is_empty() {
    panic!("{context}");
  }

  panic!("{context}; cleanup failures: {}", failures.join("; "));
}

fn fail(context: &str, failures: Vec<Option<String>>) -> ! {
  let failures = failures.into_iter().flatten().collect::<Vec<_>>();
  if failures.is_empty() {
    panic!("{context}");
  }

  panic!("{context}: {}", failures.join("; "));
}

#[cfg(test)]
mod tests {
  use std::future::pending;

  use ratatui::{backend::Backend, buffer::Cell};

  use super::*;

  #[tokio::test]
  async fn aborting_a_pending_task_is_clean() {
    let task = tokio::spawn(pending::<()>());
    assert_eq!(abort_and_await("pending task", task).await, None);
  }

  #[tokio::test]
  async fn a_task_that_already_returned_is_not_treated_as_cancelled() {
    let task = tokio::spawn(async {});
    tokio::task::yield_now().await;

    assert!(
      abort_and_await("completed task", task)
        .await
        .unwrap()
        .contains("completed unexpectedly")
    );
  }

  #[tokio::test]
  async fn backend_draws_are_coalesced_and_report_receiver_loss() {
    let (mut backend, _, mut renders) = TestBackend::new(2, 2);
    let cell = Cell::new("x");

    backend.draw([(0, 0, &cell)].into_iter()).unwrap();
    backend.draw([(1, 0, &cell)].into_iter()).unwrap();
    renders.changed().await.unwrap();
    assert_eq!(*renders.borrow_and_update(), 2);

    drop(renders);
    let error = backend.draw([(0, 1, &cell)].into_iter()).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
  }

  #[test]
  fn clearing_the_whole_backend_does_not_relock_its_buffer() {
    let (mut backend, buffer, _) = TestBackend::new(2, 2);
    buffer.lock().unwrap()[(0, 0)].set_symbol("x");

    backend.clear_region(ratatui::backend::ClearType::All).unwrap();

    assert_eq!(buffer.lock().unwrap()[(0, 0)], Cell::default());
  }
}
