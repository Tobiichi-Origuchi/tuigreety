use std::borrow::Cow;

use crate::{
  power::{CommandLine, PowerOption},
  ui::common::menu::MenuItem,
};

#[derive(Default, Clone, Debug, Eq, PartialEq)]
pub struct Power {
  pub action: PowerOption,
  pub label: String,
  pub command: Option<CommandLine>,
}

impl MenuItem for Power {
  fn format(&self) -> Cow<'_, str> {
    Cow::Borrowed(&self.label)
  }
}
