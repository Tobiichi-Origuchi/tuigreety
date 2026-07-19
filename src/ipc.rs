use std::{
  borrow::Cow,
  env,
  error::Error,
  io,
  sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
  },
  time::Duration,
};

use greetd_ipc::{AuthMessageType, ErrorType, Request, Response, codec::TokioCodec};
use tokio::{
  net::UnixStream,
  sync::{
    Mutex,
    Notify,
    RwLock,
    mpsc::{Receiver, Sender, UnboundedSender},
  },
  time::{Instant, sleep, sleep_until, timeout},
};

use crate::{
  AuthStatus,
  Greeter,
  Mode,
  cache::{CacheStore, CacheUpdate, RememberedSelection, RememberedUser},
  event::{Control, Event},
  macros::SafeDebug,
  ui::sessions::{Session, SessionSource, SessionType},
};

const MAX_RECOVERY_ATTEMPTS: u8 = 3;
const CANCEL_WRITE_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CachedSession {
  Command(String),
  Desktop(RememberedSelection),
  None,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PendingSession {
  username: String,
  display_name: Option<String>,
  selection: CachedSession,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CachePolicy {
  remember_username: bool,
  remember_session: bool,
  remember_user_session: bool,
  allow_command_editor: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AuthInput {
  Secret,
  Visible,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) enum AuthState {
  #[default]
  Idle,
  CreatingSession,
  AwaitingInput(AuthInput),
  ContinuingAuth,
  StartingSession(PendingSession, CachePolicy),
  Cancelling,
  Started,
  Failed,
}

impl AuthState {
  pub(crate) fn accepts_input(&self) -> bool {
    matches!(self, Self::Idle | Self::AwaitingInput(_))
  }

  pub(crate) fn is_waiting(&self) -> bool {
    matches!(
      self,
      Self::CreatingSession | Self::ContinuingAuth | Self::StartingSession(..) | Self::Cancelling
    )
  }

  pub(crate) fn can_cancel(&self) -> bool {
    matches!(
      self,
      Self::CreatingSession | Self::AwaitingInput(_) | Self::ContinuingAuth
    )
  }

  fn is_live_prestart(&self) -> bool {
    self.can_cancel() || matches!(self, Self::Cancelling)
  }
}

impl CachePolicy {
  fn current(greeter: &Greeter) -> Self {
    Self {
      remember_username: greeter.remember,
      remember_session: greeter.remember_session,
      remember_user_session: greeter.remember_user_session,
      allow_command_editor: greeter.allow_command_editor,
    }
  }
}

impl PendingSession {
  fn capture(greeter: &Greeter) -> Self {
    let selection = match &greeter.session_source {
      SessionSource::Command(command) if greeter.allow_command_editor => CachedSession::Command(command.clone()),
      SessionSource::Session(index) => greeter
        .sessions
        .options
        .get(*index)
        .and_then(RememberedSelection::from_session)
        .map_or(CachedSession::None, CachedSession::Desktop),
      _ => CachedSession::None,
    };

    Self {
      username: greeter.username.value.clone(),
      display_name: greeter.username.mask.clone(),
      selection,
    }
  }
}

fn build_cache_update(pending: PendingSession, policy: CachePolicy) -> CacheUpdate {
  let selection = match pending.selection {
    CachedSession::Command(command) => Some(RememberedSelection::command(command)),
    CachedSession::Desktop(selection) => Some(selection),
    CachedSession::None => None,
  };

  CacheUpdate::successful_login(
    RememberedUser {
      username: pending.username,
      display_name: pending.display_name,
    },
    selection,
    policy.remember_username,
    policy.remember_session,
    policy.remember_user_session,
    policy.allow_command_editor,
  )
}

struct ResponseOutcome {
  control: Option<Control>,
  cache_update: Option<CacheUpdate>,
}

#[derive(Clone)]
pub struct Ipc(Arc<IpcHandle>);

struct QueuedRequest {
  request: Request,
  generation: u64,
}

pub struct IpcHandle {
  tx: Sender<QueuedRequest>,
  rx: Mutex<Receiver<QueuedRequest>>,
  cancel_generation: AtomicU64,
  cancel_notify: Notify,
  shutdown: AtomicBool,
  shutdown_notify: Notify,
}

#[derive(Debug)]
enum TransactionFailure {
  Cancelled,
  Shutdown,
  TimedOut,
  Transport(String),
}

impl Ipc {
  pub fn new() -> Ipc {
    let (tx, rx) = tokio::sync::mpsc::channel::<QueuedRequest>(10);

    Ipc(Arc::new(IpcHandle {
      tx,
      rx: Mutex::new(rx),
      cancel_generation: AtomicU64::new(0),
      cancel_notify: Notify::new(),
      shutdown: AtomicBool::new(false),
      shutdown_notify: Notify::new(),
    }))
  }

  pub async fn send(&self, request: Request) {
    tracing::info!("sending request to greetd: {}", request.safe_repr());

    let generation = self.0.cancel_generation.load(Ordering::Acquire);
    let _ = self.0.tx.send(QueuedRequest { request, generation }).await;
  }

  async fn next_queued(&self) -> Option<QueuedRequest> {
    self.0.rx.lock().await.recv().await
  }

  #[cfg(test)]
  pub async fn next(&self) -> Option<Request> {
    self.next_queued().await.map(|queued| queued.request)
  }

  #[cfg(test)]
  pub async fn handle(&self, greeter: Arc<RwLock<Greeter>>) -> Result<Option<Control>, Box<dyn Error + Send + Sync>> {
    let Some(queued) = self.next_queued().await else {
      return Ok(None);
    };
    let mut stream = None;
    let ipc_timeout = Duration::from_secs(u64::from(greeter.read().await.ipc_timeout));
    let response = self
      .transact(&greeter, &mut stream, &queued.request, queued.generation, ipc_timeout)
      .await
      .map_err(transaction_error)?;
    let (outcome, cache_store) = {
      let mut state = greeter.write().await;
      let outcome = self.parse_response(&mut state, response).await?;
      (outcome, state.cache_store.clone())
    };
    persist_cache(cache_store, outcome.cache_update).await;
    Ok(outcome.control)
  }

  pub async fn run(&self, greeter: Arc<RwLock<Greeter>>, controls: UnboundedSender<Control>, renders: Sender<Event>) {
    let mut stream = None;
    let mut failures = 0_u8;
    // A cancellation can be requested immediately after spawning this actor,
    // before its future is first polled. Start at the initial generation so
    // that such a request cannot be mistaken for already handled work.
    let mut observed_generation = 0;

    loop {
      let queued = tokio::select! {
        biased;
        _ = self.wait_for_shutdown() => {
          self.cancel_stream_if_live(&greeter, &mut stream).await;
          break;
        },
        _ = self.wait_for_cancel(observed_generation) => {
          observed_generation = self.0.cancel_generation.load(Ordering::Acquire);
          self.cancel_stream_if_live(&greeter, &mut stream).await;
          finish_cancellation(&greeter).await;
          request_render(&renders);
          continue;
        },
        request = self.next_queued() => match request {
          Some(request) => request,
          None => break,
        },
      };

      if queued.generation != self.0.cancel_generation.load(Ordering::Acquire) {
        observed_generation = self.0.cancel_generation.load(Ordering::Acquire);
        self.cancel_stream_if_live(&greeter, &mut stream).await;
        finish_cancellation(&greeter).await;
        request_render(&renders);
        continue;
      }

      let ipc_timeout = Duration::from_secs(u64::from(greeter.read().await.ipc_timeout));
      let transaction = self
        .transact(&greeter, &mut stream, &queued.request, queued.generation, ipc_timeout)
        .await;

      match transaction {
        Ok(response) => {
          failures = 0;
          // greetd errors automatically cancel the authentication session.
          // Reopen the transport before a fresh CreateSession so no daemon or
          // test implementation can retain stale per-connection PAM state.
          let session_cancelled = matches!(response, Response::Error { .. });
          let parsed = {
            let mut state = greeter.write().await;
            self
              .parse_response(&mut state, response)
              .await
              .map(|outcome| (outcome, state.cache_store.clone()))
          };
          match parsed {
            Ok((outcome, cache_store)) => {
              if session_cancelled {
                stream = None;
              }
              persist_cache(cache_store, outcome.cache_update).await;
              request_render(&renders);
              if let Some(control) = outcome.control
                && controls.send(control).is_err()
              {
                break;
              }
            },
            Err(error) => {
              tracing::error!("greetd IPC protocol error: {error}");
              stream = None;
              failures = failures.saturating_add(1);
              if !self
                .recover(
                  &greeter,
                  &renders,
                  &controls,
                  &queued.request,
                  queued.generation,
                  failures,
                )
                .await
              {
                break;
              }
            },
          }
        },
        Err(TransactionFailure::Cancelled) => {
          observed_generation = self.0.cancel_generation.load(Ordering::Acquire);
          finish_cancellation(&greeter).await;
          request_render(&renders);
        },
        Err(TransactionFailure::Shutdown) => {
          self.cancel_stream_if_live(&greeter, &mut stream).await;
          break;
        },
        Err(error) => {
          tracing::error!("greetd IPC request failed: {}", transaction_error_message(&error));
          stream = None;
          failures = failures.saturating_add(1);
          if !self
            .recover(
              &greeter,
              &renders,
              &controls,
              &queued.request,
              queued.generation,
              failures,
            )
            .await
          {
            break;
          }
        },
      }
    }
  }

  async fn transact(
    &self,
    greeter: &Arc<RwLock<Greeter>>,
    stream: &mut Option<UnixStream>,
    request: &Request,
    generation: u64,
    ipc_timeout: Duration,
  ) -> Result<Response, TransactionFailure> {
    let mock = greeter.read().await.mock;
    if mock {
      if self.cancelled(generation) {
        return Err(TransactionFailure::Cancelled);
      }
      return Ok(mock_response(request));
    }

    if stream.is_none() {
      let socket = {
        let configured = greeter.read().await.socket.clone();
        if configured.is_empty() {
          env::var("GREETD_SOCK").map_err(|_| TransactionFailure::Transport("GREETD_SOCK must be defined".into()))?
        } else {
          configured
        }
      };
      let connect = UnixStream::connect(socket);
      let connected = tokio::select! {
        biased;
        _ = self.wait_for_shutdown() => return Err(TransactionFailure::Shutdown),
        _ = self.wait_for_cancel(generation) => return Err(TransactionFailure::Cancelled),
        result = timeout(ipc_timeout, connect) => match result {
          Ok(Ok(stream)) => stream,
          Ok(Err(error)) => return Err(TransactionFailure::Transport(error.to_string())),
          Err(_) => return Err(TransactionFailure::TimedOut),
        },
      };
      *stream = Some(connected);
    }

    let deadline = Instant::now() + ipc_timeout;
    let write_result = {
      let socket = stream.as_mut().expect("connected stream");
      tokio::select! {
        biased;
        _ = self.wait_for_shutdown() => Err(TransactionFailure::Shutdown),
        _ = self.wait_for_cancel(generation) => Err(TransactionFailure::Cancelled),
        _ = sleep_until(deadline) => Err(TransactionFailure::TimedOut),
        result = request.write_to(socket) => result.map_err(|error| TransactionFailure::Transport(error.to_string())),
      }
    };
    if let Err(error) = write_result {
      *stream = None;
      return Err(error);
    }

    let response = {
      let socket = stream.as_mut().expect("connected stream");
      tokio::select! {
        biased;
        _ = self.wait_for_shutdown() => Err(TransactionFailure::Shutdown),
        _ = self.wait_for_cancel(generation) => Err(TransactionFailure::Cancelled),
        _ = sleep_until(deadline) => Err(TransactionFailure::TimedOut),
        result = Response::read_from(socket) => result.map_err(|error| TransactionFailure::Transport(error.to_string())),
      }
    };

    if let Err(TransactionFailure::Cancelled | TransactionFailure::Shutdown) = &response
      && !matches!(request, Request::StartSession { .. })
      && let Some(socket) = stream.as_mut()
    {
      let _ = timeout(CANCEL_WRITE_TIMEOUT, Request::CancelSession.write_to(socket)).await;
    }
    if response.is_err() {
      *stream = None;
    }
    response
  }

  async fn recover(
    &self,
    greeter: &Arc<RwLock<Greeter>>,
    renders: &Sender<Event>,
    controls: &UnboundedSender<Control>,
    request: &Request,
    generation: u64,
    failures: u8,
  ) -> bool {
    let is_start = matches!(request, Request::StartSession { .. });
    {
      let mut greeter = greeter.write().await;
      greeter.auth_state = AuthState::Failed;
      greeter.message = Some(text!(greeter, greetd_error));
    }
    request_render(renders);

    if is_start || failures >= MAX_RECOVERY_ATTEMPTS {
      let _ = controls.send(Control::Exit(AuthStatus::Failure));
      return false;
    }

    {
      let mut greeter = greeter.write().await;
      greeter.reset_local(true);
      greeter.auth_state = AuthState::CreatingSession;
    }
    request_render(renders);

    let delay = Duration::from_millis(250 * u64::from(failures));
    tokio::select! {
      biased;
      _ = self.wait_for_shutdown() => false,
      _ = self.wait_for_cancel(generation) => {
        finish_cancellation(greeter).await;
        request_render(renders);
        true
      },
      _ = sleep(delay) => {
        let username = greeter.read().await.username.value.clone();
        self.send(Request::CreateSession { username }).await;
        true
      },
    }
  }

  async fn cancel_stream_if_live(&self, greeter: &Arc<RwLock<Greeter>>, stream: &mut Option<UnixStream>) {
    let can_cancel = greeter.read().await.auth_state.is_live_prestart();
    if can_cancel && let Some(socket) = stream.as_mut() {
      let _ = timeout(CANCEL_WRITE_TIMEOUT, Request::CancelSession.write_to(socket)).await;
    }
    *stream = None;
  }

  fn cancelled(&self, generation: u64) -> bool {
    self.0.cancel_generation.load(Ordering::Acquire) != generation
  }

  async fn wait_for_cancel(&self, generation: u64) {
    loop {
      let notified = self.0.cancel_notify.notified();
      if self.cancelled(generation) {
        return;
      }
      notified.await;
    }
  }

  async fn wait_for_shutdown(&self) {
    loop {
      let notified = self.0.shutdown_notify.notified();
      if self.0.shutdown.load(Ordering::Acquire) {
        return;
      }
      notified.await;
    }
  }

  pub fn shutdown(&self) {
    self.0.shutdown.store(true, Ordering::Release);
    self.0.shutdown_notify.notify_waiters();
  }

  async fn parse_response(
    &self,
    greeter: &mut Greeter,
    response: Response,
  ) -> Result<ResponseOutcome, Box<dyn Error + Send + Sync>> {
    let mut pending_cache = None;
    let control = self
      .parse_response_with(greeter, response, |pending, policy| {
        pending_cache = Some(build_cache_update(pending, policy));
      })
      .await?;

    Ok(ResponseOutcome {
      control,
      cache_update: pending_cache,
    })
  }

  async fn parse_response_with<F>(
    &self,
    greeter: &mut Greeter,
    response: Response,
    commit: F,
  ) -> Result<Option<Control>, Box<dyn Error + Send + Sync>>
  where
    F: FnOnce(PendingSession, CachePolicy),
  {
    let mut control = None;

    // Do not display actual message from greetd, which may contain entered information, sometimes passwords.
    match response {
      Response::Error { ref error_type, .. } => tracing::info!("received greetd error message: {error_type:?}"),
      ref response => tracing::info!("received greetd message: {:?}", response),
    }

    match response {
      Response::AuthMessage {
        auth_message_type,
        auth_message,
      } => {
        if !matches!(
          greeter.auth_state,
          AuthState::CreatingSession | AuthState::ContinuingAuth
        ) {
          return Err(protocol_error(&greeter.auth_state, "authentication message"));
        }

        match auth_message_type {
          AuthMessageType::Secret => {
            greeter.mode = Mode::Password;
            greeter.previous_mode = Mode::Password;
            greeter.asking_for_secret = true;
            greeter.set_prompt(&auth_message);
            greeter.auth_state = AuthState::AwaitingInput(AuthInput::Secret);
          },

          AuthMessageType::Visible => {
            greeter.mode = Mode::Password;
            greeter.previous_mode = Mode::Password;
            greeter.asking_for_secret = false;
            greeter.set_prompt(&auth_message);
            greeter.auth_state = AuthState::AwaitingInput(AuthInput::Visible);
          },

          AuthMessageType::Error => {
            greeter.message = Some(auth_message);
            greeter.auth_state = AuthState::ContinuingAuth;

            self.send(Request::PostAuthMessageResponse { response: None }).await;
          },

          AuthMessageType::Info => {
            greeter.remove_prompt();

            greeter.previous_mode = greeter.mode;
            greeter.mode = Mode::Action;

            if let Some(message) = &mut greeter.message {
              message.push('\n');
              message.push_str(auth_message.trim_end());
            } else {
              greeter.message = Some(auth_message.trim_end().to_string());
            }

            greeter.auth_state = AuthState::ContinuingAuth;
            self.send(Request::PostAuthMessageResponse { response: None }).await;
          },
        }
      },

      Response::Success => {
        let state = std::mem::take(&mut greeter.auth_state);
        if let AuthState::StartingSession(pending, policy) = state {
          tracing::info!("greetd acknowledged session start, exiting");
          commit(pending, policy);
          greeter.auth_state = AuthState::Started;
          control = Some(Control::Exit(AuthStatus::Success));
          return Ok(control);
        }
        if !matches!(state, AuthState::CreatingSession | AuthState::ContinuingAuth) {
          greeter.auth_state = state;
          return Err(protocol_error(&greeter.auth_state, "success"));
        }
        greeter.auth_state = state;

        tracing::info!("authentication successful, starting session");

        let command = greeter.launch_command().map(str::to_string);

        match command {
          None => {
            greeter.show_command_missing();
            self.cancel(greeter);
          },

          Some(command) => {
            greeter.mode = Mode::Processing;

            let session = Session::get_selected(greeter);
            let default = DefaultCommand(&command, greeter.session_source.env());
            let (command, env) = wrap_session_command(greeter, session, &default);
            let command = command.to_string();
            let pending = PendingSession::capture(greeter);
            let policy = CachePolicy::current(greeter);
            greeter.auth_state = AuthState::StartingSession(pending, policy);

            self
              .send(Request::StartSession {
                cmd: vec![command],
                env,
              })
              .await;
          },
        }
      },

      Response::Error { error_type, .. } => {
        // Do not display actual message from greetd, which may contain entered information, sometimes passwords.
        tracing::info!("received an error from greetd: {error_type:?}");

        if !matches!(
          greeter.auth_state,
          AuthState::CreatingSession | AuthState::ContinuingAuth | AuthState::StartingSession(..)
        ) {
          return Err(protocol_error(&greeter.auth_state, "error"));
        }

        match error_type {
          ErrorType::AuthError => {
            greeter.reset_local(true);
            greeter.message = Some(text!(greeter, failed));
            greeter.auth_state = AuthState::CreatingSession;
            self
              .send(Request::CreateSession {
                username: greeter.username.value.clone(),
              })
              .await;
          },

          ErrorType::Error => {
            // Do not display actual message from greetd, which may contain entered information, sometimes passwords.
            greeter.reset_local(false);
            greeter.message = Some(text!(greeter, greetd_error));
          },
        }
      },
    }

    Ok(control)
  }

  pub fn cancel(&self, greeter: &mut Greeter) {
    if !greeter.auth_state.can_cancel() {
      tracing::debug!("ignoring cancellation without a live pre-start transaction");
      return;
    }

    tracing::info!("cancelling session");
    greeter.auth_state = AuthState::Cancelling;
    self.0.cancel_generation.fetch_add(1, Ordering::AcqRel);
    self.0.cancel_notify.notify_waiters();
  }
}

async fn finish_cancellation(greeter: &Arc<RwLock<Greeter>>) {
  let mut greeter = greeter.write().await;
  greeter.reset_local(false);
}

fn request_render(renders: &Sender<Event>) {
  let _ = renders.try_send(Event::Render);
}

async fn persist_cache(store: CacheStore, update: Option<CacheUpdate>) {
  let Some(update) = update.filter(|update| store.is_enabled() && !update.is_noop()) else {
    return;
  };

  match tokio::task::spawn_blocking(move || store.commit(update)).await {
    Ok(Ok(commit)) => {
      for warning in commit.warnings {
        eprintln!("tuigreet: warning: {warning}");
        tracing::warn!("{warning}");
      }
    },
    Ok(Err(error)) => {
      eprintln!("tuigreet: warning: failed to persist remembered state: {error}");
      tracing::warn!("failed to persist remembered state: {error}");
    },
    Err(error) => {
      eprintln!("tuigreet: warning: cache worker failed: {error}");
      tracing::warn!("cache worker failed: {error}");
    },
  }
}

#[cfg(test)]
fn transaction_error(error: TransactionFailure) -> Box<dyn Error + Send + Sync> {
  io::Error::other(transaction_error_message(&error)).into()
}

fn transaction_error_message(error: &TransactionFailure) -> String {
  match error {
    TransactionFailure::Cancelled => "greetd IPC request was cancelled".into(),
    TransactionFailure::Shutdown => "greetd IPC actor is shutting down".into(),
    TransactionFailure::TimedOut => "greetd IPC request timed out".into(),
    TransactionFailure::Transport(error) => error.clone(),
  }
}

fn protocol_error(state: &AuthState, response: &str) -> Box<dyn Error + Send + Sync> {
  io::Error::new(
    io::ErrorKind::InvalidData,
    format!("greetd returned {response} while client state was {state:?}"),
  )
  .into()
}

fn mock_response(request: &Request) -> Response {
  match request {
    Request::CreateSession { .. } => Response::AuthMessage {
      auth_message_type: AuthMessageType::Secret,
      auth_message: "Password: ".to_string(),
    },
    Request::PostAuthMessageResponse { .. } | Request::StartSession { .. } | Request::CancelSession => {
      Response::Success
    },
  }
}

fn desktop_names_to_xdg(names: &str) -> String {
  names.replace(';', ":").trim_end_matches(':').to_string()
}

struct DefaultCommand<'a>(&'a str, Option<Vec<String>>);

impl<'a> DefaultCommand<'a> {
  fn command(&'a self) -> &'a str {
    self.0
  }

  fn env(&'a self) -> Option<&'a Vec<String>> {
    self.1.as_ref()
  }
}

fn wrap_session_command<'a>(
  greeter: &Greeter,
  session: Option<&Session>,
  default: &'a DefaultCommand<'a>,
) -> (Cow<'a, str>, Vec<String>) {
  let mut env: Vec<String> = vec![];

  match session {
    // If the target is a defined session, we should be able to deduce all the
    // environment we need from the desktop file.
    Some(Session {
      slug,
      session_type,
      xdg_desktop_names,
      ..
    }) => {
      if let Some(slug) = slug {
        env.push(format!("XDG_SESSION_DESKTOP={slug}"));
        env.push(format!("DESKTOP_SESSION={slug}"));
      }
      if *session_type != SessionType::None {
        env.push(format!("XDG_SESSION_TYPE={}", session_type.as_xdg_session_type()));
      }
      if let Some(xdg_desktop_names) = xdg_desktop_names {
        env.push(format!(
          "XDG_CURRENT_DESKTOP={}",
          desktop_names_to_xdg(xdg_desktop_names)
        ));
      }

      if *session_type == SessionType::X11 {
        if let Some(ref wrap) = greeter.xsession_wrapper {
          return (Cow::Owned(format!("{} {}", wrap, default.command())), env);
        }
      } else if let Some(ref wrap) = greeter.session_wrapper {
        return (Cow::Owned(format!("{} {}", wrap, default.command())), env);
      }
    },

    _ => {
      // The configured environment belongs to the default command regardless
      // of whether that command is wrapped.
      if let Some(base_env) = default.env() {
        env.extend(base_env.iter().cloned());
      }
      if let Some(ref wrap) = greeter.session_wrapper {
        return (Cow::Owned(format!("{} {}", wrap, default.command())), env);
      }
    },
  }

  (Cow::Borrowed(default.command()), env)
}

#[cfg(test)]
mod test {
  use std::{
    fs::{self, OpenOptions},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    sync::Arc,
    time::Duration,
  };

  use greetd_ipc::{AuthMessageType, ErrorType, Request, Response, codec::TokioCodec};
  use nix::fcntl::{Flock, FlockArg};
  use tempfile::tempdir;
  use tokio::sync::RwLock;

  use super::{
    AuthInput,
    AuthState,
    CachePolicy,
    CachedSession,
    Ipc,
    PendingSession,
    TransactionFailure,
    mock_response,
    persist_cache,
    wrap_session_command,
  };
  use crate::{
    Greeter,
    Mode,
    cache::{CacheStore, CacheUpdate, RememberedSelection, RememberedUser},
    event::{Control, Events, fill_event_queue},
    ipc::{DefaultCommand, desktop_names_to_xdg},
    ui::{
      common::masked::MaskedString,
      sessions::{Session, SessionSource, SessionType},
    },
  };

  #[test]
  fn mock_responses_follow_authentication_flow() {
    assert!(matches!(
      mock_response(&Request::CreateSession {
        username: "test".into()
      }),
      Response::AuthMessage {
        auth_message_type: AuthMessageType::Secret,
        ..
      }
    ));
    assert!(matches!(
      mock_response(&Request::PostAuthMessageResponse {
        response: Some("secret".into())
      }),
      Response::Success
    ));
    assert!(matches!(mock_response(&Request::CancelSession), Response::Success));
  }

  #[tokio::test]
  async fn no_op_cache_persistence_never_touches_damaged_disk_state() {
    let directory = tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let state_path = directory.path().join("state.json");
    let damaged = b"leave this damaged state untouched";
    fs::write(&state_path, damaged).unwrap();
    fs::set_permissions(&state_path, fs::Permissions::from_mode(0o600)).unwrap();
    let store = CacheStore::at(directory.path());
    let update = CacheUpdate::successful_login(
      RememberedUser {
        username: "alice".into(),
        display_name: None,
      },
      Some(RememberedSelection::command("session".into())),
      false,
      false,
      false,
      true,
    );

    persist_cache(store, Some(update)).await;

    assert_eq!(fs::read(state_path).unwrap(), damaged);
  }

  #[tokio::test]
  async fn disabled_commands_still_make_cache_persistence_non_noop() {
    let directory = tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let store = CacheStore::at(directory.path());
    let user = RememberedUser {
      username: "alice".into(),
      display_name: None,
    };
    store
      .commit(CacheUpdate::successful_login(
        user.clone(),
        Some(RememberedSelection::command("session".into())),
        false,
        true,
        false,
        true,
      ))
      .unwrap();

    persist_cache(
      store.clone(),
      Some(CacheUpdate::successful_login(user, None, false, false, false, false)),
    )
    .await;

    assert!(store.load(&[], true, true).state.global_selection().is_none());
  }

  #[tokio::test]
  async fn mock_ipc_does_not_require_a_socket() {
    let mut state = Greeter::default();
    state.mock = true;
    state.auth_state = AuthState::CreatingSession;
    let greeter = Arc::new(RwLock::new(state));
    let ipc = Ipc::new();

    ipc
      .send(Request::CreateSession {
        username: "test".into(),
      })
      .await;
    ipc.handle(greeter.clone()).await.unwrap();

    let greeter = greeter.read().await;
    assert_eq!(greeter.mode, Mode::Password);
    assert!(greeter.asking_for_secret);
    assert_eq!(greeter.prompt.as_deref(), Some("Password: "));
    assert_eq!(greeter.auth_state, AuthState::AwaitingInput(AuthInput::Secret));
  }

  #[tokio::test]
  async fn protocol_state_tracks_visible_and_automatic_messages() {
    let mut greeter = Greeter::default();
    greeter.auth_state = AuthState::CreatingSession;
    let ipc = Ipc::new();

    ipc
      .parse_response(&mut greeter, Response::AuthMessage {
        auth_message_type: AuthMessageType::Visible,
        auth_message: "One-time code: ".into(),
      })
      .await
      .unwrap();
    assert_eq!(greeter.auth_state, AuthState::AwaitingInput(AuthInput::Visible));
    assert!(!greeter.asking_for_secret);

    greeter.auth_state = AuthState::ContinuingAuth;
    ipc
      .parse_response(&mut greeter, Response::AuthMessage {
        auth_message_type: AuthMessageType::Info,
        auth_message: "Touch your security key".into(),
      })
      .await
      .unwrap();
    assert_eq!(greeter.auth_state, AuthState::ContinuingAuth);
    assert!(matches!(
      ipc.next().await,
      Some(Request::PostAuthMessageResponse { response: None })
    ));
  }

  #[tokio::test]
  async fn greetd_errors_obey_the_automatic_cancel_contract() {
    let mut greeter = Greeter::default();
    greeter.username.value = "alice".into();
    greeter.auth_state = AuthState::ContinuingAuth;
    let ipc = Ipc::new();

    ipc
      .parse_response(&mut greeter, Response::Error {
        error_type: ErrorType::AuthError,
        description: "redacted".into(),
      })
      .await
      .unwrap();

    assert_eq!(greeter.auth_state, AuthState::CreatingSession);
    assert!(matches!(
      ipc.next().await,
      Some(Request::CreateSession { username }) if username == "alice"
    ));
  }

  #[tokio::test]
  async fn unexpected_protocol_messages_are_rejected() {
    let mut greeter = Greeter::default();
    let ipc = Ipc::new();

    let error = ipc
      .parse_response(&mut greeter, Response::AuthMessage {
        auth_message_type: AuthMessageType::Secret,
        auth_message: "Password: ".into(),
      })
      .await
      .err()
      .expect("unexpected protocol message was accepted");

    assert!(error.to_string().contains("client state was Idle"));
  }

  #[tokio::test]
  async fn cancellation_is_available_while_waiting_for_greetd() {
    let events = Events::testing().await;
    let (control_tx, _control_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = Greeter::default();
    state.mock = true;
    state.auth_state = AuthState::CreatingSession;
    let greeter = Arc::new(RwLock::new(state));
    let ipc = Ipc::new();
    let actor = tokio::spawn({
      let ipc = ipc.clone();
      let greeter = greeter.clone();
      let renders = events.sender();
      async move { ipc.run(greeter, control_tx, renders).await }
    });

    ipc.cancel(&mut *greeter.write().await);
    tokio::time::timeout(Duration::from_millis(100), async {
      loop {
        if greeter.read().await.auth_state == AuthState::Idle {
          break;
        }
        tokio::task::yield_now().await;
      }
    })
    .await
    .expect("IPC cancellation did not restore an interactive state");

    ipc.shutdown();
    actor.await.unwrap();
  }

  #[tokio::test]
  async fn response_timeout_closes_the_uncertain_connection() {
    let (client, _server) = tokio::net::UnixStream::pair().unwrap();
    let greeter = Arc::new(RwLock::new(Greeter::default()));
    let ipc = Ipc::new();
    let mut stream = Some(client);

    let result = ipc
      .transact(
        &greeter,
        &mut stream,
        &Request::CreateSession {
          username: "alice".into(),
        },
        0,
        Duration::from_millis(20),
      )
      .await;

    assert!(matches!(result, Err(TransactionFailure::TimedOut)));
    assert!(stream.is_none());
  }

  #[tokio::test]
  async fn active_cancellation_sends_cancel_and_drops_the_connection() {
    let (client, mut server) = tokio::net::UnixStream::pair().unwrap();
    let mut state = Greeter::default();
    state.auth_state = AuthState::CreatingSession;
    let greeter = Arc::new(RwLock::new(state));
    let ipc = Ipc::new();
    let transaction = tokio::spawn({
      let ipc = ipc.clone();
      let greeter = greeter.clone();
      async move {
        let mut stream = Some(client);
        let result = ipc
          .transact(
            &greeter,
            &mut stream,
            &Request::CreateSession {
              username: "alice".into(),
            },
            0,
            Duration::from_secs(1),
          )
          .await;
        (result, stream)
      }
    });

    assert!(matches!(
      Request::read_from(&mut server).await.unwrap(),
      Request::CreateSession { .. }
    ));
    ipc.cancel(&mut *greeter.write().await);
    assert!(matches!(
      tokio::time::timeout(Duration::from_millis(100), Request::read_from(&mut server))
        .await
        .expect("CancelSession was not written")
        .unwrap(),
      Request::CancelSession
    ));

    let (result, stream) = transaction.await.unwrap();
    assert!(matches!(result, Err(TransactionFailure::Cancelled)));
    assert!(stream.is_none());
  }

  #[tokio::test]
  async fn successful_session_exit_does_not_block_on_a_full_event_queue() {
    let events = Events::testing().await;
    fill_event_queue(&events);

    let mut state = Greeter::default();
    state.mock = true;
    let policy = CachePolicy::current(&state);
    state.auth_state = AuthState::StartingSession(
      PendingSession {
        username: "test".into(),
        display_name: None,
        selection: CachedSession::None,
      },
      policy,
    );
    let greeter = Arc::new(RwLock::new(state));
    let ipc = Ipc::new();
    ipc
      .send(Request::StartSession {
        cmd: vec!["true".into()],
        env: Vec::new(),
      })
      .await;

    let control = tokio::time::timeout(Duration::from_millis(100), ipc.handle(greeter))
      .await
      .expect("successful authentication blocked on the full render/event queue")
      .unwrap();

    assert!(matches!(control, Some(Control::Exit(crate::AuthStatus::Success))));
  }

  #[tokio::test]
  async fn cache_contention_is_bounded_and_never_holds_the_greeter_lock() {
    let directory = tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let lock_file = OpenOptions::new()
      .read(true)
      .write(true)
      .create(true)
      .truncate(false)
      .mode(0o600)
      .open(directory.path().join(".state.lock"))
      .unwrap();
    let lock = Flock::lock(lock_file, FlockArg::LockExclusive).unwrap();
    let store = CacheStore::at(directory.path());

    let mut state = Greeter::default();
    state.mock = true;
    state.cache_store = store.clone();
    state.auth_state = AuthState::StartingSession(
      PendingSession {
        username: "alice".into(),
        display_name: Some("Alice".into()),
        selection: CachedSession::None,
      },
      CachePolicy {
        remember_username: true,
        remember_session: false,
        remember_user_session: false,
        allow_command_editor: false,
      },
    );
    let greeter = Arc::new(RwLock::new(state));
    let ipc = Ipc::new();
    ipc
      .send(Request::StartSession {
        cmd: vec!["true".into()],
        env: Vec::new(),
      })
      .await;
    let handle = tokio::spawn({
      let ipc = ipc.clone();
      let greeter = greeter.clone();
      async move { ipc.handle(greeter).await }
    });

    tokio::time::timeout(Duration::from_secs(1), async {
      loop {
        if greeter.read().await.auth_state == AuthState::Started {
          break;
        }
        tokio::task::yield_now().await;
      }
    })
    .await
    .expect("cache I/O retained the Greeter write lock");

    let control = tokio::time::timeout(Duration::from_secs(1), handle)
      .await
      .expect("cache contention blocked successful login")
      .unwrap()
      .unwrap();
    assert!(matches!(control, Some(Control::Exit(crate::AuthStatus::Success))));
    drop(lock.unlock().unwrap());
    assert!(store.load(&[], false, true).state.last_user().is_none());
  }

  #[tokio::test]
  async fn missing_socket_is_not_implicitly_mocked() {
    let greeter = Arc::new(RwLock::new(Greeter::default()));
    let ipc = Ipc::new();

    ipc.send(Request::CancelSession).await;

    assert!(ipc.handle(greeter).await.is_err());
  }

  #[tokio::test]
  async fn session_cache_uses_the_acknowledged_start_snapshot() {
    let mut greeter = Greeter::default();
    greeter.mock = true;
    greeter.allow_command_editor = true;
    greeter.remember = true;
    greeter.remember_session = true;
    greeter.username = MaskedString::from("original-user".into(), Some("Original User".into()));
    greeter.session_source = SessionSource::Command("original-command".into());
    greeter.auth_state = AuthState::CreatingSession;
    let ipc = Ipc::new();
    let committed = Arc::new(std::sync::Mutex::new(Vec::new()));

    ipc
      .parse_response_with(&mut greeter, Response::Success, {
        let committed = Arc::clone(&committed);
        move |pending, policy| {
          committed.lock().unwrap().push((
            pending,
            (
              policy.remember_username,
              policy.remember_session,
              policy.remember_user_session,
              policy.allow_command_editor,
            ),
          ));
        }
      })
      .await
      .unwrap();

    assert!(committed.lock().unwrap().is_empty());
    assert!(matches!(greeter.auth_state, AuthState::StartingSession(..)));

    greeter.username = MaskedString::from("changed-user".into(), None);
    greeter.session_source = SessionSource::Command("changed-command".into());
    greeter.remember = false;
    greeter.remember_session = false;
    greeter.remember_user_session = true;
    greeter.allow_command_editor = false;

    ipc
      .parse_response_with(&mut greeter, Response::Success, {
        let committed = Arc::clone(&committed);
        move |pending, policy| {
          committed.lock().unwrap().push((
            pending,
            (
              policy.remember_username,
              policy.remember_session,
              policy.remember_user_session,
              policy.allow_command_editor,
            ),
          ));
        }
      })
      .await
      .unwrap();

    let committed = committed.lock().unwrap();
    assert_eq!(committed.len(), 1);
    assert_eq!(committed[0].0.username, "original-user");
    assert_eq!(committed[0].0.display_name.as_deref(), Some("Original User"));
    assert_eq!(
      &committed[0].0.selection,
      &CachedSession::Command("original-command".into())
    );
    assert_eq!(committed[0].1, (true, true, false, true));
    assert_eq!(greeter.auth_state, AuthState::Started);
  }

  #[tokio::test]
  async fn session_start_uses_the_selected_command_in_debug_builds() {
    let mut greeter = Greeter::default();
    greeter.mock = true;
    greeter.session_source = SessionSource::DefaultCommand("actual-session --flag".into(), None);
    greeter.auth_state = AuthState::CreatingSession;
    let ipc = Ipc::new();

    ipc
      .parse_response_with(&mut greeter, Response::Success, |_, _| unreachable!())
      .await
      .unwrap();

    assert!(matches!(
      ipc.next().await,
      Some(Request::StartSession { cmd, .. }) if cmd == ["actual-session --flag"]
    ));
  }

  #[tokio::test]
  async fn failed_session_start_discards_the_cache_snapshot() {
    let mut greeter = Greeter::default();
    greeter.mock = true;
    greeter.allow_command_editor = true;
    greeter.remember_session = true;
    greeter.session_source = SessionSource::Command("do-not-cache".into());
    greeter.auth_state = AuthState::CreatingSession;
    let ipc = Ipc::new();
    let commits = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    ipc
      .parse_response_with(&mut greeter, Response::Success, |_, _| unreachable!())
      .await
      .unwrap();
    assert!(matches!(greeter.auth_state, AuthState::StartingSession(..)));

    ipc
      .parse_response_with(
        &mut greeter,
        Response::Error {
          error_type: ErrorType::Error,
          description: "start failed".into(),
        },
        {
          let commits = Arc::clone(&commits);
          move |_, _| {
            commits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
          }
        },
      )
      .await
      .unwrap();

    assert_eq!(commits.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert_eq!(greeter.auth_state, AuthState::Idle);
  }

  #[tokio::test]
  async fn disabled_command_editor_rejects_free_form_sources() {
    let mut greeter = Greeter::default();
    greeter.mock = true;
    greeter.session_source = SessionSource::Command("untrusted-command".into());
    greeter.auth_state = AuthState::CreatingSession;
    let ipc = Ipc::new();

    ipc
      .parse_response_with(&mut greeter, Response::Success, |_, _| unreachable!())
      .await
      .unwrap();

    assert_eq!(greeter.auth_state, AuthState::Cancelling);
    assert_eq!(greeter.mode, Mode::Username);
  }

  #[test]
  fn wayland_no_wrapper() {
    let greeter = Greeter::default();

    let session = Session {
      name: "Session1".into(),
      session_type: SessionType::Wayland,
      command: "Session1Cmd".into(),
      path: Some(PathBuf::from("/Session1Path")),
      ..Default::default()
    };

    let default = DefaultCommand(&session.command, None);
    let (command, env) = wrap_session_command(&greeter, Some(&session), &default);

    assert_eq!(command.as_ref(), "Session1Cmd");
    assert_eq!(env, vec!["XDG_SESSION_TYPE=wayland"]);
  }

  #[test]
  fn default_command_wrapper_preserves_configured_environment() {
    let mut greeter = Greeter::default();
    greeter.session_wrapper = Some("/wrapper.sh --flag".into());
    let environment = vec!["DISPLAY=:7".into(), "XDG_CURRENT_DESKTOP=custom".into()];
    let default = DefaultCommand("default-session --argument", Some(environment.clone()));

    let (command, env) = wrap_session_command(&greeter, None, &default);

    assert_eq!(command.as_ref(), "/wrapper.sh --flag default-session --argument");
    assert_eq!(env, environment);
  }

  #[test]
  fn wayland_wrapper() {
    let mut greeter = Greeter::default();
    greeter.session_wrapper = Some("/wrapper.sh".into());

    let session = Session {
      name: "Session1".into(),
      session_type: SessionType::Wayland,
      command: "Session1Cmd".into(),
      path: Some(PathBuf::from("/Session1Path")),
      ..Default::default()
    };

    let default = DefaultCommand(&session.command, None);
    let (command, env) = wrap_session_command(&greeter, Some(&session), &default);

    assert_eq!(command.as_ref(), "/wrapper.sh Session1Cmd");
    assert_eq!(env, vec!["XDG_SESSION_TYPE=wayland"]);
  }

  #[test]
  fn x11_wrapper() {
    let mut greeter = Greeter::default();
    greeter.xsession_wrapper = Some("startx /usr/bin/env".into());

    let session = Session {
      slug: Some("thede".to_string()),
      name: "Session1".into(),
      session_type: SessionType::X11,
      command: "Session1Cmd".into(),
      path: Some(PathBuf::from("/Session1Path")),
      xdg_desktop_names: Some("one;two;three;".to_string()),
    };

    let default = DefaultCommand(&session.command, None);
    let (command, env) = wrap_session_command(&greeter, Some(&session), &default);

    assert_eq!(command.as_ref(), "startx /usr/bin/env Session1Cmd");
    assert_eq!(env, vec![
      "XDG_SESSION_DESKTOP=thede",
      "DESKTOP_SESSION=thede",
      "XDG_SESSION_TYPE=x11",
      "XDG_CURRENT_DESKTOP=one:two:three"
    ]);
  }

  #[test]
  fn xdg_current_desktop() {
    assert_eq!(desktop_names_to_xdg("one;two;three four"), "one:two:three four");
    assert_eq!(desktop_names_to_xdg("one;"), "one");
    assert_eq!(desktop_names_to_xdg(""), "");
    assert_eq!(desktop_names_to_xdg(";"), "");
  }
}
