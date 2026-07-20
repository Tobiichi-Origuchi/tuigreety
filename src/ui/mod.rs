mod command;
pub mod common;
pub(crate) mod input;
pub mod power;
mod processing;
mod prompt;
pub mod sessions;
pub mod users;
pub(crate) mod util;

use std::{borrow::Cow, error::Error, sync::Arc};

use chrono::prelude::*;
use ratatui::{
  Frame as CrosstermFrame,
  Terminal,
  buffer::Buffer,
  layout::{Alignment, Constraint, Direction, Layout, Rect},
  style::Modifier,
  text::{Line, Span},
  widgets::Paragraph,
};
use sessions::SessionSource;
use tokio::sync::RwLock;
use util::inset;

use self::common::style::{Theme, Themed};
use crate::{
  Greeter,
  Mode,
  config::{HorizontalPosition, WidgetPosition},
  info::capslock_status,
  ui::util::should_hide_cursor,
};

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
  let sample_capslock = {
    let greeter = greeter.read().await;
    greeter.settings.status_caps_lock && greeter.settings.status_position != WidgetPosition::Hidden
  };
  let capslock = sample_capslock && capslock_status();

  let greeter = greeter.read().await;
  let hide_cursor = should_hide_cursor(&greeter);

  {
    let mut f = terminal.get_frame();
    let theme = &greeter.theme;

    let size = f.area();
    let padded = inset(size, greeter.window_padding());
    let time_top = greeter.time && greeter.settings.time_position == WidgetPosition::Top;
    let time_bottom = greeter.time && greeter.settings.time_position == WidgetPosition::Bottom;
    let info_top = time_top || greeter.settings.battery;
    let status_top = greeter.settings.status_position == WidgetPosition::Top;
    let status_bottom = greeter.settings.status_position == WidgetPosition::Bottom;
    let mut constraints = Vec::with_capacity(5);
    let mut info_slot = None;
    let mut status_slot = None;
    let mut time_bottom_slot = None;

    if info_top {
      info_slot = Some(constraints.len());
      constraints.push(Constraint::Length(1));
    }
    if status_top {
      status_slot = Some(constraints.len());
      constraints.push(Constraint::Length(1));
    }
    let main_slot = constraints.len();
    constraints.push(Constraint::Min(0));
    if status_bottom {
      status_slot = Some(constraints.len());
      constraints.push(Constraint::Length(1));
    }
    if time_bottom {
      time_bottom_slot = Some(constraints.len());
      constraints.push(Constraint::Length(1));
    }
    let chunks = Layout::default().constraints(constraints).split(padded);

    if let Some(slot) = info_slot {
      render_info_row(&greeter, &mut f, chunks[slot], time_top);
    }
    if let Some(slot) = time_bottom_slot {
      let time = Paragraph::new(Span::from(get_time(&greeter)))
        .alignment(Alignment::Center)
        .style(theme.of(&[Themed::Time]));
      f.render_widget(time, chunks[slot]);
    }
    if let Some(slot) = status_slot {
      render_status(&greeter, &mut f, chunks[slot], capslock);
    }

    let cursor = match greeter.mode {
      Mode::Command => self::command::draw(&greeter, &mut f, chunks[main_slot]),
      Mode::Sessions => greeter.sessions.draw(&greeter, &mut f, chunks[main_slot]),
      Mode::Power => greeter.powers.draw(&greeter, &mut f, chunks[main_slot]),
      Mode::Users => greeter.users.draw(&greeter, &mut f, chunks[main_slot]),
      Mode::Processing => self::processing::draw(&greeter, &mut f, chunks[main_slot]),
      _ => self::prompt::draw(&greeter, &mut f, chunks[main_slot]),
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

fn render_info_row(greeter: &Greeter, frame: &mut Frame<'_>, area: Rect, show_time: bool) {
  let theme = &greeter.theme;
  let time = show_time.then(|| get_time(greeter));
  if let Some(time) = &time {
    frame.render_widget(
      Paragraph::new(Span::from(time.as_str()))
        .alignment(Alignment::Center)
        .style(theme.of(&[Themed::Time])),
      area,
    );
  }

  let Some(battery) = greeter.battery_info else {
    return;
  };
  let battery = if battery.charging {
    format!("{}%+", battery.percentage)
  } else {
    format!("{}%", battery.percentage)
  };
  if !info_items_fit(area.width, time.as_deref(), &battery, greeter.settings.battery_position) {
    return;
  }
  let alignment = match greeter.settings.battery_position {
    HorizontalPosition::Left => Alignment::Left,
    HorizontalPosition::Right => Alignment::Right,
  };
  frame.render_widget(
    Paragraph::new(Span::from(battery))
      .alignment(alignment)
      .style(theme.of(&[Themed::Time])),
    area,
  );
}

fn info_items_fit(width: u16, time: Option<&str>, battery: &str, position: HorizontalPosition) -> bool {
  let width = usize::from(width);
  let battery_width = input::width(battery);
  if battery_width > width {
    return false;
  }
  let Some(time) = time else {
    return true;
  };
  let time_width = input::width(time).min(width);
  let time_start = width.saturating_sub(time_width) / 2;
  let time_end = time_start.saturating_add(time_width);
  match position {
    HorizontalPosition::Left => battery_width.saturating_add(1) <= time_start,
    HorizontalPosition::Right => width.saturating_sub(battery_width) >= time_end.saturating_add(1),
  }
}

struct StatusEntry<'a> {
  spans: Vec<Span<'a>>,
  width: usize,
  priority: u8,
}

fn status_entry<'a>(label: Span<'a>, value: Span<'a>, priority: u8) -> StatusEntry<'a> {
  let width = input::width(label.content.as_ref())
    .saturating_add(1)
    .saturating_add(input::width(value.content.as_ref()));
  StatusEntry {
    spans: vec![label, Span::from(" "), value],
    width,
    priority,
  }
}

fn render_status(greeter: &Greeter, frame: &mut Frame<'_>, area: Rect, capslock: bool) {
  let theme = &greeter.theme;
  let caps = (capslock && greeter.settings.status_caps_lock).then(|| {
    let label = greeter.text.status_caps.as_str();
    let width = input::width(label);
    (status_label(theme, label), width)
  });
  let caps = caps.filter(|(_, width)| *width <= usize::from(area.width));
  let right_width = caps.as_ref().map_or(0, |(_, width)| *width);
  let right_gap = usize::from(right_width != 0 && usize::from(area.width) > right_width);
  let left_width = usize::from(area.width).saturating_sub(right_width.saturating_add(right_gap));

  let mut entries = Vec::new();
  if greeter.settings.status_reset {
    entries.push(status_entry(
      status_label(theme, "ESC"),
      status_value(greeter, theme, Button::Other, &greeter.text.action_reset),
      0,
    ));
  }
  if greeter.settings.status_command && greeter.allow_command_editor {
    entries.push(status_entry(
      status_label(theme, greeter.status_command_key.as_str()),
      status_value(greeter, theme, Button::Command, &greeter.text.action_command),
      3,
    ));
  }
  if greeter.settings.status_sessions {
    entries.push(status_entry(
      status_label(theme, greeter.status_sessions_key.as_str()),
      status_value(greeter, theme, Button::Session, &greeter.text.action_session),
      1,
    ));
  }
  if greeter.settings.status_power && !greeter.powers.options.is_empty() {
    entries.push(status_entry(
      status_label(theme, greeter.status_power_key.as_str()),
      status_value(greeter, theme, Button::Power, &greeter.text.action_power),
      2,
    ));
  }
  if greeter.settings.status_selection {
    let label = match greeter.session_source {
      SessionSource::Session(_) => greeter.text.status_session.as_str(),
      _ => greeter.text.status_command.as_str(),
    };
    entries.push(status_entry(
      status_label(theme, label),
      status_value(
        greeter,
        theme,
        Button::Other,
        greeter.session_source.label(greeter).unwrap_or("-"),
      ),
      4,
    ));
  }
  if greeter.settings.status_config
    && let Some(notice) = &greeter.config_notice
  {
    entries.push(status_entry(
      status_label(theme, "CONFIG"),
      status_value(greeter, theme, Button::Other, notice),
      5,
    ));
  }

  let selected = select_status_entries(&entries, left_width);
  let mut spans = Vec::new();
  for (index, entry) in entries.into_iter().enumerate() {
    if !selected[index] {
      continue;
    }
    if !spans.is_empty() {
      spans.push(Span::from(" "));
    }
    spans.extend(entry.spans);
  }

  let right_width = u16::try_from(right_width).unwrap_or(area.width).min(area.width);
  let chunks = Layout::default()
    .direction(Direction::Horizontal)
    .constraints([Constraint::Min(0), Constraint::Length(right_width)])
    .split(area);
  frame.render_widget(Paragraph::new(Line::from(spans)), chunks[0]);
  if let Some((caps, _)) = caps {
    frame.render_widget(Paragraph::new(caps).alignment(Alignment::Right), chunks[1]);
  }
}

fn select_status_entries(entries: &[StatusEntry<'_>], available: usize) -> Vec<bool> {
  let mut order = (0..entries.len()).collect::<Vec<_>>();
  order.sort_by_key(|index| (entries[*index].priority, *index));
  let mut selected = vec![false; entries.len()];
  let mut used = 0_usize;
  for index in order {
    let required = entries[index].width.saturating_add(usize::from(used != 0));
    if used.saturating_add(required) <= available {
      selected[index] = true;
      used = used.saturating_add(required);
    }
  }
  selected
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
  let format = greeter.time_format.as_deref().unwrap_or(&greeter.text.date);

  Local::now().format(format).to_string()
}

fn status_label<'s, S>(theme: &Theme, text: S) -> Span<'s>
where
  S: Into<Cow<'s, str>>,
{
  Span::styled(text, theme.of(&[Themed::ActionButton]).add_modifier(Modifier::REVERSED))
}

fn status_value<'s>(greeter: &Greeter, theme: &Theme, button: Button, text: &'s str) -> Span<'s> {
  let relevant_mode = match button {
    Button::Command => Mode::Command,
    Button::Session => Mode::Sessions,
    Button::Power => Mode::Power,

    _ => {
      return Span::from(text).style(theme.of(&[Themed::Action]));
    },
  };

  let style = match greeter.mode == relevant_mode {
    true => theme.of(&[Themed::ActionButton]).add_modifier(Modifier::REVERSED),
    false => theme.of(&[Themed::Action]),
  };

  Span::from(text).style(style)
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
  use crate::{
    battery::BatteryInfo,
    config::{HorizontalPosition, WidgetPosition},
    ui::{power::Power, sessions::Session, users::User},
  };

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
          greeter.set_greeting(Some("欢迎 e\u{301} 👩‍💻".into()));
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
  async fn terminal_resize_reflows_to_the_current_backend_area() {
    let mut greeter = Greeter::default();
    greeter.username.value = "alice".into();
    greeter.username_cursor = greeter.username.value.len();
    greeter.set_greeting(Some("RESIZE-MARKER".into()));
    let greeter = Arc::new(RwLock::new(greeter));
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();

    for (width, height) in [(120, 30), (32, 8), (160, 50)] {
      terminal.backend_mut().resize(width, height);
      draw(greeter.clone(), &mut terminal, false).await.unwrap();

      let expected = Rect::new(0, 0, width, height);
      assert_eq!(terminal.get_frame().area(), expected);
      assert_eq!(terminal.backend().buffer().area, expected);
      assert!(row_containing(terminal.backend().buffer(), "Username: alice").is_some());
    }
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
  async fn prompt_title_can_be_custom_or_hidden() {
    let greeter = Arc::new(RwLock::new(Greeter::default()));
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();

    {
      let mut state = greeter.write().await;
      state.settings.container_title = crate::config::ContainerTitle::Custom("Custom login".into());
      state.refresh_render_cache();
    }
    draw(greeter.clone(), &mut terminal, false).await.unwrap();
    let rendered = terminal
      .backend()
      .buffer()
      .content
      .iter()
      .map(Cell::symbol)
      .collect::<String>();
    assert!(rendered.contains("Custom login"));

    {
      let mut state = greeter.write().await;
      state.settings.container_title = crate::config::ContainerTitle::Hidden;
      state.refresh_render_cache();
    }
    draw(greeter, &mut terminal, false).await.unwrap();
    let rendered = terminal
      .backend()
      .buffer()
      .content
      .iter()
      .map(Cell::symbol)
      .collect::<String>();
    assert!(!rendered.contains("Custom login"));
    assert!(!rendered.contains("Authenticate into"));
  }

  #[tokio::test]
  async fn issue_with_its_own_blank_line_has_one_row_before_username() {
    let mut greeter = Greeter::default();
    greeter.settings.width = 50;
    greeter.settings.container_padding = 1;
    greeter.set_greeting(Some("CachyOS 7.1.4-1-cachyos (tty1)\n\n".into()));
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
    let mut terminal = Terminal::new(TestBackend::new(24, 7)).unwrap();

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
    let mut terminal = Terminal::new(TestBackend::new(24, 6)).unwrap();

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

  #[tokio::test]
  async fn time_status_and_battery_positions_are_explicit_and_hideable() {
    let mut greeter = Greeter::default();
    greeter.time = true;
    greeter.time_format = Some("CLOCK".into());
    greeter.settings.time_position = WidgetPosition::Top;
    greeter.settings.status_position = WidgetPosition::Top;
    greeter.settings.battery = true;
    greeter.settings.battery_position = HorizontalPosition::Right;
    greeter.battery_info = Some(BatteryInfo {
      percentage: 73,
      charging: true,
    });
    let greeter = Arc::new(RwLock::new(greeter));
    let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();

    draw(greeter.clone(), &mut terminal, false).await.unwrap();
    let buffer = terminal.backend().buffer();
    assert_eq!(row_containing(buffer, "CLOCK"), Some(0));
    assert_eq!(row_containing(buffer, "73%+"), Some(0));
    assert_eq!(row_containing(buffer, "Reset"), Some(1));

    {
      let mut state = greeter.write().await;
      state.settings.time_position = WidgetPosition::Bottom;
      state.settings.status_position = WidgetPosition::Bottom;
      state.settings.battery = false;
      state.battery_info = None;
    }
    draw(greeter.clone(), &mut terminal, false).await.unwrap();
    let buffer = terminal.backend().buffer();
    assert_eq!(row_containing(buffer, "Reset"), Some(10));
    assert_eq!(row_containing(buffer, "CLOCK"), Some(11));
    assert!(row_containing(buffer, "73%+").is_none());

    {
      let mut state = greeter.write().await;
      state.settings.time_position = WidgetPosition::Hidden;
      state.settings.status_position = WidgetPosition::Hidden;
    }
    draw(greeter, &mut terminal, false).await.unwrap();
    let buffer = terminal.backend().buffer();
    assert!(row_containing(buffer, "CLOCK").is_none());
    assert!(row_containing(buffer, "Reset").is_none());
  }

  #[tokio::test]
  async fn status_item_visibility_is_independent() {
    let mut greeter = Greeter::default();
    greeter.settings.status_reset = false;
    greeter.settings.status_command = false;
    greeter.settings.status_power = false;
    greeter.settings.status_selection = false;
    greeter.settings.status_caps_lock = false;
    greeter.settings.status_config = false;
    let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();

    draw(Arc::new(RwLock::new(greeter)), &mut terminal, false)
      .await
      .unwrap();

    let rendered = terminal
      .backend()
      .buffer()
      .content
      .iter()
      .map(Cell::symbol)
      .collect::<String>();
    assert!(rendered.contains("Choose session"));
    assert!(!rendered.contains("Reset"));
    assert!(!rendered.contains("Command"));
  }

  #[test]
  fn narrow_status_selection_keeps_atomic_items_by_priority() {
    let entry = |width, priority| StatusEntry {
      spans: Vec::new(),
      width,
      priority,
    };
    let entries = vec![entry(7, 0), entry(8, 3), entry(6, 1), entry(4, 5)];

    assert_eq!(select_status_entries(&entries, 14), [true, false, true, false]);
    assert_eq!(select_status_entries(&entries, 6), [false, false, true, false]);
    assert_eq!(select_status_entries(&entries, 3), [false, false, false, false]);
  }

  #[test]
  fn battery_is_hidden_instead_of_overlapping_centered_time() {
    assert!(info_items_fit(20, Some("TIME"), "50%", HorizontalPosition::Left));
    assert!(!info_items_fit(10, Some("LONGTIME"), "50%", HorizontalPosition::Left));
    assert!(!info_items_fit(2, None, "50%", HorizontalPosition::Right));
  }
}
