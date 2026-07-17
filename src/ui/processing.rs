use ratatui::{
  layout::{Alignment, Constraint, Direction, Layout, Rect},
  text::Span,
  widgets::{Block, BorderType, Borders, Paragraph},
};

use crate::{
  Greeter,
  ui::{Frame, common::style::Themed, util::*},
};

pub fn draw(greeter: &Greeter, f: &mut Frame, area: Rect) -> Option<(u16, u16)> {
  let container = get_rect(greeter, area, 1);
  let container_padding = greeter.container_padding();
  let frame = inset(container, container_padding);

  let block = Block::default()
    .style(greeter.theme.of(&[Themed::Container]))
    .borders(Borders::ALL)
    .border_type(BorderType::Plain)
    .border_style(greeter.theme.of(&[Themed::Border]));

  let constraints = [Constraint::Length(1)];

  let chunks = Layout::default()
    .direction(Direction::Vertical)
    .constraints(constraints.as_ref())
    .split(frame);
  let text = Span::from(text!(greeter, wait));
  let paragraph = Paragraph::new(text).alignment(Alignment::Center);

  f.render_widget(block, container);
  f.render_widget(paragraph, chunks[0]);

  None
}
