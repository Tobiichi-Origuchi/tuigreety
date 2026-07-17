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
  let greeter = greeter.read().await;
  let hide_cursor = should_hide_cursor(&greeter);

  terminal.draw(|f| {
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
      status_label(theme, format!("F{}", greeter.kb_power)),
      status_value(&greeter, theme, Button::Power, text!(greeter, action_power)),
      Span::from(" "),
      status_label(theme, session_source_label),
      status_value(&greeter, theme, Button::Other, session_source),
    ]);
    let status_left_text = Line::from(status_left_spans);
    let status_left = Paragraph::new(status_left_text);

    f.render_widget(status_left, status_chunks[0]);

    if capslock_status() {
      let status_right_text = status_label(theme, text!(greeter, status_caps));
      let status_right = Paragraph::new(status_right_text).alignment(Alignment::Right);

      f.render_widget(status_right, status_chunks[1]);
    }

    let cursor = match greeter.mode {
      Mode::Command => self::command::draw(&greeter, f, chunks[MAIN_INDEX]),
      Mode::Sessions => greeter.sessions.draw(&greeter, f, chunks[MAIN_INDEX]),
      Mode::Power => greeter.powers.draw(&greeter, f, chunks[MAIN_INDEX]),
      Mode::Users => greeter.users.draw(&greeter, f, chunks[MAIN_INDEX]),
      Mode::Processing => self::processing::draw(&greeter, f, chunks[MAIN_INDEX]),
      _ => self::prompt::draw(&greeter, f, chunks[MAIN_INDEX]),
    };

    draw_cursor(f.buffer_mut(), cursor, cursor_on && !hide_cursor);
  })?;

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

fn prompt_value<'s, S>(theme: &Theme, text: Option<S>) -> Span<'s>
where
  S: Into<String>,
{
  match text {
    Some(text) => Span::styled(text.into(), theme.of(&[Themed::Prompt]).add_modifier(Modifier::BOLD)),
    None => Span::from(""),
  }
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use ratatui::{Terminal, backend::TestBackend, layout::Rect};
  use tokio::sync::RwLock;

  use super::*;
  use crate::ui::{power::Power, sessions::Session, users::User};

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
}
