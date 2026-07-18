use std::{error::Error, sync::Arc};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use greetd_ipc::Request;
use tokio::sync::RwLock;
use zeroize::Zeroize;

use crate::{
  Greeter,
  Mode,
  event::Control,
  ipc::Ipc,
  power::power,
  ui::{
    common::masked::MaskedString,
    input::{self, COMMAND_LIMIT, RESPONSE_LIMIT, USERNAME_LIMIT},
    sessions::SessionSource,
    users::User,
  },
};

// Act on keyboard events.
//
// This function will be called whenever a keyboard event was captured by the
// application. It takes a reference to the `Greeter` so it can be aware of the
// current state of the application and act accordinly; It also receives the
// `Ipc` interface so it is able to interact with `greetd` if necessary.
#[cfg(test)]
pub async fn handle(
  greeter: Arc<RwLock<Greeter>>,
  input: KeyEvent,
  ipc: Ipc,
) -> Result<Option<Control>, Box<dyn Error>> {
  handle_with_power(greeter, input, ipc, false).await
}

pub async fn handle_with_power(
  greeter: Arc<RwLock<Greeter>>,
  input: KeyEvent,
  ipc: Ipc,
  power_active: bool,
) -> Result<Option<Control>, Box<dyn Error>> {
  // Some terminals report a second event when a key is released. Acting on
  // both events would duplicate text input and actions. Repeats, however, are
  // intentional and should behave like presses.
  if input.kind == KeyEventKind::Release {
    return Ok(None);
  }

  let mut greeter = greeter.write().await;

  // Debug exits and transaction cancellation must remain available while an
  // IPC request or PAM module is taking time to respond.
  #[cfg(debug_assertions)]
  if let KeyEvent {
    code: KeyCode::Char(c),
    modifiers,
    ..
  } = input
    && modifiers.contains(KeyModifiers::CONTROL)
    && c.eq_ignore_ascii_case(&'x')
  {
    return Ok(Some(Control::Exit(crate::AuthStatus::Cancel)));
  }

  // A power command is independent of the greetd/PAM transaction. While it
  // is active, Esc targets only that command and all other ordinary input is
  // ignored. The debug exit above intentionally remains available.
  if power_active {
    return match input {
      KeyEvent { code: KeyCode::Esc, .. } => Ok(Some(Control::CancelPower)),
      _ => Ok(None),
    };
  }

  if let KeyEvent { code: KeyCode::Esc, .. } = input {
    match greeter.mode {
      Mode::Command => greeter.close_command_editor(),
      Mode::Users | Mode::Sessions | Mode::Power => greeter.mode = greeter.previous_mode,
      _ if greeter.auth_state.can_cancel() => ipc.cancel(&mut greeter),
      _ if greeter.auth_state.accepts_input() => greeter.reset(false).await,
      _ => {},
    }
    return Ok(None);
  }

  if !greeter.auth_state.accepts_input() {
    return Ok(None);
  }

  match input {
    // ^U should erase the current buffer.
    KeyEvent {
      code: KeyCode::Char(c),
      modifiers,
      ..
    } if modifiers.contains(KeyModifiers::CONTROL) && c.eq_ignore_ascii_case(&'u') => clear_input(&mut greeter),

    // Simple cursor directions in text fields.
    KeyEvent {
      code: KeyCode::Left, ..
    } => move_cursor(&mut greeter, false),
    KeyEvent {
      code: KeyCode::Right, ..
    } => move_cursor(&mut greeter, true),

    // F2 will display the command entry prompt. If we are already in one of the
    // popup screens, we set the previous screen as being the current previous
    // screen.
    KeyEvent {
      code: KeyCode::F(i), ..
    } if i == greeter.kb_command && greeter.allow_command_editor => {
      greeter.open_command_editor();
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

      if greeter.mode == Mode::Command {
        greeter.close_command_editor();
      }

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

      if greeter.mode == Mode::Command {
        greeter.close_command_editor();
      }

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
      code: KeyCode::Char(c),
      modifiers,
      ..
    } if modifiers.contains(KeyModifiers::CONTROL) && c.eq_ignore_ascii_case(&'a') => {
      move_cursor_to_edge(&mut greeter, false);
    },

    // ^A should go to the end of the current prompt
    KeyEvent {
      code: KeyCode::Char(c),
      modifiers,
      ..
    } if modifiers.contains(KeyModifiers::CONTROL) && c.eq_ignore_ascii_case(&'e') => {
      move_cursor_to_edge(&mut greeter, true);
    },

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

        greeter.mode = Mode::Users;
      },

      Mode::Username => {},

      Mode::Password => {
        greeter.auth_state = crate::ipc::AuthState::ContinuingAuth;
        greeter.message = None;

        ipc
          .send(Request::PostAuthMessageResponse {
            response: Some(greeter.buffer.clone()),
          })
          .await;

        greeter.buffer = String::new();
        greeter.response_cursor = 0;
        greeter.input_warning = None;
      },

      Mode::Command if greeter.allow_command_editor => {
        greeter.sessions.selected = 0;
        greeter.session_source = SessionSource::Command(greeter.command_buffer.clone());

        greeter.close_command_editor();
      },

      Mode::Command => {
        greeter.close_command_editor();
      },

      Mode::Users => {
        let username = greeter.users.options.get(greeter.users.selected).cloned();

        if let Some(User { username, name }) = username {
          greeter.username = MaskedString::from(username, name);
          greeter.username_cursor = greeter.username.value.len();
          greeter.mode = greeter.previous_mode;
          validate_username(&mut greeter, &ipc).await;
        } else {
          greeter.mode = greeter.previous_mode;
        }
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

        let control = power_command.and_then(|command| power(&mut greeter, command.action));

        greeter.mode = greeter.previous_mode;

        if control.is_some() {
          return Ok(control);
        }
      },

      _ => {},
    },

    // Do not handle any other controls keybindings
    KeyEvent { modifiers, .. } if modifiers.contains(KeyModifiers::CONTROL) => {},

    // Handle free-form entry of characters.
    KeyEvent {
      code: KeyCode::Char(c), ..
    } => insert_key(&mut greeter, c),

    // Handle deletion of characters.
    KeyEvent {
      code: KeyCode::Backspace,
      ..
    }
    | KeyEvent {
      code: KeyCode::Delete, ..
    } => delete_key(&mut greeter, input.code),

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

  // An empty prefix should reveal a username only when there is exactly one
  // eligible account. With multiple accounts, require the user to type a
  // distinguishing prefix first.
  if input.is_empty() && matches.len() != 1 {
    return Completion::None;
  }

  let Some(completion) = common_prefix(&matches) else {
    return Completion::None;
  };

  if completion == input {
    return Completion::None;
  }

  greeter.username.value = completion;
  greeter.username.mask = None;
  greeter.username_cursor = greeter.username.value.len();
  greeter.input_warning = None;
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
fn insert_key(greeter: &mut Greeter, character: char) {
  let result = match greeter.mode {
    Mode::Username => {
      let result = insert(
        &mut greeter.username.value,
        &mut greeter.username_cursor,
        character,
        USERNAME_LIMIT,
      );
      if result {
        greeter.username.mask = None;
      }
      result
    },
    Mode::Password => insert(
      &mut greeter.buffer,
      &mut greeter.response_cursor,
      character,
      RESPONSE_LIMIT,
    ),
    Mode::Command => insert(
      &mut greeter.command_buffer,
      &mut greeter.command_cursor,
      character,
      COMMAND_LIMIT,
    ),
    _ => return,
  };

  greeter.input_warning = (!result).then(|| {
    let limit = match greeter.mode {
      Mode::Username => USERNAME_LIMIT,
      Mode::Password => RESPONSE_LIMIT,
      Mode::Command => COMMAND_LIMIT,
      _ => unreachable!(),
    };
    format!("Input limit reached (maximum {limit} bytes)")
  });
}

// Handle deletion of characters from a prompt into the proper buffer, depending
// on the current mode, whether Backspace or Delete was pressed and the position
// of the cursor.
fn delete_key(greeter: &mut Greeter, key: KeyCode) {
  match greeter.mode {
    Mode::Username => {
      delete(&mut greeter.username.value, &mut greeter.username_cursor, key);
      greeter.username.mask = None;
    },
    Mode::Password => delete(&mut greeter.buffer, &mut greeter.response_cursor, key),
    Mode::Command => delete(&mut greeter.command_buffer, &mut greeter.command_cursor, key),
    _ => return,
  }
  greeter.input_warning = None;
}

fn insert(value: &mut String, cursor: &mut usize, character: char, limit: usize) -> bool {
  if value.len().saturating_add(character.len_utf8()) > limit {
    return false;
  }

  *cursor = input::clamp_cursor(value, *cursor);
  value.insert(*cursor, character);
  *cursor = input::cursor_after_insertion(value, (*cursor).saturating_add(character.len_utf8()));
  true
}

fn delete(value: &mut String, cursor: &mut usize, key: KeyCode) {
  *cursor = input::clamp_cursor(value, *cursor);
  let range = match key {
    KeyCode::Backspace => input::previous_cursor(value, *cursor)..*cursor,
    KeyCode::Delete => *cursor..input::next_cursor(value, *cursor),
    _ => return,
  };

  if !range.is_empty() {
    *cursor = range.start;
    value.replace_range(range, "");
  }
}

fn move_cursor(greeter: &mut Greeter, forward: bool) {
  let (value, cursor) = match greeter.mode {
    Mode::Username => (&greeter.username.value, &mut greeter.username_cursor),
    Mode::Password => (&greeter.buffer, &mut greeter.response_cursor),
    Mode::Command => (&greeter.command_buffer, &mut greeter.command_cursor),
    _ => return,
  };

  *cursor = if forward {
    input::next_cursor(value, *cursor)
  } else {
    input::previous_cursor(value, *cursor)
  };
  greeter.input_warning = None;
}

fn move_cursor_to_edge(greeter: &mut Greeter, end: bool) {
  let (value, cursor) = match greeter.mode {
    Mode::Username => (&greeter.username.value, &mut greeter.username_cursor),
    Mode::Password => (&greeter.buffer, &mut greeter.response_cursor),
    Mode::Command => (&greeter.command_buffer, &mut greeter.command_cursor),
    _ => return,
  };

  *cursor = if end { value.len() } else { 0 };
  greeter.input_warning = None;
}

fn clear_input(greeter: &mut Greeter) {
  match greeter.mode {
    Mode::Username => {
      greeter.username.zeroize();
      greeter.username_cursor = 0;
    },
    Mode::Password => {
      greeter.buffer.zeroize();
      greeter.response_cursor = 0;
    },
    Mode::Command => {
      greeter.command_buffer.zeroize();
      greeter.command_cursor = 0;
    },
    _ => return,
  }
  greeter.input_warning = None;
}

// Creates a `greetd` session for the provided username.
async fn validate_username(greeter: &mut Greeter, ipc: &Ipc) {
  greeter.auth_state = crate::ipc::AuthState::CreatingSession;
  greeter.message = None;

  ipc
    .send(Request::CreateSession {
      username: greeter.username.value.clone(),
    })
    .await;
  greeter.buffer = String::new();
  greeter.response_cursor = 0;
  greeter.input_warning = None;

  if greeter.remember_user_session
    && let Some(selection) = greeter.cache_state.user_selection(&greeter.username.value).cloned()
    && greeter.restore_cached_selection(&selection)
  {
    tracing::info!("restored the remembered session for the selected user");
  }
}

#[cfg(test)]
mod test {
  use std::sync::Arc;

  use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
  use tokio::{sync::RwLock, time::Duration};

  use super::{Completion, common_prefix, complete_username, delete, handle, handle_with_power, insert};
  #[cfg(debug_assertions)]
  use crate::{
    AuthStatus,
    event::{Events, fill_event_queue},
  };
  use crate::{
    Greeter,
    Mode,
    event::Control,
    ipc::Ipc,
    power::PowerOption,
    ui::{
      common::masked::MaskedString,
      input::USERNAME_LIMIT,
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
  fn empty_prefix_does_not_reveal_a_shared_prefix() {
    let mut greeter = Greeter::default();
    greeter.users.options = vec![
      User {
        username: "alice".into(),
        name: None,
      },
      User {
        username: "adam".into(),
        name: None,
      },
    ];

    assert_eq!(complete_username(&mut greeter), Completion::None);
    assert!(greeter.username.value.is_empty());
  }

  #[tokio::test]
  async fn release_events_are_ignored_and_repeat_events_are_handled() {
    let greeter = Arc::new(RwLock::new(Greeter::default()));

    handle(
      greeter.clone(),
      KeyEvent::new_with_kind(KeyCode::Char('a'), KeyModifiers::empty(), KeyEventKind::Release),
      Ipc::new(),
    )
    .await
    .unwrap();
    assert!(greeter.read().await.username.value.is_empty());

    handle(
      greeter.clone(),
      KeyEvent::new_with_kind(KeyCode::Char('a'), KeyModifiers::empty(), KeyEventKind::Repeat),
      Ipc::new(),
    )
    .await
    .unwrap();
    assert_eq!(greeter.read().await.username.value, "a");
  }

  #[tokio::test]
  async fn control_bindings_accept_additional_modifiers() {
    let greeter = Arc::new(RwLock::new(Greeter::default()));
    {
      let mut state = greeter.write().await;
      state.username.value = "username".into();
    }

    handle(
      greeter.clone(),
      KeyEvent::new(KeyCode::Char('U'), KeyModifiers::CONTROL | KeyModifiers::SHIFT),
      Ipc::new(),
    )
    .await
    .unwrap();
    assert!(greeter.read().await.username.value.is_empty());

    handle(
      greeter.clone(),
      KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL | KeyModifiers::ALT),
      Ipc::new(),
    )
    .await
    .unwrap();
    assert!(greeter.read().await.username.value.is_empty());
  }

  #[test]
  fn common_prefix_handles_characters_not_bytes() {
    assert_eq!(common_prefix(&["ørlin", "ørjan"]), Some("ør".into()));
  }

  #[tokio::test]
  #[cfg(debug_assertions)]
  async fn ctrl_x_does_not_block_on_a_full_event_queue() {
    let events = Events::testing().await;
    fill_event_queue(&events);
    let mut state = Greeter::default();
    state.auth_state = crate::ipc::AuthState::ContinuingAuth;

    let result = tokio::time::timeout(
      Duration::from_millis(100),
      handle(
        Arc::new(RwLock::new(state)),
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        Ipc::new(),
      ),
    )
    .await
    .expect("Ctrl-X blocked on the full render/event queue");

    assert!(matches!(result, Ok(Some(Control::Exit(AuthStatus::Cancel)))));
  }

  #[tokio::test]
  async fn escape_cancels_a_waiting_authentication_transaction() {
    let mut state = Greeter::default();
    state.mode = Mode::Password;
    state.auth_state = crate::ipc::AuthState::ContinuingAuth;
    let greeter = Arc::new(RwLock::new(state));

    handle(
      greeter.clone(),
      KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()),
      Ipc::new(),
    )
    .await
    .unwrap();

    assert_eq!(greeter.read().await.auth_state, crate::ipc::AuthState::Cancelling);
  }

  #[tokio::test]
  async fn power_processing_keys_do_not_touch_the_greetd_transaction() {
    let mut state = Greeter::default();
    state.mode = Mode::Processing;
    state.auth_state = crate::ipc::AuthState::ContinuingAuth;
    let greeter = Arc::new(RwLock::new(state));

    let control = handle_with_power(
      greeter.clone(),
      KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()),
      Ipc::new(),
      true,
    )
    .await
    .unwrap();
    assert!(matches!(control, Some(Control::CancelPower)));
    assert_eq!(greeter.read().await.auth_state, crate::ipc::AuthState::ContinuingAuth);

    let control = handle_with_power(
      greeter.clone(),
      KeyEvent::new(KeyCode::F(12), KeyModifiers::empty()),
      Ipc::new(),
      true,
    )
    .await
    .unwrap();
    assert!(control.is_none());
    assert_eq!(greeter.read().await.mode, Mode::Processing);
  }

  #[tokio::test]
  async fn processing_escape_still_cancels_greetd_without_a_power_command() {
    let mut state = Greeter::default();
    state.mode = Mode::Processing;
    state.auth_state = crate::ipc::AuthState::ContinuingAuth;
    let greeter = Arc::new(RwLock::new(state));

    let control = handle_with_power(
      greeter.clone(),
      KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()),
      Ipc::new(),
      false,
    )
    .await
    .unwrap();

    assert!(control.is_none());
    assert_eq!(greeter.read().await.auth_state, crate::ipc::AuthState::Cancelling);
  }

  #[tokio::test]
  #[cfg(debug_assertions)]
  async fn debug_exit_remains_available_during_a_power_command() {
    let mut state = Greeter::default();
    state.mode = Mode::Processing;
    state.auth_state = crate::ipc::AuthState::ContinuingAuth;

    let control = handle_with_power(
      Arc::new(RwLock::new(state)),
      KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
      Ipc::new(),
      true,
    )
    .await
    .unwrap();

    assert!(matches!(control, Some(Control::Exit(AuthStatus::Cancel))));
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
      greeter.command_buffer = "newcommand".to_string();
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
      assert_eq!(status.command_buffer, "".to_string());
    }
  }

  #[tokio::test]
  async fn escape() {
    let greeter = Arc::new(RwLock::new(Greeter::default()));

    {
      let mut greeter = greeter.write().await;
      greeter.previous_mode = Mode::Username;
      greeter.mode = Mode::Command;
      greeter.buffer = "password".to_string();
      greeter.command_buffer = "newcommand".to_string();
      greeter.command_cursor = 2;
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
      assert_eq!(status.buffer, "password".to_string());
      assert!(status.command_buffer.is_empty());
      assert_eq!(status.command_cursor, 0);
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
    {
      let mut state = greeter.write().await;
      state.username.value = "a界".into();
      state.username_cursor = state.username.value.len();
    }

    let result = handle(
      greeter.clone(),
      KeyEvent::new(KeyCode::Left, KeyModifiers::empty()),
      Ipc::new(),
    )
    .await;

    {
      let status = greeter.read().await;

      assert!(result.is_ok());
      assert_eq!(status.username_cursor, 1);
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
      assert_eq!(status.username_cursor, status.username.value.len());
    }
  }

  #[test]
  fn insertion_and_deletion_preserve_grapheme_boundaries() {
    let mut value = "a界e\u{301}👩‍💻".to_string();
    let mut cursor = value.len();

    delete(&mut value, &mut cursor, KeyCode::Backspace);
    assert_eq!(value, "a界e\u{301}");
    assert_eq!(cursor, value.len());

    delete(&mut value, &mut cursor, KeyCode::Backspace);
    assert_eq!(value, "a界");
    assert_eq!(cursor, value.len());

    assert!(insert(&mut value, &mut cursor, '\u{301}', USERNAME_LIMIT));
    assert_eq!(cursor, value.len());
    assert_eq!(super::input::previous_cursor(&value, cursor), 1);
  }

  #[tokio::test]
  async fn input_limit_is_bounded_and_reported_without_corrupting_text() {
    let greeter = Arc::new(RwLock::new(Greeter::default()));
    {
      let mut state = greeter.write().await;
      state.username.value = "a".repeat(USERNAME_LIMIT);
      state.username_cursor = state.username.value.len();
    }

    handle(
      greeter.clone(),
      KeyEvent::new(KeyCode::Char('界'), KeyModifiers::empty()),
      Ipc::new(),
    )
    .await
    .unwrap();

    {
      let state = greeter.read().await;
      assert_eq!(state.username.value.len(), USERNAME_LIMIT);
      assert_eq!(
        state.input_warning.as_deref(),
        Some("Input limit reached (maximum 256 bytes)")
      );
    }

    handle(
      greeter.clone(),
      KeyEvent::new(KeyCode::Backspace, KeyModifiers::empty()),
      Ipc::new(),
    )
    .await
    .unwrap();
    let state = greeter.read().await;
    assert_eq!(state.username.value.len(), USERNAME_LIMIT - 1);
    assert!(state.input_warning.is_none());
  }

  #[tokio::test]
  async fn popup_keys_do_not_move_any_text_field_cursor() {
    let greeter = Arc::new(RwLock::new(Greeter::default()));
    {
      let mut state = greeter.write().await;
      state.mode = Mode::Sessions;
      state.username_cursor = 1;
      state.response_cursor = 2;
      state.command_cursor = 3;
    }

    handle(
      greeter.clone(),
      KeyEvent::new(KeyCode::Left, KeyModifiers::empty()),
      Ipc::new(),
    )
    .await
    .unwrap();

    let state = greeter.read().await;
    assert_eq!(state.username_cursor, 1);
    assert_eq!(state.response_cursor, 2);
    assert_eq!(state.command_cursor, 3);
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
      assert_eq!(status.buffer, "apognu".to_string());
      assert_eq!(status.command_buffer, "thecommand".to_string());
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
  async fn nested_menus_cannot_replace_an_authentication_response_with_command_text() {
    let greeter = Arc::new(RwLock::new(Greeter::default()));
    {
      let mut state = greeter.write().await;
      state.allow_command_editor = true;
      state.mode = Mode::Password;
      state.previous_mode = Mode::Password;
      state.buffer = "secret response".into();
      state.session_source = SessionSource::Command("original command".into());
      state.sessions.options.push(Session::default());
    }

    handle(
      greeter.clone(),
      KeyEvent::new(KeyCode::F(2), KeyModifiers::empty()),
      Ipc::new(),
    )
    .await
    .unwrap();
    greeter.write().await.command_buffer = "attacker command".into();

    handle(
      greeter.clone(),
      KeyEvent::new(KeyCode::F(3), KeyModifiers::empty()),
      Ipc::new(),
    )
    .await
    .unwrap();
    handle(
      greeter.clone(),
      KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
      Ipc::new(),
    )
    .await
    .unwrap();

    let state = greeter.read().await;
    assert_eq!(state.mode, Mode::Password);
    assert_eq!(state.buffer, "secret response");
    assert!(state.command_buffer.is_empty());
    assert!(!matches!(&state.session_source, SessionSource::Command(command) if command == "attacker command"));
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
      greeter.mode = Mode::Command;
      greeter.buffer = "password".into();
      greeter.command_buffer = "untrusted-command".into();
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
    assert_eq!(greeter.buffer, "password");
    assert!(greeter.command_buffer.is_empty());
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
  async fn empty_user_menu_cannot_submit_an_empty_username() {
    let greeter = Arc::new(RwLock::new(Greeter::default()));
    {
      let mut state = greeter.write().await;
      state.mode = Mode::Users;
      state.previous_mode = Mode::Username;
      state.user_menu = true;
    }
    let ipc = Ipc::new();

    handle(
      greeter.clone(),
      KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
      ipc.clone(),
    )
    .await
    .unwrap();

    let state = greeter.read().await;
    assert_eq!(state.mode, Mode::Username);
    assert!(state.username.value.is_empty());
    assert_eq!(state.auth_state, crate::ipc::AuthState::Idle);
    drop(state);
    assert!(
      tokio::time::timeout(Duration::from_millis(10), ipc.next())
        .await
        .is_err()
    );
  }

  #[tokio::test]
  async fn ctrl_a_e() {
    let greeter = Arc::new(RwLock::new(Greeter::default()));

    {
      let mut greeter = greeter.write().await;
      greeter.mode = Mode::Command;
      greeter.command_buffer = "123456789".to_string();
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
      assert_eq!(status.command_cursor, 0);
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
      assert_eq!(status.command_cursor, status.command_buffer.len());
    }
  }
}
