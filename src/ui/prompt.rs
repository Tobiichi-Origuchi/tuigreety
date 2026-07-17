use std::sync::LazyLock;

use ratatui::{
  layout::{Alignment, Constraint, Direction, Layout, Rect},
  style::Style,
  text::Span,
  widgets::{Block, BorderType, Borders, Paragraph},
};
use unicode_segmentation::UnicodeSegmentation;

use super::common::style::Themed;
use crate::{
  GreetAlign,
  Greeter,
  Mode,
  SecretDisplay,
  info::get_hostname,
  ui::{Frame, input, prompt_value, util::*},
};

const GREETING_INDEX: usize = 0;
const USERNAME_INDEX: usize = 1;
const ANSWER_INDEX: usize = 3;

static HOSTNAME: LazyLock<String> = LazyLock::new(get_hostname);

fn mask_secret(pool: &str, length: usize) -> String {
  pool.graphemes(true).cycle().take(length).collect()
}

pub fn draw(greeter: &Greeter, f: &mut Frame, area: Rect) -> Option<(u16, u16)> {
  let theme = &greeter.theme;

  let container = get_rect(greeter, area, 0);

  let container_padding = greeter.container_padding();
  let prompt_padding = greeter.prompt_padding();
  let greeting_alignment = match greeter.greet_align() {
    GreetAlign::Center => Alignment::Center,
    GreetAlign::Left => Alignment::Left,
    GreetAlign::Right => Alignment::Right,
  };

  let frame = inset(container, container_padding);

  let hostname = Span::from(titleize(&greeter.text.authenticate_title(&HOSTNAME)));
  let block = Block::default()
    .title(hostname)
    .title_style(theme.of(&[Themed::Title]))
    .style(theme.of(&[Themed::Container]))
    .borders(Borders::ALL)
    .border_type(BorderType::Plain)
    .border_style(theme.of(&[Themed::Border]));

  f.render_widget(block, container);

  let visible_message = greeter.input_warning.as_deref().or(greeter.message.as_deref());
  let (message, message_height) = get_message_height(visible_message, container.width, container_padding, 1);
  let (greeting, greeting_height) = get_greeting_height(greeter, frame.width, 0);

  let should_display_answer = greeter.mode == Mode::Password;

  let constraints = [
    Constraint::Length(greeting_height), // Greeting
    Constraint::Length(1),               // Username
    Constraint::Length(if should_display_answer { prompt_padding } else { 0 }), // Prompt padding
    Constraint::Length(if should_display_answer { 1 } else { 0 }), // Answer
  ];

  let chunks = Layout::default()
    .direction(Direction::Vertical)
    .constraints(constraints.as_ref())
    .split(frame);
  if let Some(greeting) = greeting {
    let greeting_label = greeting.alignment(greeting_alignment).style(theme.of(&[Themed::Greet]));

    f.render_widget(greeting_label, chunks[GREETING_INDEX]);
  }

  let username_label = if greeter.user_menu && greeter.username.value.is_empty() {
    let prompt_text = Span::from(text!(greeter, select_user));

    Paragraph::new(prompt_text).alignment(Alignment::Center)
  } else {
    let username_text = prompt_value(theme, Some(text!(greeter, username)));

    Paragraph::new(username_text)
  };

  let username = greeter.username.get();
  let mut cursor = None;

  match greeter.mode {
    Mode::Username | Mode::Password | Mode::Action => {
      f.render_widget(username_label, chunks[USERNAME_INDEX]);

      if !greeter.user_menu || !greeter.username.value.is_empty() {
        let label = text!(greeter, username);
        let username_area = input_area(chunks[USERNAME_INDEX], &label);
        let username_cursor = if greeter.mode != Mode::Username {
          0
        } else if greeter.username.mask.is_some() {
          username.len()
        } else {
          greeter.username_cursor
        };
        let rendered_cursor = render_input(f, username_area, username, username_cursor, theme.of(&[Themed::Input]));
        if greeter.mode == Mode::Username {
          cursor = rendered_cursor;
        }
      }

      let answer_text = if greeter.auth_state.is_waiting() {
        Span::from(text!(greeter, wait))
      } else {
        prompt_value(theme, greeter.prompt.as_ref())
      };

      let answer_label = Paragraph::new(answer_text);

      if greeter.mode == Mode::Password || greeter.previous_mode == Mode::Password {
        f.render_widget(answer_label, chunks[ANSWER_INDEX]);

        if !greeter.asking_for_secret || greeter.secret_display.show() {
          let value = match (greeter.asking_for_secret, &greeter.secret_display) {
            (true, SecretDisplay::Character(pool)) => mask_secret(pool, greeter.buffer.graphemes(true).count()),
            _ => greeter.buffer.clone(),
          };
          let prompt = greeter.prompt.as_deref().unwrap_or_default();
          let answer_area = input_area(chunks[ANSWER_INDEX], prompt);
          let answer_cursor = match (greeter.asking_for_secret, &greeter.secret_display) {
            (true, SecretDisplay::Character(pool)) => {
              let entered = input::grapheme_count_before(&greeter.buffer, greeter.response_cursor);
              mask_secret(pool, entered).len()
            },
            _ => greeter.response_cursor,
          };
          cursor = render_input(f, answer_area, &value, answer_cursor, theme.of(&[Themed::Input]));
        } else {
          let prompt = greeter.prompt.as_deref().unwrap_or_default();
          let answer_area = input_area(chunks[ANSWER_INDEX], prompt);
          cursor = area_cursor(answer_area, 0);
        }
      }

      if let Some(message) = message {
        let message = message.alignment(Alignment::Center);
        let y = container.bottom();
        let height = message_height.min(area.bottom().saturating_sub(y));
        f.render_widget(message, Rect::new(container.x, y, container.width, height));
      }
    },

    _ => {},
  }

  cursor
}

fn render_input(f: &mut Frame, area: Rect, value: &str, cursor: usize, style: Style) -> Option<(u16, u16)> {
  if area.width == 0 || area.height == 0 {
    return None;
  }

  let view = input::view(value, cursor, area.width);
  f.render_widget(Paragraph::new(view.text).style(style), area);
  area_cursor(area, view.cursor_column)
}

fn area_cursor(area: Rect, column: u16) -> Option<(u16, u16)> {
  (area.width > 0 && area.height > 0 && column < area.width).then(|| (area.x.saturating_add(column), area.y))
}

#[cfg(test)]
mod tests {
  use super::mask_secret;

  #[test]
  fn secret_mask_cycles_configured_characters() {
    assert_eq!(mask_secret("*", 4), "****");
    assert_eq!(mask_secret("ab", 5), "ababa");
    assert_eq!(mask_secret("●○", 3), "●○●");
  }

  #[test]
  fn empty_secret_mask_is_safe() {
    assert_eq!(mask_secret("", 4), "");
    assert_eq!(mask_secret("*", 0), "");
  }

  #[test]
  fn secret_mask_cycles_extended_graphemes() {
    assert_eq!(mask_secret("👩‍💻界", 3), "👩‍💻界👩‍💻");
  }
}
