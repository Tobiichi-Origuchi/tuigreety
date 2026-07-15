use std::{error::Error, io, path::Path};

use ini::Ini;

pub const SYSTEM_TEXT_CONFIG: &str = "/etc/tuigreet/text.conf";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Text {
  pub title_authenticate: String,
  pub title_command: String,
  pub title_power: String,
  pub title_session: String,
  pub title_users: String,
  pub action_reset: String,
  pub action_command: String,
  pub action_session: String,
  pub action_power: String,
  pub date: String,
  pub select_user: String,
  pub username: String,
  pub wait: String,
  pub failed: String,
  pub greetd_error: String,
  pub new_command: String,
  pub shutdown: String,
  pub reboot: String,
  pub suspend: String,
  pub hibernate: String,
  pub command_missing: String,
  pub command_exited: String,
  pub command_failed: String,
  pub status_command: String,
  pub status_session: String,
  pub status_caps: String,
}

impl Default for Text {
  fn default() -> Self {
    Self {
      title_authenticate: "Authenticate into {hostname}".into(),
      title_command: "Change session command".into(),
      title_power: "Power options".into(),
      title_session: "Change session".into(),
      title_users: "Select a user".into(),
      action_reset: "Reset".into(),
      action_command: "Change command".into(),
      action_session: "Choose session".into(),
      action_power: "Power".into(),
      date: "%a, %d %h %Y - %H:%M".into(),
      select_user: "Press Enter to select a user or start typing...".into(),
      username: "Username:".into(),
      wait: "Please wait...".into(),
      failed: "Authentication failed, please try again.".into(),
      greetd_error: "An error was received from greetd".into(),
      new_command: "New command:".into(),
      shutdown: "Shut down".into(),
      reboot: "Reboot".into(),
      suspend: "Suspend".into(),
      hibernate: "Hibernate".into(),
      command_missing: "No command configured".into(),
      command_exited: "Command exited with".into(),
      command_failed: "Command failed".into(),
      status_command: "CMD".into(),
      status_session: "SESS".into(),
      status_caps: "CAPS LOCK".into(),
    }
  }
}

impl Text {
  pub fn authenticate_title(&self, hostname: &str) -> String {
    self.title_authenticate.replace("{hostname}", hostname)
  }

  pub fn load_standard(&mut self) -> Result<(), Box<dyn Error>> {
    self.load_if_present(Path::new(SYSTEM_TEXT_CONFIG))
  }

  pub fn load_file(&mut self, path: &Path) -> Result<(), Box<dyn Error>> {
    let config = Ini::load_from_file(path).map_err(|error| format!("cannot load {}: {error}", path.display()))?;

    for (key, value) in config.general_section().iter() {
      if !self.set(key, value) {
        return Err(format!("unknown text field '{key}' in {}", path.display()).into());
      }
    }

    Ok(())
  }

  fn load_if_present(&mut self, path: &Path) -> Result<(), Box<dyn Error>> {
    match path.try_exists() {
      Ok(true) => self.load_file(path),
      Ok(false) => Ok(()),
      Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
      Err(error) => Err(format!("cannot access {}: {error}", path.display()).into()),
    }
  }

  fn set(&mut self, key: &str, value: &str) -> bool {
    let target = match key {
      "title_authenticate" => &mut self.title_authenticate,
      "title_command" => &mut self.title_command,
      "title_power" => &mut self.title_power,
      "title_session" => &mut self.title_session,
      "title_users" => &mut self.title_users,
      "action_reset" => &mut self.action_reset,
      "action_command" => &mut self.action_command,
      "action_session" => &mut self.action_session,
      "action_power" => &mut self.action_power,
      "date" => &mut self.date,
      "select_user" => &mut self.select_user,
      "username" => &mut self.username,
      "wait" => &mut self.wait,
      "failed" => &mut self.failed,
      "greetd_error" => &mut self.greetd_error,
      "new_command" => &mut self.new_command,
      "shutdown" => &mut self.shutdown,
      "reboot" => &mut self.reboot,
      "suspend" => &mut self.suspend,
      "hibernate" => &mut self.hibernate,
      "command_missing" => &mut self.command_missing,
      "command_exited" => &mut self.command_exited,
      "command_failed" => &mut self.command_failed,
      "status_command" => &mut self.status_command,
      "status_session" => &mut self.status_session,
      "status_caps" => &mut self.status_caps,
      _ => return false,
    };

    value.clone_into(target);
    true
  }
}

#[cfg(test)]
mod tests {
  use std::fs;

  use tempfile::tempdir;

  use super::*;

  #[test]
  fn defaults_are_english() {
    let text = Text::default();

    assert_eq!(text.username, "Username:");
    assert_eq!(text.authenticate_title("host"), "Authenticate into host");
  }

  #[test]
  fn files_override_selected_fields() {
    let directory = tempdir().unwrap();
    let first = directory.path().join("first.conf");
    let second = directory.path().join("second.conf");
    fs::write(&first, "username=User:\nshutdown=Power off\n").unwrap();
    fs::write(&second, "username=Login:\n").unwrap();

    let mut text = Text::default();
    text.load_file(&first).unwrap();
    text.load_file(&second).unwrap();

    assert_eq!(text.username, "Login:");
    assert_eq!(text.shutdown, "Power off");
    assert_eq!(text.reboot, "Reboot");
  }

  #[test]
  fn unknown_fields_are_rejected() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("text.conf");
    fs::write(&path, "typo=ignored?\n").unwrap();

    let error = Text::default().load_file(&path).unwrap_err().to_string();

    assert!(error.contains("unknown text field 'typo'"));
  }

  #[test]
  fn example_contains_valid_defaults() {
    let mut text = Text::default();
    text.load_file(Path::new("contrib/text.conf")).unwrap();

    assert_eq!(text, Text::default());
  }
}
