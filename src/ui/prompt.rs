use std::{borrow::Cow, sync::LazyLock};

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
  config::ContainerTitle,
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

fn masked_cursor(pool: &str, length: usize) -> usize {
  pool
    .graphemes(true)
    .map(str::len)
    .cycle()
    .take(length)
    .fold(0, usize::saturating_add)
}

pub fn draw(greeter: &Greeter, f: &mut Frame, area: Rect) -> Option<(u16, u16)> {
  let theme = &greeter.theme;
  let container_padding = greeter.container_padding();
  let prompt_padding = greeter.prompt_padding();
  let width = greeter.width().min(area.width);
  let content_width = width.saturating_sub(container_padding.saturating_mul(2));
  let (greeting, greeting_height) = get_greeting_height(greeter, content_width, 0);
  let container_height = get_height_with_greeting(greeter, greeting_height);
  let should_display_answer = greeter.mode == Mode::Password && greeter.prompt.is_some();
  let minimum_container_height = container_padding
    .saturating_mul(2)
    .saturating_add(1)
    .saturating_add(u16::from(should_display_answer));
  let visible_message = greeter.input_warning.as_deref().or(greeter.message.as_deref());
  let (message, message_height) = get_message_height(visible_message, width);
  let feedback = feedback_layout(area, width, container_height, minimum_container_height, message_height);
  let container = feedback.container;
  let greeting_alignment = match greeter.greet_align() {
    GreetAlign::Center => Alignment::Center,
    GreetAlign::Left => Alignment::Left,
    GreetAlign::Right => Alignment::Right,
  };

  let frame = inset(container, container_padding);

  let mut block = Block::default()
    .style(theme.of(&[Themed::Container]))
    .borders(Borders::ALL)
    .border_type(BorderType::Plain)
    .border_style(theme.of(&[Themed::Border]));
  let title = match &greeter.settings.container_title {
    ContainerTitle::Hostname => Some(greeter.text.authenticate_title(&HOSTNAME)),
    ContainerTitle::Custom(title) => Some(title.clone()),
    ContainerTitle::Hidden => None,
  };
  if let Some(title) = title {
    block = block
      .title(Span::from(titleize(&title)))
      .title_style(theme.of(&[Themed::Title]));
  }

  f.render_widget(block, container);

  let available = frame.height;
  let answer_height = u16::from(should_display_answer).min(available);
  let username_height = 1.min(available.saturating_sub(answer_height));
  let remaining = available.saturating_sub(answer_height).saturating_sub(username_height);
  let visible_greeting_height = greeting_height.min(remaining);
  let visible_prompt_padding = if should_display_answer {
    prompt_padding.min(remaining.saturating_sub(visible_greeting_height))
  } else {
    0
  };

  let constraints = [
    Constraint::Length(visible_greeting_height), // Greeting
    Constraint::Length(username_height),         // Username
    Constraint::Length(visible_prompt_padding),  // Prompt padding
    Constraint::Length(answer_height),           // Answer
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
    let prompt_text = Span::from(greeter.text.select_user.as_str());

    Paragraph::new(prompt_text).alignment(Alignment::Center)
  } else {
    let username_text = prompt_value(theme, Some(greeter.text.username.as_str()));

    Paragraph::new(username_text)
  };

  let username = greeter.username.get();
  let mut cursor = None;

  match greeter.mode {
    Mode::Username | Mode::Password | Mode::Action => {
      f.render_widget(username_label, chunks[USERNAME_INDEX]);

      if !greeter.user_menu || !greeter.username.value.is_empty() {
        let label = greeter.text.username.as_str();
        let username_area = input_area(chunks[USERNAME_INDEX], label);
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
        Span::from(greeter.text.wait.as_str())
      } else {
        prompt_value(theme, greeter.prompt.as_deref())
      };

      let answer_label = Paragraph::new(answer_text);

      if greeter.mode == Mode::Password || greeter.previous_mode == Mode::Password {
        f.render_widget(answer_label, chunks[ANSWER_INDEX]);

        if !greeter.asking_for_secret || greeter.secret_display.show() {
          let value = match (greeter.asking_for_secret, &greeter.secret_display) {
            (true, SecretDisplay::Character(pool)) => {
              Cow::Owned(mask_secret(pool, greeter.buffer.graphemes(true).count()))
            },
            _ => Cow::Borrowed(greeter.buffer.as_str()),
          };
          let prompt = greeter.prompt.as_deref().unwrap_or_default();
          let answer_area = input_area(chunks[ANSWER_INDEX], prompt);
          let answer_cursor = match (greeter.asking_for_secret, &greeter.secret_display) {
            (true, SecretDisplay::Character(pool)) => {
              let entered = input::grapheme_count_before(&greeter.buffer, greeter.response_cursor);
              masked_cursor(pool, entered)
            },
            _ => greeter.response_cursor,
          };
          cursor = render_input(
            f,
            answer_area,
            value.as_ref(),
            answer_cursor,
            theme.of(&[Themed::Input]),
          );
        } else {
          let prompt = greeter.prompt.as_deref().unwrap_or_default();
          let answer_area = input_area(chunks[ANSWER_INDEX], prompt);
          cursor = area_cursor(answer_area, 0);
        }
      }

      if let Some(message) = message {
        let message = message
          .alignment(Alignment::Center)
          .scroll((feedback.message_scroll, 0));
        f.render_widget(message, feedback.message);
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
  use super::{mask_secret, masked_cursor};

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

  #[test]
  fn secret_mask_cursor_uses_the_repeated_pool_without_allocating_a_prefix() {
    assert_eq!(masked_cursor("a界", 0), 0);
    assert_eq!(masked_cursor("a界", 1), "a".len());
    assert_eq!(masked_cursor("a界", 2), "a界".len());
    assert_eq!(masked_cursor("a界", 3), "a界a".len());
    assert_eq!(masked_cursor("", 4), 0);
  }
}
