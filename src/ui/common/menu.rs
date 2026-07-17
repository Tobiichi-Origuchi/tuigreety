use std::borrow::Cow;

use ratatui::{
  prelude::Rect,
  style::{Modifier, Style},
  text::Span,
  widgets::{Block, BorderType, Borders, Paragraph},
};

use super::style::Themed;
use crate::{
  Greeter,
  ui::{
    Frame,
    util::{get_rect, titleize},
  },
};

pub trait MenuItem {
  fn format(&self) -> Cow<'_, str>;
}

#[derive(Default)]
pub struct Menu<T>
where
  T: MenuItem,
{
  pub title: String,
  pub options: Vec<T>,
  pub selected: usize,
}

impl<T> Menu<T>
where
  T: MenuItem,
{
  pub fn draw(&self, greeter: &Greeter, f: &mut Frame, area: Rect) -> Option<(u16, u16)> {
    let theme = &greeter.theme;

    let container = get_rect(greeter, area, self.options.len());
    let inner = crate::ui::util::inset(container, greeter.container_padding());
    let visible = usize::from(inner.height);
    let selected = self.selected.min(self.options.len().saturating_sub(1));
    let start = selected.saturating_add(1).saturating_sub(visible);

    let title = if self.options.len() > visible && !self.options.is_empty() {
      titleize(&format!(
        "{} [{}/{}]",
        self.title,
        selected.saturating_add(1),
        self.options.len()
      ))
    } else {
      titleize(&self.title)
    };
    let title = Span::from(title);
    let block = Block::default()
      .title(title)
      .title_style(theme.of(&[Themed::Title]))
      .style(theme.of(&[Themed::Container]))
      .borders(Borders::ALL)
      .border_type(BorderType::Plain)
      .border_style(theme.of(&[Themed::Border]));

    f.render_widget(block, container);

    for (row, (index, option)) in self.options.iter().enumerate().skip(start).take(visible).enumerate() {
      let Ok(row) = u16::try_from(row) else {
        break;
      };
      let frame = Rect::new(inner.x, inner.y.saturating_add(row), inner.width, 1);
      let option_text = Self::get_option(option.format(), index, selected);
      let option = Paragraph::new(option_text);

      f.render_widget(option, frame);
    }

    None
  }

  fn get_option<'g, S>(name: S, index: usize, selected: usize) -> Span<'g>
  where
    S: Into<String>,
  {
    if selected == index {
      Span::styled(name.into(), Style::default().add_modifier(Modifier::REVERSED))
    } else {
      Span::from(name.into())
    }
  }
}
