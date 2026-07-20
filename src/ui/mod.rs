mod command;
pub mod common;
pub(crate) mod input;
pub mod power;
mod processing;
mod prompt;
pub mod sessions;
pub mod users;
mod util;

use std::{borrow::Cow, error::Error, sync::Arc};

use chrono::prelude::*;
use ratatui::{
  Frame as CrosstermFrame,
  Terminal,
  buffer::Buffer,
  layout::{Alignment, Constraint, Direction, Layout},
  style::Modifier,
  text::{Line, Span},
  widgets::Paragraph,
};
use sessions::SessionSource;
use tokio::sync::RwLock;
use util::{buttonize, inset};

use self::common::style::{Theme, Themed};
use crate::{Greeter, Mode, info::capslock_status, ui::util::should_hide_cursor};

const TITLEBAR_INDEX: usize = 0;
const MAIN_INDEX: usize = 1;
const STATUSBAR_INDEX: usize = 2;

pub(super) type Frame<'a> = CrosstermFrame<'a>;

enum Button {
  Command,
  Session,
  Power,
  Other,
}

pub async fn draw<B>(
  greeter: Arc<RwLock<Greeter>>,
  terminal: &mut Terminal<B>,
  cursor_on: bool,
) -> Result<(), Box<dyn Error>>
where
  B: ratatui::backend::Backend,
  B::Error: 'static,
{
  // Backend size discovery can perform terminal I/O. Keep it outside the
  // Greeter lock just like the buffer flush below.
  terminal.autoresize()?;
  let capslock = capslock_status();

  let greeter = greeter.read().await;
  let hide_cursor = should_hide_cursor(&greeter);

  {
    let mut f = terminal.get_frame();
    let theme = &greeter.theme;

    let size = f.area();
    let padded = inset(size, greeter.window_padding());
    let chunks = Layout::default()
      .constraints(
        [
          Constraint::Length(1), // Date and time
          Constraint::Min(0),    // Main area
          Constraint::Length(1), // Status line
        ]
        .as_ref(),
      )
      .split(padded);

    if greeter.time {
      let time_text = Span::from(get_time(&greeter));
      let time = Paragraph::new(time_text)
        .alignment(Alignment::Center)
        .style(theme.of(&[Themed::Time]));

      f.render_widget(time, chunks[TITLEBAR_INDEX]);
    }

    let status_area = chunks[STATUSBAR_INDEX];
    let status_block_size_right = u16::try_from(input::width(&text!(greeter, status_caps)).saturating_add(1))
      .unwrap_or(u16::MAX)
      .min(status_area.width);

    let status_chunks = Layout::default()
      .direction(Direction::Horizontal)
      .constraints([Constraint::Min(0), Constraint::Length(status_block_size_right)])
      .split(status_area);

    let session_source_label = match greeter.session_source {
      SessionSource::Session(_) => text!(greeter, status_session),
      _ => text!(greeter, status_command),
    };

    let session_source = greeter.session_source.label(&greeter).unwrap_or("-");

    let mut status_left_spans = vec![
      status_label(theme, "ESC"),
      status_value(&greeter, theme, Button::Other, text!(greeter, action_reset)),
      Span::from(" "),
    ];
    if greeter.allow_command_editor {
      status_left_spans.extend([
        status_label(theme, format!("F{}", greeter.kb_command)),
        status_value(&greeter, theme, Button::Command, text!(greeter, action_command)),
        Span::from(" "),
      ]);
    }
    status_left_spans.extend([
      status_label(theme, format!("F{}", greeter.kb_sessions)),
      status_value(&greeter, theme, Button::Session, text!(greeter, action_session)),
      Span::from(" "),
    ]);
    if !greeter.powers.options.is_empty() {
      status_left_spans.extend([
        status_label(theme, format!("F{}", greeter.kb_power)),
        status_value(&greeter, theme, Button::Power, text!(greeter, action_power)),
        Span::from(" "),
      ]);
    }
    status_left_spans.extend([
      status_label(theme, session_source_label),
      status_value(&greeter, theme, Button::Other, session_source),
    ]);
    if let Some(notice) = &greeter.config_notice {
      status_left_spans.extend([
        Span::from(" "),
        status_label(theme, "CONFIG"),
        status_value(&greeter, theme, Button::Other, notice),
      ]);
    }
    let status_left_text = Line::from(status_left_spans);
    let status_left = Paragraph::new(status_left_text);

    f.render_widget(status_left, status_chunks[0]);

    if capslock {
      let status_right_text = status_label(theme, text!(greeter, status_caps));
      let status_right = Paragraph::new(status_right_text).alignment(Alignment::Right);

      f.render_widget(status_right, status_chunks[1]);
    }

    let cursor = match greeter.mode {
      Mode::Command => self::command::draw(&greeter, &mut f, chunks[MAIN_INDEX]),
      Mode::Sessions => greeter.sessions.draw(&greeter, &mut f, chunks[MAIN_INDEX]),
      Mode::Power => greeter.powers.draw(&greeter, &mut f, chunks[MAIN_INDEX]),
      Mode::Users => greeter.users.draw(&greeter, &mut f, chunks[MAIN_INDEX]),
      Mode::Processing => self::processing::draw(&greeter, &mut f, chunks[MAIN_INDEX]),
      _ => self::prompt::draw(&greeter, &mut f, chunks[MAIN_INDEX]),
    };

    draw_cursor(f.buffer_mut(), cursor, cursor_on && !hide_cursor);
  }

  // Rendering above only mutates Ratatui's in-memory buffer. Release the
  // shared application state before diffing, writing, moving the hardware
  // cursor, or flushing the terminal backend.
  drop(greeter);
  terminal.apply_buffer()?;

  Ok(())
}

fn draw_cursor(buffer: &mut Buffer, cursor: Option<(u16, u16)>, visible: bool) {
  if !visible {
    return;
  }

  let Some((x, y)) = cursor else {
    return;
  };
  let Some(cell) = buffer.cell_mut((x, y)) else {
    return;
  };

  if cell.symbol().trim().is_empty() {
    cell.set_symbol("_");
  } else {
    cell.modifier.insert(Modifier::UNDERLINED);
  }
}

fn get_time(greeter: &Greeter) -> String {
  let format = match &greeter.time_format {
    Some(format) => Cow::Borrowed(format),
    None => Cow::Owned(text!(greeter, date)),
  };

  Local::now().format(&format).to_string()
}

fn status_label<'s, S>(theme: &Theme, text: S) -> Span<'s>
where
  S: Into<String>,
{
  Span::styled(
    text.into(),
    theme.of(&[Themed::ActionButton]).add_modifier(Modifier::REVERSED),
  )
}

fn status_value<'s, S>(greeter: &Greeter, theme: &Theme, button: Button, text: S) -> Span<'s>
where
  S: Into<String>,
{
  let relevant_mode = match button {
    Button::Command => Mode::Command,
    Button::Session => Mode::Sessions,
    Button::Power => Mode::Power,

    _ => {
      return Span::from(buttonize(&text.into())).style(theme.of(&[Themed::Action]));
    },
  };

  let style = match greeter.mode == relevant_mode {
    true => theme.of(&[Themed::ActionButton]).add_modifier(Modifier::REVERSED),
    false => theme.of(&[Themed::Action]),
  };

  Span::from(buttonize(&text.into())).style(style)
}

fn prompt_value<'s>(theme: &Theme, text: Option<&'s str>) -> Span<'s> {
  match text {
    Some(text) => Span::styled(text, theme.of(&[Themed::Prompt]).add_modifier(Modifier::BOLD)),
    None => Span::from(""),
  }
}

#[cfg(test)]
mod tests {
  use std::{convert::Infallible, sync::Arc};

  use ratatui::{
    Terminal,
    backend::{Backend, ClearType, TestBackend, WindowSize},
    buffer::{Buffer, Cell},
    layout::{Position, Rect, Size},
  };
  use tokio::sync::RwLock;

  use super::*;
  use crate::ui::{power::Power, sessions::Session, users::User};

  struct LockCheckingBackend {
    inner: TestBackend,
    greeter: Arc<RwLock<Greeter>>,
  }

  impl LockCheckingBackend {
    fn assert_unlocked(&self) {
      assert!(
        self.greeter.try_write().is_ok(),
        "terminal backend was accessed while the Greeter lock was held"
      );
    }
  }

  impl Backend for LockCheckingBackend {
    type Error = Infallible;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
      I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
      self.assert_unlocked();
      self.inner.draw(content)
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
      self.assert_unlocked();
      self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
      self.assert_unlocked();
      self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
      self.assert_unlocked();
      self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
      self.assert_unlocked();
      self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
      self.assert_unlocked();
      self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
      self.assert_unlocked();
      self.inner.clear_region(clear_type)
    }

    fn size(&self) -> Result<Size, Self::Error> {
      self.assert_unlocked();
      self.inner.size()
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
      self.assert_unlocked();
      self.inner.window_size()
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
      self.assert_unlocked();
      self.inner.flush()
    }
  }

  fn row_containing(buffer: &Buffer, needle: &str) -> Option<u16> {
    let width = usize::from(buffer.area.width);
    if width == 0 {
      return None;
    }

    buffer.content.chunks(width).enumerate().find_map(|(row, cells)| {
      let text = cells.iter().map(Cell::symbol).collect::<String>();
      text
        .contains(needle)
        .then(|| buffer.area.y.saturating_add(u16::try_from(row).unwrap_or(u16::MAX)))
    })
  }

  #[test]
  fn software_cursor_draws_blank_and_text_cells() {
    let mut blank = Buffer::empty(Rect::new(0, 0, 2, 1));
    draw_cursor(&mut blank, Some((0, 0)), true);
    assert_eq!(blank[(0, 0)].symbol(), "_");

    let mut text = Buffer::with_lines(["a"]);
    draw_cursor(&mut text, Some((0, 0)), true);
    assert_eq!(text[(0, 0)].symbol(), "a");
    assert!(text[(0, 0)].modifier.contains(Modifier::UNDERLINED));
  }

  #[test]
  fn hidden_software_cursor_does_not_change_buffer() {
    let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
    draw_cursor(&mut buffer, Some((0, 0)), false);

    assert_eq!(buffer[(0, 0)].symbol(), " ");
  }

  #[tokio::test]
  async fn every_mode_handles_tiny_areas_and_extreme_layout_values() {
    let sizes = [(0, 0), (1, 1), (3, 3), (10, 3), (80, 24)];
    let layouts = [
      (80, 0, 1, 1),
      (1, 0, 0, 0),
      (u16::MAX, u16::MAX, u16::MAX - 1, u16::MAX),
    ];
    let modes = [
      Mode::Username,
      Mode::Password,
      Mode::Action,
      Mode::Users,
      Mode::Command,
      Mode::Sessions,
      Mode::Power,
      Mode::Processing,
    ];

    for (width, height) in sizes {
      for (prompt_width, window_padding, container_padding, prompt_padding) in layouts {
        for mode in modes {
          let mut greeter = Greeter::default();
          greeter.mode = mode;
          greeter.previous_mode = Mode::Password;
          greeter.settings.width = prompt_width;
          greeter.settings.window_padding = window_padding;
          greeter.settings.container_padding = container_padding;
          greeter.settings.prompt_padding = prompt_padding;
          greeter.time = true;
          greeter.greeting = Some("欢迎 e\u{301} 👩‍💻".into());
          greeter.message = Some("A wrapped message".into());
          greeter.username.value = "用户".into();
          greeter.username_cursor = greeter.username.value.len();
          greeter.buffer = "秘密".into();
          greeter.response_cursor = greeter.buffer.len();
          greeter.command_buffer = "运行 --选项".into();
          greeter.command_cursor = greeter.command_buffer.len();
          greeter.prompt = Some("密码： ".into());
          greeter.users.options = (0..100)
            .map(|index| User {
              username: format!("user-{index}"),
              name: Some(format!("用户 {index}")),
            })
            .collect();
          greeter.users.selected = 99;
          greeter.sessions.options = (0..100)
            .map(|index| Session {
              name: format!("会话 {index}"),
              ..Default::default()
            })
            .collect();
          greeter.sessions.selected = 99;
          greeter.powers.options = vec![Power::default(); 100];
          greeter.powers.selected = 99;

          let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
          draw(Arc::new(RwLock::new(greeter)), &mut terminal, true).await.unwrap();
        }
      }
    }
  }

  #[tokio::test]
  async fn terminal_io_never_holds_the_greeter_lock() {
    let greeter = Arc::new(RwLock::new(Greeter::default()));
    let backend = LockCheckingBackend {
      inner: TestBackend::new(80, 24),
      greeter: greeter.clone(),
    };
    let mut terminal = Terminal::new(backend).unwrap();

    draw(greeter, &mut terminal, true).await.unwrap();
  }

  #[tokio::test]
  async fn prompt_and_feedback_are_centered_as_one_visual_block() {
    let mut greeter = Greeter::default();
    greeter.settings.width = 20;
    greeter.settings.container_padding = 0;
    greeter.username.value = "alice".into();
    greeter.username_cursor = greeter.username.value.len();
    greeter.message = Some("PAM-FEEDBACK".into());
    let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();

    draw(Arc::new(RwLock::new(greeter)), &mut terminal, false)
      .await
      .unwrap();

    let buffer = terminal.backend().buffer();
    let username = row_containing(buffer, "Username: alice").expect("username prompt was not rendered");
    let message = row_containing(buffer, "PAM-FEEDBACK").expect("PAM feedback was not rendered");
    let combined_top = username.saturating_sub(1);
    let combined_bottom = message.saturating_add(1);
    let main_top = 1;
    let main_bottom = buffer.area.height.saturating_sub(1);

    assert!(
      combined_top
        .saturating_sub(main_top)
        .abs_diff(main_bottom.saturating_sub(combined_bottom))
        <= 1,
      "combined prompt and feedback were not vertically centered"
    );
  }

  #[tokio::test]
  async fn issue_with_its_own_blank_line_has_one_row_before_username() {
    let mut greeter = Greeter::default();
    greeter.settings.width = 50;
    greeter.settings.container_padding = 1;
    greeter.greeting = Some("CachyOS 7.1.4-1-cachyos (tty1)\n\n".into());
    let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();

    draw(Arc::new(RwLock::new(greeter)), &mut terminal, false)
      .await
      .unwrap();

    let buffer = terminal.backend().buffer();
    let issue = row_containing(buffer, "CachyOS 7.1.4-1-cachyos (tty1)").expect("issue was not rendered");
    let username = row_containing(buffer, "Username:").expect("username prompt was not rendered");

    assert_eq!(username, issue.saturating_add(2));
  }

  #[tokio::test]
  async fn short_prompt_keeps_authentication_visible_and_shows_the_latest_feedback_line() {
    let mut greeter = Greeter::default();
    greeter.settings.width = 20;
    greeter.settings.container_padding = 0;
    greeter.settings.prompt_padding = 0;
    greeter.mode = Mode::Password;
    greeter.previous_mode = Mode::Password;
    greeter.username.value = "alice".into();
    greeter.prompt = Some("Password:".into());
    greeter.asking_for_secret = true;
    greeter.message = Some("OLD-LINE-ONE\nOLD-LINE-TWO\nLATEST-LINE".into());
    let mut terminal = Terminal::new(TestBackend::new(24, 8)).unwrap();

    draw(Arc::new(RwLock::new(greeter)), &mut terminal, true).await.unwrap();

    let buffer = terminal.backend().buffer();
    assert!(row_containing(buffer, "Username: alice").is_some());
    assert!(row_containing(buffer, "Password:").is_some());
    assert!(row_containing(buffer, "LATEST-LINE").is_some());
    assert!(row_containing(buffer, "OLD-LINE-ONE").is_none());
    assert!(row_containing(buffer, "OLD-LINE-TWO").is_none());
  }

  #[tokio::test]
  async fn short_command_prompt_uses_the_same_latest_warning_layout() {
    let mut greeter = Greeter::default();
    greeter.settings.width = 20;
    greeter.settings.container_padding = 0;
    greeter.mode = Mode::Command;
    greeter.command_buffer = "command".into();
    greeter.command_cursor = greeter.command_buffer.len();
    greeter.input_warning = Some("OLD-WARNING-ONE\nOLD-WARNING-TWO\nLATEST-WARNING".into());
    let mut terminal = Terminal::new(TestBackend::new(24, 7)).unwrap();

    draw(Arc::new(RwLock::new(greeter)), &mut terminal, true).await.unwrap();

    let buffer = terminal.backend().buffer();
    assert!(row_containing(buffer, "New command:").is_some());
    assert!(row_containing(buffer, "LATEST-WARNING").is_some());
    assert!(row_containing(buffer, "OLD-WARNING-ONE").is_none());
    assert!(row_containing(buffer, "OLD-WARNING-TWO").is_none());
  }

  #[tokio::test]
  async fn power_key_hint_is_hidden_when_no_action_is_available() {
    let greeter = Arc::new(RwLock::new(Greeter::default()));
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();

    draw(greeter.clone(), &mut terminal, true).await.unwrap();
    let without_power = terminal
      .backend()
      .buffer()
      .content
      .iter()
      .map(Cell::symbol)
      .collect::<String>();
    assert!(!without_power.contains("Power"));

    greeter.write().await.powers.options.push(Power::default());
    draw(greeter, &mut terminal, true).await.unwrap();
    let with_power = terminal
      .backend()
      .buffer()
      .content
      .iter()
      .map(Cell::symbol)
      .collect::<String>();
    assert!(with_power.contains("Power"));
  }

  #[tokio::test]
  async fn configuration_notice_does_not_replace_pam_feedback() {
    let mut greeter = Greeter::default();
    greeter.message = Some("PAM feedback remains visible".into());
    greeter.config_notice = Some("Reload warning summary".into());
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();

    draw(Arc::new(RwLock::new(greeter)), &mut terminal, true).await.unwrap();

    let rendered = terminal
      .backend()
      .buffer()
      .content
      .iter()
      .map(|cell| cell.symbol())
      .collect::<String>();
    assert!(rendered.contains("PAM feedback remains visible"));
    assert!(rendered.contains("Reload warning summary"));
  }
}
