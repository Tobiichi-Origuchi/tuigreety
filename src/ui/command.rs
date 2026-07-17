use ratatui::{
  layout::{Alignment, Constraint, Direction, Layout, Rect},
  widgets::{Block, BorderType, Borders, Paragraph},
};

use super::common::style::Themed;
use crate::{
  Greeter,
  ui::{Frame, input, prompt_value, util::*},
};

pub fn draw(greeter: &Greeter, f: &mut Frame, area: Rect) -> Option<(u16, u16)> {
  let theme = &greeter.theme;

  let container = get_rect(greeter, area, 0);

  let container_padding = greeter.container_padding();
  let frame = inset(container, container_padding);

  let block = Block::default()
    .title(titleize(&text!(greeter, title_command)))
    .title_style(theme.of(&[Themed::Title]))
    .style(theme.of(&[Themed::Container]))
    .borders(Borders::ALL)
    .border_type(BorderType::Plain)
    .border_style(theme.of(&[Themed::Border]));

  f.render_widget(block, container);

  let constraints = [
    Constraint::Length(1), // Username
  ];

  let chunks = Layout::default()
    .direction(Direction::Vertical)
    .constraints(constraints.as_ref())
    .split(frame);
  let cursor = chunks[0];

  let command_label_text = prompt_value(theme, Some(text!(greeter, new_command)));
  let command_label = Paragraph::new(command_label_text).style(theme.of(&[Themed::Prompt]));

  f.render_widget(command_label, chunks[0]);
  let label = text!(greeter, new_command);
  let input_area = input_area(cursor, &label);
  if input_area.width == 0 || input_area.height == 0 {
    return None;
  }

  let view = input::view(&greeter.command_buffer, greeter.command_cursor, input_area.width);
  let command_value = Paragraph::new(view.text).style(theme.of(&[Themed::Input]));
  f.render_widget(command_value, input_area);

  if let Some(warning) = greeter.input_warning.as_deref() {
    let (warning, warning_height) = get_message_height(Some(warning), container.width, container_padding, 0);
    if let Some(warning) = warning {
      let y = container.bottom();
      let height = warning_height.min(area.bottom().saturating_sub(y));
      f.render_widget(
        warning.alignment(Alignment::Center),
        Rect::new(container.x, y, container.width, height),
      );
    }
  }

  Some((input_area.x.saturating_add(view.cursor_column), input_area.y))
}
