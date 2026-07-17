use unicode_segmentation::{GraphemeCursor, UnicodeSegmentation};
use unicode_width::UnicodeWidthStr;

pub(crate) const USERNAME_LIMIT: usize = 256;
pub(crate) const RESPONSE_LIMIT: usize = 4 * 1024;
pub(crate) const COMMAND_LIMIT: usize = 16 * 1024;

pub(crate) fn width(value: &str) -> usize {
  UnicodeWidthStr::width(value)
}

/// Clamp a byte offset to the nearest preceding extended grapheme boundary.
pub(crate) fn clamp_cursor(value: &str, cursor: usize) -> usize {
  let mut cursor = cursor.min(value.len());
  while !value.is_char_boundary(cursor) {
    cursor = cursor.saturating_sub(1);
  }

  let mut grapheme_cursor = GraphemeCursor::new(cursor, value.len(), true);
  match grapheme_cursor.is_boundary(value, 0) {
    Ok(true) => cursor,
    Ok(false) => grapheme_cursor.prev_boundary(value, 0).ok().flatten().unwrap_or(0),
    Err(_) => 0,
  }
}

pub(crate) fn previous_cursor(value: &str, cursor: usize) -> usize {
  let cursor = clamp_cursor(value, cursor);
  GraphemeCursor::new(cursor, value.len(), true)
    .prev_boundary(value, 0)
    .ok()
    .flatten()
    .unwrap_or(0)
}

pub(crate) fn next_cursor(value: &str, cursor: usize) -> usize {
  let cursor = clamp_cursor(value, cursor);
  GraphemeCursor::new(cursor, value.len(), true)
    .next_boundary(value, 0)
    .ok()
    .flatten()
    .unwrap_or(cursor)
}

pub(crate) fn cursor_after_insertion(value: &str, inserted_end: usize) -> usize {
  if inserted_end >= value.len() {
    return value.len();
  }

  let boundary = clamp_cursor(value, inserted_end);
  if boundary == inserted_end {
    boundary
  } else {
    next_cursor(value, boundary)
  }
}

pub(crate) fn grapheme_count_before(value: &str, cursor: usize) -> usize {
  let cursor = clamp_cursor(value, cursor);
  value[..cursor].graphemes(true).count()
}

pub(crate) struct View<'a> {
  pub(crate) text: &'a str,
  pub(crate) cursor_column: u16,
}

/// Select the display-width-bounded slice around the cursor.
///
/// One column is reserved for the software cursor, so the cursor remains
/// visible at both the beginning and end of a full field.
pub(crate) fn view(value: &str, cursor: usize, columns: u16) -> View<'_> {
  if columns == 0 {
    return View {
      text: "",
      cursor_column: 0,
    };
  }

  let cursor = clamp_cursor(value, cursor);
  let cursor_budget = usize::from(columns.saturating_sub(1));
  let mut start = cursor;
  let mut before_width = 0usize;

  for (index, grapheme) in value[..cursor].grapheme_indices(true).rev() {
    let grapheme_width = width(grapheme);
    if before_width.saturating_add(grapheme_width) > cursor_budget {
      break;
    }
    before_width += grapheme_width;
    start = index;
  }

  let mut end = cursor;
  let mut used_width = before_width;
  for (relative_index, grapheme) in value[cursor..].grapheme_indices(true) {
    let grapheme_width = width(grapheme);
    if used_width.saturating_add(grapheme_width) > usize::from(columns) {
      break;
    }
    used_width += grapheme_width;
    end = cursor + relative_index + grapheme.len();
  }

  View {
    text: &value[start..end],
    cursor_column: u16::try_from(before_width).unwrap_or(columns.saturating_sub(1)),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn movement_uses_extended_grapheme_boundaries() {
    let value = "a界e\u{301}👩‍💻";
    let boundaries = [0, 1, 4, 7, value.len()];

    for pair in boundaries.windows(2) {
      assert_eq!(next_cursor(value, pair[0]), pair[1]);
      assert_eq!(previous_cursor(value, pair[1]), pair[0]);
    }
  }

  #[test]
  fn viewport_counts_terminal_columns_and_keeps_cursor_visible() {
    let value = "ab界e\u{301}z";
    let cursor = value.len();
    let view = view(value, cursor, 5);

    assert_eq!(view.text, "界e\u{301}z");
    assert_eq!(view.cursor_column, 4);
  }

  #[test]
  fn zero_width_view_is_empty() {
    let view = view("content", 4, 0);
    assert_eq!(view.text, "");
    assert_eq!(view.cursor_column, 0);
  }

  #[test]
  fn cursors_are_not_limited_by_i16_or_invalid_utf8_offsets() {
    let long = "a".repeat(40_000);
    assert_eq!(previous_cursor(&long, long.len()), long.len() - 1);
    assert_eq!(next_cursor(&long, long.len() - 1), long.len());

    let wide = "a界b";
    assert_eq!(clamp_cursor(wide, 3), 1);
  }

  #[test]
  fn display_width_matches_terminal_cells() {
    assert_eq!(width("界"), 2);
    assert_eq!(width("e\u{301}"), 1);
    assert_eq!(width("👩‍💻"), 2);
  }
}
