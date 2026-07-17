mod command;
pub mod common;
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
use util::buttonize;

use self::common::style::{Theme, Themed};
use crate::{Greeter, Mode, info::capslock_status, ui::util::should_hide_cursor};

const TITLEBAR_INDEX: usize = 1;
const STATUSBAR_INDEX: usize = 3;
const STATUSBAR_LEFT_INDEX: usize = 1;
const STATUSBAR_RIGHT_INDEX: usize = 2;

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
  let mut greeter = greeter.write().await;
  let hide_cursor = should_hide_cursor(&greeter);

  terminal.draw(|f| {
    let theme = &greeter.theme;

    let size = f.area();
    let chunks = Layout::default()
      .constraints(
        [
          Constraint::Length(greeter.window_padding()), // Top vertical padding
          Constraint::Length(1),                        // Date and time
          Constraint::Min(1),                           // Main area
          Constraint::Length(1),                        // Status line
          Constraint::Length(greeter.window_padding()), // Bottom vertical padding
        ]
        .as_ref(),
      )
      .split(size);

    if greeter.time {
      let time_text = Span::from(get_time(&greeter));
      let time = Paragraph::new(time_text)
        .alignment(Alignment::Center)
        .style(theme.of(&[Themed::Time]));

      f.render_widget(time, chunks[TITLEBAR_INDEX]);
    }

    let status_block_size_right = 1 + greeter.window_padding() + text!(greeter, status_caps).chars().count() as u16;
    let status_block_size_left = (size.width - greeter.window_padding()) - status_block_size_right;

    let status_chunks = Layout::default()
      .direction(Direction::Horizontal)
      .constraints(
        [
          Constraint::Length(greeter.window_padding()),
          Constraint::Length(status_block_size_left),
          Constraint::Length(status_block_size_right),
          Constraint::Length(greeter.window_padding()),
        ]
        .as_ref(),
      )
      .split(chunks[STATUSBAR_INDEX]);

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

    f.render_widget(status_left, status_chunks[STATUSBAR_LEFT_INDEX]);

    if capslock_status() {
      let status_right_text = status_label(theme, text!(greeter, status_caps));
      let status_right = Paragraph::new(status_right_text).alignment(Alignment::Right);

      f.render_widget(status_right, status_chunks[STATUSBAR_RIGHT_INDEX]);
    }

    let cursor = Some(match greeter.mode {
      Mode::Command => self::command::draw(&mut greeter, f),
      Mode::Sessions => greeter.sessions.draw(&greeter, f),
      Mode::Power => greeter.powers.draw(&greeter, f),
      Mode::Users => greeter.users.draw(&greeter, f),
      Mode::Processing => self::processing::draw(&mut greeter, f),
      _ => self::prompt::draw(&mut greeter, f),
    });

    draw_cursor(f.buffer_mut(), cursor, cursor_on && !hide_cursor);
  })?;

  Ok(())
}

fn draw_cursor(buffer: &mut Buffer, cursor: Option<(u16, u16)>, visible: bool) {
  if !visible {
    return;
  }

  let Some((x, y)) = cursor.and_then(|(x, y)| Some((x.checked_sub(1)?, y.checked_sub(1)?))) else {
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
  use ratatui::layout::Rect;

  use super::*;

  #[test]
  fn software_cursor_draws_blank_and_text_cells() {
    let mut blank = Buffer::empty(Rect::new(0, 0, 2, 1));
    draw_cursor(&mut blank, Some((1, 1)), true);
    assert_eq!(blank[(0, 0)].symbol(), "_");

    let mut text = Buffer::with_lines(["a"]);
    draw_cursor(&mut text, Some((1, 1)), true);
    assert_eq!(text[(0, 0)].symbol(), "a");
    assert!(text[(0, 0)].modifier.contains(Modifier::UNDERLINED));
  }

  #[test]
  fn hidden_software_cursor_does_not_change_buffer() {
    let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
    draw_cursor(&mut buffer, Some((1, 1)), false);

    assert_eq!(buffer[(0, 0)].symbol(), " ");
  }
}
