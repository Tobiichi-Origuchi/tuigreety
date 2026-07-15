use std::{borrow::Cow, error::Error, io, path::PathBuf, sync::Arc};

use greetd_ipc::{AuthMessageType, ErrorType, Request, Response, codec::TokioCodec};
use tokio::sync::{
  Mutex,
  RwLock,
  mpsc::{Receiver, Sender},
};

use crate::{
  AuthStatus,
  Greeter,
  Mode,
  event::Control,
  info::{
    delete_last_command,
    delete_last_session,
    delete_last_user_command,
    delete_last_user_session,
    write_last_command,
    write_last_session_path,
    write_last_user_command,
    write_last_user_session,
    write_last_username,
  },
  macros::SafeDebug,
  ui::{
    common::masked::MaskedString,
    sessions::{Session, SessionSource, SessionType},
  },
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CachedSession {
  Command(String),
  Session(PathBuf),
  None,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PendingSession {
  username: String,
  display_name: Option<String>,
  selection: CachedSession,
}

#[derive(Clone, Copy)]
struct CachePolicy {
  remember_username: bool,
  remember_session: bool,
  remember_user_session: bool,
  allow_command_editor: bool,
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
        .and_then(|session| session.path.clone())
        .map_or(CachedSession::None, CachedSession::Session),
      _ => CachedSession::None,
    };

    Self {
      username: greeter.username.value.clone(),
      display_name: greeter.username.mask.clone(),
      selection,
    }
  }
}

fn commit_pending_session(pending: PendingSession, policy: CachePolicy) {
  if policy.remember_username {
    write_last_username(&MaskedString::from(pending.username.clone(), pending.display_name));
  }

  let ignored_command = CachedSession::None;
  let selection = match &pending.selection {
    CachedSession::Command(_) if !policy.allow_command_editor => &ignored_command,
    selection => selection,
  };

  if policy.remember_session {
    match selection {
      CachedSession::Command(command) => {
        write_last_command(command);
        delete_last_session();
      },
      CachedSession::Session(path) => {
        write_last_session_path(path);
        delete_last_command();
      },
      CachedSession::None => {
        delete_last_command();
        delete_last_session();
      },
    }
  }

  if policy.remember_user_session {
    match selection {
      CachedSession::Command(command) => {
        write_last_user_command(&pending.username, command);
        delete_last_user_session(&pending.username);
      },
      CachedSession::Session(path) => {
        write_last_user_session(&pending.username, path);
        delete_last_user_command(&pending.username);
      },
      CachedSession::None => {
        delete_last_user_command(&pending.username);
        delete_last_user_session(&pending.username);
      },
    }
  }
}

#[derive(Clone)]
pub struct Ipc(Arc<IpcHandle>);

pub struct IpcHandle {
  tx: RwLock<Sender<Request>>,
  rx: Mutex<Receiver<Request>>,
}

impl Ipc {
  pub fn new() -> Ipc {
    let (tx, rx) = tokio::sync::mpsc::channel::<Request>(10);

    Ipc(Arc::new(IpcHandle {
      tx: RwLock::new(tx),
      rx: Mutex::new(rx),
    }))
  }

  pub async fn send(&self, request: Request) {
    tracing::info!("sending request to greetd: {}", request.safe_repr());

    let _ = self.0.tx.read().await.send(request).await;
  }

  pub async fn next(&mut self) -> Option<Request> {
    self.0.rx.lock().await.recv().await
  }

  pub async fn handle(&mut self, greeter: Arc<RwLock<Greeter>>) -> Result<Option<Control>, Box<dyn Error>> {
    let request = self.next().await;

    if let Some(request) = request {
      let (stream, mock) = {
        let greeter = greeter.read().await;

        (greeter.stream.as_ref().map(Arc::clone), greeter.mock)
      };

      let response = if let Some(stream) = stream {
        let mut stream = stream.write().await;
        request.write_to(&mut *stream).await?;
        let response = Response::read_from(&mut *stream).await?;
        drop(stream);

        greeter.write().await.working = false;

        response
      } else if mock {
        greeter.write().await.working = false;
        mock_response(&request)
      } else {
        return Err(io::Error::new(io::ErrorKind::NotConnected, "greetd socket is not connected").into());
      };

      return self.parse_response(&mut *greeter.write().await, response).await;
    }

    Ok(None)
  }

  async fn parse_response(
    &mut self,
    greeter: &mut Greeter,
    response: Response,
  ) -> Result<Option<Control>, Box<dyn Error>> {
    self
      .parse_response_with(greeter, response, commit_pending_session)
      .await
  }

  async fn parse_response_with<F>(
    &mut self,
    greeter: &mut Greeter,
    response: Response,
    commit: F,
  ) -> Result<Option<Control>, Box<dyn Error>>
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
      } => match auth_message_type {
        AuthMessageType::Secret => {
          greeter.mode = Mode::Password;
          greeter.working = false;
          greeter.asking_for_secret = true;
          greeter.set_prompt(&auth_message);
        },

        AuthMessageType::Visible => {
          greeter.mode = Mode::Password;
          greeter.working = false;
          greeter.asking_for_secret = false;
          greeter.set_prompt(&auth_message);
        },

        AuthMessageType::Error => {
          greeter.message = Some(auth_message);

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

          self.send(Request::PostAuthMessageResponse { response: None }).await;
        },
      },

      Response::Success => {
        if greeter.done {
          tracing::info!("greetd acknowledged session start, exiting");

          if let Some(pending) = greeter.pending_session.take() {
            commit(pending, CachePolicy::current(greeter));
          } else {
            tracing::warn!("session start was acknowledged without a pending session snapshot");
          }

          control = Some(Control::Exit(AuthStatus::Success));
        } else {
          tracing::info!("authentication successful, starting session");

          let command = if !greeter.allow_command_editor && matches!(&greeter.session_source, SessionSource::Command(_))
          {
            tracing::warn!("refusing a free-form session command because the command editor is disabled");
            None
          } else {
            greeter.session_source.command(greeter).map(str::to_string)
          };

          match command {
            None => {
              Ipc::cancel(greeter).await;

              greeter.message = Some(text!(greeter, command_missing));
              greeter.reset(false).await;
            },

            Some(command) if command.is_empty() => {
              Ipc::cancel(greeter).await;

              greeter.message = Some(text!(greeter, command_missing));
              greeter.reset(false).await;
            },

            Some(command) => {
              greeter.done = true;
              greeter.mode = Mode::Processing;

              let session = Session::get_selected(greeter);
              let default = DefaultCommand(&command, greeter.session_source.env());
              let (command, env) = wrap_session_command(greeter, session, &default);
              greeter.pending_session = Some(PendingSession::capture(greeter));

              #[cfg(not(debug_assertions))]
              self
                .send(Request::StartSession {
                  cmd: vec![command.to_string()],
                  env,
                })
                .await;

              #[cfg(debug_assertions)]
              {
                let _ = command;

                self
                  .send(Request::StartSession {
                    cmd: vec!["true".to_string()],
                    env,
                  })
                  .await;
              }
            },
          }
        }
      },

      Response::Error { error_type, .. } => {
        // Do not display actual message from greetd, which may contain entered information, sometimes passwords.
        tracing::info!("received an error from greetd: {error_type:?}");

        Ipc::cancel(greeter).await;

        match error_type {
          ErrorType::AuthError => {
            greeter.message = Some(text!(greeter, failed));
            self
              .send(Request::CreateSession {
                username: greeter.username.value.clone(),
              })
              .await;
            greeter.reset(true).await;
          },

          ErrorType::Error => {
            // Do not display actual message from greetd, which may contain entered information, sometimes passwords.
            greeter.message = Some(text!(greeter, greetd_error));
            greeter.reset(false).await;
          },
        }
      },
    }

    Ok(control)
  }

  pub async fn cancel(greeter: &mut Greeter) {
    tracing::info!("cancelling session");

    if greeter.mock {
      return;
    }

    let _ = Request::CancelSession.write_to(&mut *greeter.stream().await).await;
  }
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
      // If a wrapper script is used, assume that it is able to set up the
      // required environment.
      if let Some(ref wrap) = greeter.session_wrapper {
        return (Cow::Owned(format!("{} {}", wrap, default.command())), env);
      }
      // Otherwise, set up the environment from the provided argument.
      if let Some(base_env) = default.env() {
        env.append(&mut base_env.clone());
      }
    },
  }

  (Cow::Borrowed(default.command()), env)
}

#[cfg(test)]
mod test {
  use std::{path::PathBuf, sync::Arc, time::Duration};

  use greetd_ipc::{AuthMessageType, ErrorType, Request, Response};
  use tokio::sync::RwLock;

  use super::{CachedSession, Ipc, mock_response, wrap_session_command};
  use crate::{
    Greeter,
    Mode,
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
  async fn mock_ipc_does_not_require_a_socket() {
    let mut state = Greeter::default();
    state.mock = true;
    let greeter = Arc::new(RwLock::new(state));
    let mut ipc = Ipc::new();

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
  }

  #[tokio::test]
  async fn successful_session_exit_does_not_block_on_a_full_event_queue() {
    let events = Events::new().await;
    fill_event_queue(&events);

    let mut state = Greeter::default();
    state.mock = true;
    state.done = true;
    state.pending_session = Some(super::PendingSession {
      username: "test".into(),
      display_name: None,
      selection: CachedSession::None,
    });
    let greeter = Arc::new(RwLock::new(state));
    let mut ipc = Ipc::new();
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
  async fn missing_socket_is_not_implicitly_mocked() {
    let greeter = Arc::new(RwLock::new(Greeter::default()));
    let mut ipc = Ipc::new();

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
    let mut ipc = Ipc::new();
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
    assert!(greeter.done);
    assert!(greeter.pending_session.is_some());

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
    assert_eq!(committed[0].1, (false, false, true, false));
    assert!(greeter.pending_session.is_none());
  }

  #[tokio::test]
  async fn failed_session_start_discards_the_cache_snapshot() {
    let mut greeter = Greeter::default();
    greeter.mock = true;
    greeter.allow_command_editor = true;
    greeter.remember_session = true;
    greeter.session_source = SessionSource::Command("do-not-cache".into());
    let mut ipc = Ipc::new();
    let commits = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    ipc
      .parse_response_with(&mut greeter, Response::Success, |_, _| unreachable!())
      .await
      .unwrap();
    assert!(greeter.pending_session.is_some());

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
    assert!(greeter.pending_session.is_none());
    assert!(!greeter.done);
  }

  #[tokio::test]
  async fn disabled_command_editor_rejects_free_form_sources() {
    let mut greeter = Greeter::default();
    greeter.mock = true;
    greeter.session_source = SessionSource::Command("untrusted-command".into());
    let mut ipc = Ipc::new();

    ipc
      .parse_response_with(&mut greeter, Response::Success, |_, _| unreachable!())
      .await
      .unwrap();

    assert!(!greeter.done);
    assert!(greeter.pending_session.is_none());
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
