use std::{error::Error, sync::Arc};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use greetd_ipc::Request;
use tokio::sync::RwLock;

use crate::{
  AuthStatus,
  Greeter,
  Mode,
  info::{get_last_user_command, get_last_user_session},
  ipc::Ipc,
  power::power,
  ui::{
    common::masked::MaskedString,
    sessions::{Session, SessionSource},
    users::User,
  },
};

// Act on keyboard events.
//
// This function will be called whenever a keyboard event was captured by the
// application. It takes a reference to the `Greeter` so it can be aware of the
// current state of the application and act accordinly; It also receives the
// `Ipc` interface so it is able to interact with `greetd` if necessary.
pub async fn handle(
  greeter: Arc<RwLock<Greeter>>,
  input: KeyEvent,
  ipc: Ipc,
) -> Result<Option<AuthStatus>, Box<dyn Error>> {
  let mut greeter = greeter.write().await;

  if greeter.working {
    return Ok(None);
  }

  match input {
    // ^U should erase the current buffer.
    KeyEvent {
      code: KeyCode::Char('u'),
      modifiers: KeyModifiers::CONTROL,
      ..
    } => match greeter.mode {
      Mode::Username => greeter.username = MaskedString::default(),
      Mode::Password => greeter.buffer = String::new(),
      Mode::Command => greeter.buffer = String::new(),
      _ => {},
    },

    // In debug mode only, ^X will exit the application.
    #[cfg(debug_assertions)]
    KeyEvent {
      code: KeyCode::Char('x'),
      modifiers: KeyModifiers::CONTROL,
      ..
    } => {
      return Ok(Some(AuthStatus::Cancel));
    },

    // Depending on the active screen, pressing Escape will either return to the
    // previous mode (close a popup, for example), or cancel the `greetd`
    // session.
    KeyEvent { code: KeyCode::Esc, .. } => match greeter.mode {
      Mode::Command => {
        greeter.mode = greeter.previous_mode;
        greeter.buffer = greeter.previous_buffer.take().unwrap_or_default();
        greeter.cursor_offset = 0;
      },

      Mode::Users | Mode::Sessions | Mode::Power => {
        greeter.mode = greeter.previous_mode;
      },

      _ => {
        Ipc::cancel(&mut greeter).await;
        greeter.reset(false).await;
      },
    },

    // Simple cursor directions in text fields.
    KeyEvent {
      code: KeyCode::Left, ..
    } => greeter.cursor_offset -= 1,
    KeyEvent {
      code: KeyCode::Right, ..
    } => greeter.cursor_offset += 1,

    // F2 will display the command entry prompt. If we are already in one of the
    // popup screens, we set the previous screen as being the current previous
    // screen.
    KeyEvent {
      code: KeyCode::F(i), ..
    } if i == greeter.kb_command && greeter.allow_command_editor => {
      greeter.previous_mode = match greeter.mode {
        Mode::Users | Mode::Command | Mode::Sessions | Mode::Power => greeter.previous_mode,
        _ => greeter.mode,
      };

      // Set the edition buffer to the current command.
      greeter.previous_buffer = Some(greeter.buffer.clone());
      greeter.buffer = greeter
        .session_source
        .command(&greeter)
        .map(str::to_string)
        .unwrap_or_default();
      greeter.cursor_offset = 0;
      greeter.mode = Mode::Command;
    },

    // F3 will display the session selection menu. If we are already in one of
    // the popup screens, we set the previous screen as being the current
    // previous screen.
    KeyEvent {
      code: KeyCode::F(i), ..
    } if i == greeter.kb_sessions && !greeter.sessions.options.is_empty() => {
      greeter.previous_mode = match greeter.mode {
        Mode::Users | Mode::Command | Mode::Sessions | Mode::Power => greeter.previous_mode,
        _ => greeter.mode,
      };

      greeter.mode = Mode::Sessions;
    },

    // F12 will display the user selection menu. If we are already in one of the
    // popup screens, we set the previous screen as being the current previous
    // screen.
    KeyEvent {
      code: KeyCode::F(i), ..
    } if i == greeter.kb_power && !greeter.powers.options.is_empty() => {
      greeter.previous_mode = match greeter.mode {
        Mode::Users | Mode::Command | Mode::Sessions | Mode::Power => greeter.previous_mode,
        _ => greeter.mode,
      };

      greeter.mode = Mode::Power;
    },

    // Handle moving up in menus.
    KeyEvent { code: KeyCode::Up, .. } => {
      if let Mode::Users = greeter.mode
        && greeter.users.selected > 0
      {
        greeter.users.selected -= 1;
      }

      if let Mode::Sessions = greeter.mode
        && greeter.sessions.selected > 0
      {
        greeter.sessions.selected -= 1;
      }

      if let Mode::Power = greeter.mode
        && greeter.powers.selected > 0
      {
        greeter.powers.selected -= 1;
      }
    },

    // Handle moving down in menus.
    KeyEvent {
      code: KeyCode::Down, ..
    } => {
      if let Mode::Users = greeter.mode
        && greeter.users.selected + 1 < greeter.users.options.len()
      {
        greeter.users.selected += 1;
      }

      if let Mode::Sessions = greeter.mode
        && greeter.sessions.selected + 1 < greeter.sessions.options.len()
      {
        greeter.sessions.selected += 1;
      }

      if let Mode::Power = greeter.mode
        && greeter.powers.selected + 1 < greeter.powers.options.len()
      {
        greeter.powers.selected += 1;
      }
    },

    // ^A should go to the start of the current prompt
    KeyEvent {
      code: KeyCode::Char('a'),
      modifiers: KeyModifiers::CONTROL,
      ..
    } => {
      let value = {
        match greeter.mode {
          Mode::Username => &greeter.username.value,
          _ => &greeter.buffer,
        }
      };

      greeter.cursor_offset = -(value.chars().count() as i16);
    },

    // ^A should go to the end of the current prompt
    KeyEvent {
      code: KeyCode::Char('e'),
      modifiers: KeyModifiers::CONTROL,
      ..
    } => greeter.cursor_offset = 0,

    // With completion enabled, Tab completes a unique username or expands a
    // shared prefix. A second Tab on a complete username submits it, retaining
    // the original Tab behavior without submitting ambiguous prefixes.
    KeyEvent { code: KeyCode::Tab, .. } => match greeter.mode {
      Mode::Username if greeter.user_autocomplete => {
        if complete_username(&mut greeter) == Completion::Exact {
          validate_username(&mut greeter, &ipc).await;
        }
      },
      Mode::Username if !greeter.username.value.is_empty() => validate_username(&mut greeter, &ipc).await,
      _ => {},
    },

    // Enter validates the current entry, depending on the active mode.
    KeyEvent {
      code: KeyCode::Enter, ..
    } => match greeter.mode {
      Mode::Username if !greeter.username.value.is_empty() => validate_username(&mut greeter, &ipc).await,

      Mode::Username if greeter.user_menu && !greeter.users.options.is_empty() => {
        greeter.previous_mode = match greeter.mode {
          Mode::Users | Mode::Command | Mode::Sessions | Mode::Power => greeter.previous_mode,
          _ => greeter.mode,
        };

        greeter.buffer = greeter.previous_buffer.take().unwrap_or_default();
        greeter.mode = Mode::Users;
      },

      Mode::Username => {},

      Mode::Password => {
        greeter.working = true;
        greeter.message = None;

        ipc
          .send(Request::PostAuthMessageResponse {
            response: Some(greeter.buffer.clone()),
          })
          .await;

        greeter.buffer = String::new();
      },

      Mode::Command if greeter.allow_command_editor => {
        greeter.sessions.selected = 0;
        greeter.session_source = SessionSource::Command(greeter.buffer.clone());

        greeter.buffer = greeter.previous_buffer.take().unwrap_or_default();
        greeter.mode = greeter.previous_mode;
      },

      Mode::Command => {
        greeter.buffer = greeter.previous_buffer.take().unwrap_or_default();
        greeter.cursor_offset = 0;
        greeter.mode = greeter.previous_mode;
      },

      Mode::Users => {
        let username = greeter.users.options.get(greeter.users.selected).cloned();

        if let Some(User { username, name }) = username {
          greeter.username = MaskedString::from(username, name);
        }

        greeter.mode = greeter.previous_mode;

        validate_username(&mut greeter, &ipc).await;
      },

      Mode::Sessions => {
        let session = greeter.sessions.options.get(greeter.sessions.selected).cloned();

        if session.is_some() {
          greeter.session_source = SessionSource::Session(greeter.sessions.selected);
        }

        greeter.mode = greeter.previous_mode;
      },

      Mode::Power => {
        let power_command = greeter.powers.options.get(greeter.powers.selected).cloned();

        if let Some(command) = power_command {
          power(&mut greeter, command.action).await;
        }

        greeter.mode = greeter.previous_mode;
      },

      _ => {},
    },

    // Do not handle any other controls keybindings
    KeyEvent {
      modifiers: KeyModifiers::CONTROL,
      ..
    } => {},

    // Handle free-form entry of characters.
    KeyEvent {
      code: KeyCode::Char(c), ..
    } => insert_key(&mut greeter, c).await,

    // Handle deletion of characters.
    KeyEvent {
      code: KeyCode::Backspace,
      ..
    }
    | KeyEvent {
      code: KeyCode::Delete, ..
    } => delete_key(&mut greeter, input.code).await,

    _ => {},
  }

  Ok(None)
}

#[derive(Debug, Eq, PartialEq)]
enum Completion {
  Changed,
  Exact,
  None,
}

fn complete_username(greeter: &mut Greeter) -> Completion {
  let input = greeter.username.value.as_str();
  let matches = greeter
    .users
    .options
    .iter()
    .map(|user| user.username.as_str())
    .filter(|username| username.starts_with(input))
    .collect::<Vec<_>>();

  if matches.contains(&input) {
    return Completion::Exact;
  }

  let Some(completion) = common_prefix(&matches) else {
    return Completion::None;
  };

  if completion == input {
    return Completion::None;
  }

  greeter.username.value = completion;
  greeter.username.mask = None;
  greeter.cursor_offset = 0;
  Completion::Changed
}

fn common_prefix(values: &[&str]) -> Option<String> {
  let mut prefix = values.first()?.chars().collect::<Vec<_>>();

  for value in &values[1..] {
    let matching = prefix
      .iter()
      .zip(value.chars())
      .take_while(|(left, right)| left == &right)
      .count();
    prefix.truncate(matching);
  }

  Some(prefix.into_iter().collect())
}

// Handle insertion of characters into the proper buffer, depending on the
// current mode and the position of the cursor.
async fn insert_key(greeter: &mut Greeter, c: char) {
  let value = match greeter.mode {
    Mode::Username => &greeter.username.value,
    Mode::Password => &greeter.buffer,
    Mode::Command => &greeter.buffer,
    _ => return,
  };

  let index = (value.chars().count() as i16 + greeter.cursor_offset) as usize;
  let left = value.chars().take(index);
  let right = value.chars().skip(index);

  let value = left.chain(vec![c]).chain(right).collect();
  let mode = greeter.mode;

  match mode {
    Mode::Username => greeter.username.value = value,
    Mode::Password => greeter.buffer = value,
    Mode::Command => greeter.buffer = value,
    _ => {},
  };
}

// Handle deletion of characters from a prompt into the proper buffer, depending
// on the current mode, whether Backspace or Delete was pressed and the position
// of the cursor.
async fn delete_key(greeter: &mut Greeter, key: KeyCode) {
  let value = match greeter.mode {
    Mode::Username => &greeter.username.value,
    Mode::Password => &greeter.buffer,
    Mode::Command => &greeter.buffer,
    _ => return,
  };

  let index = match key {
    KeyCode::Backspace => (value.chars().count() as i16 + greeter.cursor_offset - 1) as usize,
    KeyCode::Delete => (value.chars().count() as i16 + greeter.cursor_offset) as usize,
    _ => 0,
  };

  if value.chars().nth(index).is_some() {
    let left = value.chars().take(index);
    let right = value.chars().skip(index + 1);

    let value = left.chain(right).collect();

    match greeter.mode {
      Mode::Username => greeter.username.value = value,
      Mode::Password => greeter.buffer = value,
      Mode::Command => greeter.buffer = value,
      _ => return,
    };

    if let KeyCode::Delete = key {
      greeter.cursor_offset += 1;
    }
  }
}

// Creates a `greetd` session for the provided username.
async fn validate_username(greeter: &mut Greeter, ipc: &Ipc) {
  greeter.working = true;
  greeter.message = None;

  ipc
    .send(Request::CreateSession {
      username: greeter.username.value.clone(),
    })
    .await;
  greeter.buffer = String::new();

  #[cfg(not(test))]
  if !greeter.allow_command_editor {
    crate::info::delete_last_user_command(&greeter.username.value);
  }

  if greeter.remember_user_session {
    if let Ok(last_session) = get_last_user_session(&greeter.username.value)
      && let Some(last_session) = Session::from_path(greeter, last_session).cloned()
    {
      tracing::info!("remembered user session is {}", last_session.name);

      greeter.sessions.selected = greeter
        .sessions
        .options
        .iter()
        .position(|sess| sess.path == last_session.path)
        .unwrap_or(0);
      greeter.session_source = SessionSource::Session(greeter.sessions.selected);
    }

    if greeter.allow_command_editor
      && let Ok(command) = get_last_user_command(&greeter.username.value)
    {
      tracing::info!("remembered user command is {}", command);

      greeter.session_source = SessionSource::Command(command);
    }
  }
}

#[cfg(test)]
mod test {
  use std::sync::Arc;

  use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
  use tokio::sync::RwLock;

  use super::{Completion, common_prefix, complete_username, handle};
  use crate::{
    AuthStatus,
    Greeter,
    Mode,
    ipc::Ipc,
    power::PowerOption,
    ui::{
      common::masked::MaskedString,
      power::Power,
      sessions::{Session, SessionSource},
      users::User,
    },
  };

  #[test]
  fn username_completion_is_prefix_based() {
    let mut greeter = Greeter::default();
    greeter.user_autocomplete = true;
    greeter.users.options = vec![
      User {
        username: "origuchi".into(),
        name: None,
      },
      User {
        username: "oxxxxxx".into(),
        name: None,
      },
    ];

    greeter.username.value = "o".into();
    assert_eq!(complete_username(&mut greeter), Completion::None);
    assert_eq!(greeter.username.value, "o");

    greeter.username.value = "or".into();
    assert_eq!(complete_username(&mut greeter), Completion::Changed);
    assert_eq!(greeter.username.value, "origuchi");

    assert_eq!(complete_username(&mut greeter), Completion::Exact);

    greeter.username.value = "nobody".into();
    assert_eq!(complete_username(&mut greeter), Completion::None);
  }

  #[test]
  fn empty_prefix_completes_a_sole_user() {
    let mut greeter = Greeter::default();
    greeter.users.options.push(User {
      username: "origuchi".into(),
      name: Some("Origuchi".into()),
    });

    assert_eq!(complete_username(&mut greeter), Completion::Changed);
    assert_eq!(greeter.username.value, "origuchi");
    assert!(greeter.username.mask.is_none());
  }

  #[test]
  fn common_prefix_handles_characters_not_bytes() {
    assert_eq!(common_prefix(&["ørlin", "ørjan"]), Some("ør".into()));
  }

  #[tokio::test]
  async fn ctrl_x_requests_exit_directly() {
    let result = handle(
      Arc::new(RwLock::new(Greeter::default())),
      KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
      Ipc::new(),
    )
    .await;

    assert!(matches!(result, Ok(Some(AuthStatus::Cancel))));
  }

  #[tokio::test]
  async fn ctrl_u() {
    let greeter = Arc::new(RwLock::new(Greeter::default()));

    {
      let mut greeter = greeter.write().await;
      greeter.mode = Mode::Username;
      greeter.username = MaskedString::from("apognu".to_string(), None);
    }

    let result = handle(
      greeter.clone(),
      KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
      Ipc::new(),
    )
    .await;

    {
      let status = greeter.read().await;

      assert!(result.is_ok());
      assert_eq!(status.username.value, "".to_string());
    }

    {
      let mut greeter = greeter.write().await;
      greeter.mode = Mode::Password;
      greeter.buffer = "password".to_string();
    }

    let result = handle(
      greeter.clone(),
      KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
      Ipc::new(),
    )
    .await;

    {
      let status = greeter.read().await;

      assert!(result.is_ok());
      assert_eq!(status.buffer, "".to_string());
    }

    {
      let mut greeter = greeter.write().await;
      greeter.mode = Mode::Command;
      greeter.buffer = "newcommand".to_string();
    }

    let result = handle(
      greeter.clone(),
      KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
      Ipc::new(),
    )
    .await;

    {
      let status = greeter.read().await;

      assert!(result.is_ok());
      assert_eq!(status.buffer, "".to_string());
    }
  }

  #[tokio::test]
  async fn escape() {
    let greeter = Arc::new(RwLock::new(Greeter::default()));

    {
      let mut greeter = greeter.write().await;
      greeter.previous_mode = Mode::Username;
      greeter.mode = Mode::Command;
      greeter.previous_buffer = Some("apognu".to_string());
      greeter.buffer = "newcommand".to_string();
      greeter.cursor_offset = 2;
    }

    let result = handle(
      greeter.clone(),
      KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()),
      Ipc::new(),
    )
    .await;

    {
      let status = greeter.read().await;

      assert!(result.is_ok());
      assert_eq!(status.mode, Mode::Username);
      assert_eq!(status.buffer, "apognu".to_string());
      assert!(status.previous_buffer.is_none());
      assert_eq!(status.cursor_offset, 0);
    }

    for mode in [Mode::Users, Mode::Sessions, Mode::Power] {
      {
        let mut greeter = greeter.write().await;
        greeter.previous_mode = Mode::Username;
        greeter.mode = mode;
      }

      let result = handle(
        greeter.clone(),
        KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()),
        Ipc::new(),
      )
      .await;

      {
        let status = greeter.read().await;

        assert!(result.is_ok());
        assert_eq!(status.mode, Mode::Username);
      }
    }
  }

  #[tokio::test]
  async fn left_right() {
    let greeter = Arc::new(RwLock::new(Greeter::default()));

    let result = handle(
      greeter.clone(),
      KeyEvent::new(KeyCode::Left, KeyModifiers::empty()),
      Ipc::new(),
    )
    .await;

    {
      let status = greeter.read().await;

      assert!(result.is_ok());
      assert_eq!(status.cursor_offset, -1);
    }

    let _ = handle(
      greeter.clone(),
      KeyEvent::new(KeyCode::Right, KeyModifiers::empty()),
      Ipc::new(),
    )
    .await;
    let result = handle(
      greeter.clone(),
      KeyEvent::new(KeyCode::Right, KeyModifiers::empty()),
      Ipc::new(),
    )
    .await;

    {
      let status = greeter.read().await;

      assert!(result.is_ok());
      assert_eq!(status.cursor_offset, 1);
    }
  }

  #[tokio::test]
  async fn f2() {
    let greeter = Arc::new(RwLock::new(Greeter::default()));

    {
      let mut greeter = greeter.write().await;
      greeter.allow_command_editor = true;
      greeter.mode = Mode::Username;
      greeter.buffer = "apognu".to_string();
      greeter.session_source = SessionSource::Command("thecommand".to_string());
    }

    let result = handle(
      greeter.clone(),
      KeyEvent::new(KeyCode::F(2), KeyModifiers::empty()),
      Ipc::new(),
    )
    .await;

    {
      let status = greeter.read().await;

      assert!(result.is_ok());
      assert_eq!(status.mode, Mode::Command);
      assert_eq!(status.previous_buffer, Some("apognu".to_string()));
      assert_eq!(status.buffer, "thecommand".to_string());
    }

    for mode in [Mode::Users, Mode::Sessions, Mode::Power] {
      {
        let mut greeter = greeter.write().await;
        greeter.previous_mode = Mode::Username;
        greeter.mode = mode;
      }

      let result = handle(
        greeter.clone(),
        KeyEvent::new(KeyCode::F(2), KeyModifiers::empty()),
        Ipc::new(),
      )
      .await;

      {
        let status = greeter.read().await;

        assert!(result.is_ok());
        assert_eq!(status.mode, Mode::Command);
        assert_eq!(status.previous_mode, Mode::Username);
      }
    }
  }

  #[tokio::test]
  async fn command_editor_is_disabled_by_default() {
    let greeter = Arc::new(RwLock::new(Greeter::default()));

    let result = handle(
      greeter.clone(),
      KeyEvent::new(KeyCode::F(2), KeyModifiers::empty()),
      Ipc::new(),
    )
    .await;

    assert!(result.is_ok());
    assert_eq!(greeter.read().await.mode, Mode::Username);
  }

  #[tokio::test]
  async fn disabled_command_mode_cannot_replace_the_session_source() {
    let greeter = Arc::new(RwLock::new(Greeter::default()));
    {
      let mut greeter = greeter.write().await;
      greeter.session_source = SessionSource::DefaultCommand("safe-command".into(), None);
      greeter.previous_mode = Mode::Username;
      greeter.previous_buffer = Some("username".into());
      greeter.mode = Mode::Command;
      greeter.buffer = "untrusted-command".into();
    }

    handle(
      greeter.clone(),
      KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
      Ipc::new(),
    )
    .await
    .unwrap();

    let greeter = greeter.read().await;
    assert!(
      matches!(&greeter.session_source, SessionSource::DefaultCommand(command, None) if command == "safe-command")
    );
    assert_eq!(greeter.mode, Mode::Username);
    assert_eq!(greeter.buffer, "username");
  }

  #[tokio::test]
  async fn f_menu() {
    let greeter = Arc::new(RwLock::new(Greeter::default()));

    {
      let mut greeter = greeter.write().await;
      greeter.allow_command_editor = true;
      greeter.sessions.options.push(Session::default());
      greeter.powers.options.push(Power {
        action: PowerOption::Shutdown,
        ..Default::default()
      });
    }

    for (key, mode) in [(KeyCode::F(3), Mode::Sessions), (KeyCode::F(12), Mode::Power)] {
      {
        let mut greeter = greeter.write().await;
        greeter.mode = Mode::Username;
        greeter.buffer = "apognu".to_string();
      }

      let result = handle(greeter.clone(), KeyEvent::new(key, KeyModifiers::empty()), Ipc::new()).await;

      {
        let status = greeter.read().await;

        assert!(result.is_ok());
        assert_eq!(status.mode, mode);
        assert_eq!(status.buffer, "apognu".to_string());
      }

      for mode in [Mode::Users, Mode::Sessions, Mode::Power] {
        {
          let mut greeter = greeter.write().await;
          greeter.previous_mode = Mode::Username;
          greeter.mode = mode;
        }

        let result = handle(
          greeter.clone(),
          KeyEvent::new(KeyCode::F(2), KeyModifiers::empty()),
          Ipc::new(),
        )
        .await;

        {
          let status = greeter.read().await;

          assert!(result.is_ok());
          assert_eq!(status.mode, Mode::Command);
          assert_eq!(status.previous_mode, Mode::Username);
        }
      }
    }
  }

  #[tokio::test]
  async fn f_menu_rebinded() {
    let greeter = Arc::new(RwLock::new(Greeter::default()));

    {
      let mut greeter = greeter.write().await;
      greeter.sessions.options.push(Session::default());
      greeter.powers.options.push(Power {
        action: PowerOption::Shutdown,
        ..Default::default()
      });
    }

    for (key, mode) in [(KeyCode::F(1), Mode::Sessions), (KeyCode::F(11), Mode::Power)] {
      {
        let mut greeter = greeter.write().await;
        greeter.allow_command_editor = true;
        greeter.kb_command = 3;
        greeter.kb_sessions = 1;
        greeter.kb_power = 11;
        greeter.mode = Mode::Username;
        greeter.buffer = "apognu".to_string();
      }

      let result = handle(greeter.clone(), KeyEvent::new(key, KeyModifiers::empty()), Ipc::new()).await;

      {
        let status = greeter.read().await;

        assert!(result.is_ok());
        assert_eq!(status.mode, mode);
        assert_eq!(status.buffer, "apognu".to_string());
      }

      for mode in [Mode::Users, Mode::Sessions, Mode::Power] {
        {
          let mut greeter = greeter.write().await;
          greeter.previous_mode = Mode::Username;
          greeter.mode = mode;
        }

        let result = handle(
          greeter.clone(),
          KeyEvent::new(KeyCode::F(3), KeyModifiers::empty()),
          Ipc::new(),
        )
        .await;

        {
          let status = greeter.read().await;

          assert!(result.is_ok());
          assert_eq!(status.mode, Mode::Command);
          assert_eq!(status.previous_mode, Mode::Username);
        }
      }
    }
  }

  #[tokio::test]
  async fn empty_menu_does_not_open_or_panic() {
    let greeter = Arc::new(RwLock::new(Greeter::default()));

    let result = handle(
      greeter.clone(),
      KeyEvent::new(KeyCode::F(3), KeyModifiers::empty()),
      Ipc::new(),
    )
    .await;
    assert!(result.is_ok());
    assert_eq!(greeter.read().await.mode, Mode::Username);

    greeter.write().await.mode = Mode::Sessions;

    let result = handle(
      greeter.clone(),
      KeyEvent::new(KeyCode::Down, KeyModifiers::empty()),
      Ipc::new(),
    )
    .await;
    assert!(result.is_ok());
    assert_eq!(greeter.read().await.sessions.selected, 0);
  }

  #[tokio::test]
  async fn ctrl_a_e() {
    let greeter = Arc::new(RwLock::new(Greeter::default()));

    {
      let mut greeter = greeter.write().await;
      greeter.mode = Mode::Command;
      greeter.buffer = "123456789".to_string();
    }

    let result = handle(
      greeter.clone(),
      KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
      Ipc::new(),
    )
    .await;

    {
      let status = greeter.read().await;

      assert!(result.is_ok());
      assert_eq!(status.cursor_offset, -9);
    }

    let result = handle(
      greeter.clone(),
      KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
      Ipc::new(),
    )
    .await;

    {
      let status = greeter.read().await;

      assert!(result.is_ok());
      assert_eq!(status.cursor_offset, 0);
    }
  }
}
