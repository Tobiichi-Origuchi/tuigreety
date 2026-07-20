use ansi_to_tui::IntoText;
use ratatui::{
  prelude::Rect,
  text::Text,
  widgets::{Paragraph, Wrap},
};

use crate::{Greeter, Mode, ui::input};

pub fn titleize(message: &str) -> String {
  format!(" {message} ")
}

pub fn buttonize(message: &str) -> String {
  format!(" {message}")
}

// Determinew whether the cursor should be shown or hidden from the current
// mode and configuration. Usually, we will show the cursor only when expecting
// text entries from the user.
pub fn should_hide_cursor(greeter: &Greeter) -> bool {
  !greeter.auth_state.accepts_input()
    || (greeter.user_menu && greeter.mode == Mode::Username && greeter.username.value.is_empty())
    || (greeter.mode == Mode::Password && greeter.prompt.is_none())
    || greeter.mode == Mode::Users
    || greeter.mode == Mode::Sessions
    || greeter.mode == Mode::Power
    || greeter.mode == Mode::Processing
    || greeter.mode == Mode::Action
}

// Computes the height of the main window where we display content, depending on
// the mode and spacing configuration.
//
// +------------------------+
// |                        | <- container padding
// |        Greeting        | <- greeting height
// |                        | <- auto-padding if greeting
// | Username:              | <- username
// | Password:              | <- password if prompt == Some(_)
// |                        | <- container padding
// +------------------------+
pub fn get_height(greeter: &Greeter, content_width: u16) -> u16 {
  let greeting_height = match greeter.mode {
    Mode::Command | Mode::Sessions | Mode::Power | Mode::Processing => 0,
    _ => get_greeting_height(greeter, content_width, 0).1,
  };

  get_height_with_greeting(greeter, greeting_height)
}

pub fn get_height_with_greeting(greeter: &Greeter, greeting_height: u16) -> u16 {
  let container_padding = greeter.container_padding();
  let prompt_padding = greeter.prompt_padding();

  let initial = match greeter.mode {
    Mode::Username | Mode::Action | Mode::Command => container_padding.saturating_mul(2).saturating_add(1),
    Mode::Password => match greeter.prompt {
      Some(_) => container_padding
        .saturating_mul(2)
        .saturating_add(prompt_padding)
        .saturating_add(2),
      None => container_padding.saturating_mul(2).saturating_add(1),
    },
    Mode::Users | Mode::Sessions | Mode::Power | Mode::Processing => container_padding.saturating_mul(2),
  };

  match greeter.mode {
    Mode::Command | Mode::Sessions | Mode::Power | Mode::Processing => initial,
    _ => initial.saturating_add(greeting_height),
  }
}

pub fn get_rect(greeter: &Greeter, area: Rect, items: usize) -> Rect {
  let width = greeter.width().min(area.width);
  let content_width = width.saturating_sub(greeter.container_padding().saturating_mul(2));
  let items = u16::try_from(items).unwrap_or(u16::MAX);
  let height = get_height(greeter, content_width)
    .saturating_add(items)
    .min(area.height);
  let x = area.x.saturating_add(area.width.saturating_sub(width) / 2);
  let y = area.y.saturating_add(area.height.saturating_sub(height) / 2);

  Rect::new(x, y, width, height)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeedbackLayout {
  pub container: Rect,
  pub message: Rect,
  pub message_scroll: u16,
}

/// Center a main container and its feedback as one visual block.
///
/// The main container keeps at least its essential height and then receives
/// space up to its preferred height. When any row remains, feedback retains
/// one newest wrapped line; a visual gap is added only when another row is
/// available. This keeps authentication usable without hiding the diagnostic
/// that explains why it needs attention.
pub fn feedback_layout(
  area: Rect,
  requested_width: u16,
  preferred_container_height: u16,
  minimum_container_height: u16,
  requested_message_height: u16,
) -> FeedbackLayout {
  let width = requested_width.min(area.width);
  let minimum_container_height = minimum_container_height
    .min(preferred_container_height)
    .min(area.height);
  let reserved_message = u16::from(requested_message_height > 0 && area.height > minimum_container_height);
  let reserved_gap =
    u16::from(reserved_message > 0 && area.height > minimum_container_height.saturating_add(reserved_message));
  let maximum_container_height = area
    .height
    .saturating_sub(reserved_message)
    .saturating_sub(reserved_gap);
  let container_height = preferred_container_height
    .min(maximum_container_height)
    .max(minimum_container_height);
  let remaining = area.height.saturating_sub(container_height);
  let gap = u16::from(requested_message_height > 0 && remaining > 1);
  let message_height = requested_message_height.min(remaining.saturating_sub(gap));
  let total_height = container_height.saturating_add(gap).saturating_add(message_height);
  let x = area.x.saturating_add(area.width.saturating_sub(width) / 2);
  let y = area.y.saturating_add(area.height.saturating_sub(total_height) / 2);
  let container = Rect::new(x, y, width, container_height);
  let message = Rect::new(x, container.bottom().saturating_add(gap), width, message_height);

  FeedbackLayout {
    container,
    message,
    message_scroll: requested_message_height.saturating_sub(message_height),
  }
}

pub fn inset(area: Rect, margin: u16) -> Rect {
  let doubled = margin.saturating_mul(2);
  Rect::new(
    area.x.saturating_add(margin.min(area.width)),
    area.y.saturating_add(margin.min(area.height)),
    area.width.saturating_sub(doubled),
    area.height.saturating_sub(doubled),
  )
}

pub fn input_area(area: Rect, label: &str) -> Rect {
  let has_trailing_space = label.chars().last().is_some_and(char::is_whitespace);
  let gap = usize::from(!label.is_empty() && !has_trailing_space);
  let offset = input::width(label).saturating_add(gap).min(usize::from(area.width));
  let offset = u16::try_from(offset).unwrap_or(area.width);

  Rect::new(
    area.x.saturating_add(offset),
    area.y,
    area.width.saturating_sub(offset),
    area.height.min(1),
  )
}

pub fn get_greeting_height(greeter: &Greeter, width: u16, fallback: u16) -> (Option<Paragraph<'_>>, u16) {
  if let Some(greeting) = &greeter.greeting {
    let text = match greeting.to_text() {
      Ok(text) => text,
      Err(_) => Text::raw(greeting),
    };

    let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });
    if width == 0 {
      return (Some(paragraph), 0);
    }
    // Reserve one visual separator below ordinary greeting text. Traditional
    // issue files already end in a blank line (`...\n\n`), so adding another
    // row here would double-space the username prompt.
    let separator = u16::from(!has_trailing_blank_line(greeting));
    let height = paragraph.line_count(width).saturating_add(usize::from(separator));

    (Some(paragraph), u16::try_from(height).unwrap_or(u16::MAX))
  } else {
    (None, fallback)
  }
}

fn has_trailing_blank_line(text: &str) -> bool {
  let Some(before_last_newline) = text.strip_suffix('\n') else {
    return false;
  };
  let before_last_newline = before_last_newline.strip_suffix('\r').unwrap_or(before_last_newline);

  before_last_newline.ends_with('\n')
}

pub fn get_message_height(message: Option<&str>, width: u16) -> (Option<Paragraph<'_>>, u16) {
  if let Some(message) = message {
    let paragraph = Paragraph::new(message.trim_end()).wrap(Wrap { trim: true });
    if width == 0 {
      return (Some(paragraph), 0);
    }
    let height = paragraph.line_count(width);

    (Some(paragraph), u16::try_from(height).unwrap_or(u16::MAX))
  } else {
    (None, 0)
  }
}

#[cfg(test)]
mod test {
  use ratatui::{
    prelude::Rect,
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Paragraph, Wrap},
  };

  use super::{feedback_layout, get_rect, input_area};
  use crate::{
    Greeter,
    Mode,
    ui::util::{get_greeting_height, get_height},
  };

  fn container_height(greeter: &Greeter) -> u16 {
    let content_width = greeter
      .width()
      .saturating_sub(greeter.container_padding().saturating_mul(2));
    get_height(greeter, content_width)
  }

  // +-----------+
  // | Username: |
  // +-----------+
  #[test]
  fn test_container_height_username_padding_zero() {
    let mut greeter = Greeter::default();
    greeter.settings.container_padding = 0;
    greeter.mode = Mode::Username;

    assert_eq!(container_height(&greeter), 3);
  }

  // +-----------+
  // |           |
  // | Username: |
  // |           |
  // +-----------+
  #[test]
  fn test_container_height_username_padding_one() {
    let mut greeter = Greeter::default();
    greeter.settings.container_padding = 1;
    greeter.mode = Mode::Username;

    assert_eq!(container_height(&greeter), 5);
  }

  // +-----------+
  // |           |
  // | Greeting  |
  // |           |
  // | Username: |
  // |           |
  // +-----------+
  #[test]
  fn test_container_height_username_greeting_padding_one() {
    let mut greeter = Greeter::default();
    greeter.settings.container_padding = 1;
    greeter.greeting = Some("Hello".into());
    greeter.mode = Mode::Username;

    assert_eq!(container_height(&greeter), 7);
  }

  // +-----------+
  // |           |
  // | Greeting  |
  // |           |
  // | Username: |
  // |           |
  // | Password: |
  // |           |
  // +-----------+
  #[test]
  fn test_container_height_password_greeting_padding_one_prompt_padding_1() {
    let mut greeter = Greeter::default();
    greeter.settings.container_padding = 1;
    greeter.greeting = Some("Hello".into());
    greeter.mode = Mode::Password;
    greeter.prompt = Some("Password:".into());

    assert_eq!(container_height(&greeter), 9);
  }

  // +-----------+
  // |           |
  // | Greeting  |
  // |           |
  // | Username: |
  // | Password: |
  // |           |
  // +-----------+
  #[test]
  fn test_container_height_password_greeting_padding_one_prompt_padding_0() {
    let mut greeter = Greeter::default();
    greeter.settings.container_padding = 1;
    greeter.settings.prompt_padding = 0;
    greeter.greeting = Some("Hello".into());
    greeter.mode = Mode::Password;
    greeter.prompt = Some("Password:".into());

    assert_eq!(container_height(&greeter), 8);
  }

  #[test]
  fn test_rect_bounds() {
    let mut greeter = Greeter::default();
    greeter.settings.width = 50;

    assert_eq!(
      get_rect(&greeter, Rect::new(0, 0, 100, 100), 1),
      Rect::new(25, 47, 50, 6)
    );
  }

  #[test]
  fn feedback_centers_the_container_and_message_as_one_block_after_clipping_width() {
    let area = Rect::new(10, 20, 40, 20);
    let layout = feedback_layout(area, 80, 5, 3, 3);

    assert_eq!(layout.container, Rect::new(10, 25, 40, 5));
    assert_eq!(layout.message, Rect::new(10, 31, 40, 3));
    assert_eq!(layout.message_scroll, 0);
    assert_eq!(layout.container.y.saturating_sub(area.y), 5);
    assert_eq!(area.bottom().saturating_sub(layout.message.bottom()), 6);
  }

  #[test]
  fn feedback_prioritizes_the_container_and_scrolls_to_the_latest_line() {
    let area = Rect::new(2, 3, 20, 6);
    let layout = feedback_layout(area, 20, 4, 4, 10);

    assert_eq!(layout.container, Rect::new(2, 3, 20, 4));
    assert_eq!(layout.message, Rect::new(2, 8, 20, 1));
    assert_eq!(layout.message_scroll, 9);
  }

  #[test]
  fn feedback_layout_saturates_extreme_geometry_inside_the_available_area() {
    let area = Rect::new(u16::MAX - 5, u16::MAX - 5, 5, 5);
    let layout = feedback_layout(area, u16::MAX, u16::MAX, u16::MAX, u16::MAX);

    assert_eq!(layout.container, area);
    assert_eq!(layout.message, Rect::new(area.x, area.bottom(), area.width, 0));
    assert_eq!(layout.message_scroll, u16::MAX);
  }

  // | Username: __________________________ |
  // <--------------------------------------> width 40 (padding 1)
  //   <-------> prompt width 9
  //             <------------------------> input width 26
  #[test]
  fn input_width() {
    let mut greeter = Greeter::default();
    greeter.settings.width = 40;
    greeter.settings.container_padding = 1;

    let input_width = input_area(Rect::new(2, 0, 36, 1), "Username:").width;

    assert_eq!(input_width, 26);
  }

  #[test]
  fn input_area_uses_terminal_cell_width() {
    let area = input_area(Rect::new(10, 4, 20, 1), "用户：");
    assert_eq!(area, Rect::new(17, 4, 13, 1));
  }

  #[test]
  fn every_u16_layout_value_stays_inside_its_area() {
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
    let mut greeter = Greeter::default();

    for value in 0..=u16::MAX {
      greeter.mode = modes[usize::from(value) % modes.len()];
      greeter.settings.width = value.max(1);
      greeter.settings.container_padding = value.min(u16::MAX - 1);
      greeter.settings.prompt_padding = value;
      let area = Rect::new(0, 0, value, value.rotate_left(8));
      let container = get_rect(&greeter, area, usize::from(value));
      let inner = super::inset(container, value);

      assert!(container.x >= area.x && container.right() <= area.right());
      assert!(container.y >= area.y && container.bottom() <= area.bottom());
      assert!(inner.x >= container.x && inner.right() <= container.right());
      assert!(inner.y >= container.y && inner.bottom() <= container.bottom());
    }
  }

  #[test]
  fn greeting_height_one_line() {
    let mut greeter = Greeter::default();
    greeter.settings.width = 15;
    greeter.settings.container_padding = 1;
    greeter.greeting = Some("Hello World".into());

    let (_, height) = get_greeting_height(&greeter, 13, 0);

    assert_eq!(height, 2);
  }

  #[test]
  fn greeting_height_two_lines() {
    let mut greeter = Greeter::default();
    greeter.settings.width = 8;
    greeter.settings.container_padding = 1;
    greeter.greeting = Some("Hello World".into());

    let (_, height) = get_greeting_height(&greeter, 6, 0);

    assert_eq!(height, 3);
  }

  #[test]
  fn ansi_greeting_height_one_line() {
    let mut greeter = Greeter::default();
    greeter.settings.width = 15;
    greeter.settings.container_padding = 1;
    greeter.greeting = Some("\x1b[31mHello\x1b[0m World".into());

    let (text, height) = get_greeting_height(&greeter, 13, 0);

    let expected = Paragraph::new(Text::from(vec![Line::from(vec![
      Span::styled("Hello", Style::default().fg(Color::Red)),
      Span::styled(" World", Style::reset()),
    ])]))
    .wrap(Wrap { trim: false });

    assert_eq!(text, Some(expected));
    assert_eq!(height, 2);
  }

  #[test]
  fn ansi_greeting_height_two_lines() {
    let mut greeter = Greeter::default();
    greeter.settings.width = 8;
    greeter.settings.container_padding = 1;
    greeter.greeting = Some("\x1b[31mHello\x1b[0m World".into());

    let (text, height) = get_greeting_height(&greeter, 6, 0);

    let expected = Paragraph::new(Text::from(vec![Line::from(vec![
      Span::styled("Hello", Style::default().fg(Color::Red)),
      Span::styled(" World", Style::reset()),
    ])]))
    .wrap(Wrap { trim: false });

    assert_eq!(text, Some(expected));
    assert_eq!(height, 3);
  }

  #[test]
  fn greeting_preserves_whitespace() {
    let mut greeter = Greeter::default();
    greeter.settings.width = 30;
    greeter.settings.container_padding = 1;
    greeter.greeting = Some("  Hello     \nWorld    ".into());

    let (text, height) = get_greeting_height(&greeter, 28, 0);

    let expected =
      Paragraph::new(Text::from(vec![Line::from("  Hello     "), Line::from("World    ")])).wrap(Wrap { trim: false });

    assert_eq!(text, Some(expected));
    assert_eq!(height, 3);
  }

  #[test]
  fn issue_blank_line_is_not_followed_by_an_extra_separator() {
    let mut greeter = Greeter::default();
    greeter.settings.width = 40;
    greeter.settings.container_padding = 1;
    greeter.greeting = Some("CachyOS 7.1.4-1-cachyos (tty1)\n\n".into());

    let (_, height) = get_greeting_height(&greeter, 38, 0);

    assert_eq!(height, 2);
  }

  #[test]
  fn crlf_issue_blank_line_is_not_followed_by_an_extra_separator() {
    let mut greeter = Greeter::default();
    greeter.settings.width = 40;
    greeter.settings.container_padding = 1;
    greeter.greeting = Some("CachyOS\r\n\r\n".into());

    let (_, height) = get_greeting_height(&greeter, 38, 0);

    assert_eq!(height, 2);
  }
}
