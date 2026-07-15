use std::{fs, path::Path, process::Stdio, sync::Arc};

use tokio::{process::Command, sync::RwLock};

use crate::{AuthStatus, Greeter, Mode, event::Control, ui::power::Power};

#[derive(SmartDefault, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PowerOption {
  #[default]
  Shutdown,
  Reboot,
  Suspend,
  Hibernate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoginManager {
  Systemd,
  Elogind,
}

fn login_manager(root: &Path) -> Option<LoginManager> {
  // elogind creates this file when its daemon starts and removes it on exit.
  // Confirm the process too, so a stale PID file cannot cause a false match.
  if let Ok(pid) = fs::read_to_string(root.join("run/elogind.pid"))
    && let Ok(comm) = fs::read_to_string(root.join("proc").join(pid.trim()).join("comm"))
    && comm.trim().starts_with("elogind")
  {
    return Some(LoginManager::Elogind);
  }

  // This is the runtime marker used by systemd's sd_booted(3).
  if root.join("run/systemd/system").is_dir() {
    return Some(LoginManager::Systemd);
  }

  None
}

pub fn default_command(option: PowerOption) -> Option<String> {
  default_command_for(option, Path::new("/"))
}

fn default_command_for(option: PowerOption, root: &Path) -> Option<String> {
  match option {
    PowerOption::Shutdown => Some("shutdown -h now".to_string()),
    PowerOption::Reboot => Some("shutdown -r now".to_string()),
    PowerOption::Suspend | PowerOption::Hibernate => {
      let action = match option {
        PowerOption::Suspend => "suspend",
        PowerOption::Hibernate => "hibernate",
        _ => unreachable!(),
      };

      match login_manager(root) {
        Some(LoginManager::Systemd) => Some(format!("systemctl {action}")),
        Some(LoginManager::Elogind) => Some(format!("loginctl {action}")),
        None => None,
      }
    },
  }
}

pub fn power(greeter: &mut Greeter, option: PowerOption) -> Option<Control> {
  if greeter.mock {
    return Some(Control::Exit(AuthStatus::Cancel));
  }

  let command = match greeter.powers.options.iter().find(|opt| opt.action == option) {
    None => None,

    Some(Power {
      command: Some(args), ..
    }) => {
      let command = match greeter.power_setsid {
        true => {
          let mut command = Command::new("setsid");
          command.args(args.split(' '));
          command
        },

        false => {
          let mut args = args.split(' ');

          let mut command = Command::new(args.next().unwrap_or_default());
          command.args(args);
          command
        },
      };

      Some(command)
    },

    Some(_) => None,
  };

  if let Some(mut command) = command {
    command.stdin(Stdio::null());
    command.stdout(Stdio::null());
    command.stderr(Stdio::null());

    Some(Control::PowerCommand(Box::new(command)))
  } else {
    greeter.message = Some(text!(greeter, command_missing));
    None
  }
}

pub enum PowerPostAction {
  Noop,
  ClearScreen,
}

pub async fn run(greeter: &Arc<RwLock<Greeter>>, mut command: Command) -> PowerPostAction {
  tracing::info!("executing configured power command");

  let text = {
    let mut greeter = greeter.write().await;
    greeter.mode = Mode::Processing;
    greeter.text.clone()
  };

  let message = match command.output().await {
    Ok(result) => match (result.status, result.stderr) {
      (status, _) if status.success() => None,
      (status, output) => {
        let status = format!("{} {status}", text.command_exited);
        let output = String::from_utf8(output).unwrap_or_default();

        Some(format!("{status}\n{output}"))
      },
    },

    Err(err) => Some(format!("{}: {err}", text.command_failed)),
  };

  tracing::info!("power command completed with an error: {}", message.is_some());

  let mode = greeter.read().await.previous_mode;

  let mut greeter = greeter.write().await;

  if message.is_none() {
    PowerPostAction::ClearScreen
  } else {
    greeter.mode = mode;
    greeter.message = message;

    PowerPostAction::Noop
  }
}

#[cfg(test)]
mod tests {
  use std::{
    ffi::OsStr,
    fs::{self, File},
    time::Duration,
  };

  use tempfile::tempdir;

  use super::*;
  use crate::event::{Events, fill_event_queue};

  #[test]
  fn detects_elogind_from_its_pid_file() {
    let root = tempdir().unwrap();
    fs::create_dir_all(root.path().join("run/systemd/system")).unwrap();
    fs::create_dir_all(root.path().join("proc/42")).unwrap();
    fs::write(root.path().join("run/elogind.pid"), "42\n").unwrap();
    fs::write(root.path().join("proc/42/comm"), "elogind-daemon\n").unwrap();

    assert_eq!(login_manager(root.path()), Some(LoginManager::Elogind));
    assert_eq!(
      default_command_for(PowerOption::Suspend, root.path()).as_deref(),
      Some("loginctl suspend")
    );
    assert_eq!(
      default_command_for(PowerOption::Hibernate, root.path()).as_deref(),
      Some("loginctl hibernate")
    );
  }

  #[test]
  fn detects_systemd_from_its_runtime_directory() {
    let root = tempdir().unwrap();
    fs::create_dir_all(root.path().join("run/systemd/system")).unwrap();

    assert_eq!(login_manager(root.path()), Some(LoginManager::Systemd));
    assert_eq!(
      default_command_for(PowerOption::Suspend, root.path()).as_deref(),
      Some("systemctl suspend")
    );
    assert_eq!(
      default_command_for(PowerOption::Hibernate, root.path()).as_deref(),
      Some("systemctl hibernate")
    );
  }

  #[test]
  fn does_not_guess_an_unknown_login_manager() {
    let root = tempdir().unwrap();
    fs::create_dir_all(root.path().join("run")).unwrap();

    assert_eq!(login_manager(root.path()), None);
    assert_eq!(default_command_for(PowerOption::Suspend, root.path()), None);
  }

  #[test]
  fn ignores_a_stale_elogind_pid_file() {
    let root = tempdir().unwrap();
    fs::create_dir_all(root.path().join("run")).unwrap();
    File::create(root.path().join("run/elogind.pid")).unwrap();

    assert_eq!(login_manager(root.path()), None);
  }

  #[tokio::test]
  async fn mock_power_does_not_block_on_a_full_event_queue() {
    let events = Events::new().await;
    fill_event_queue(&events);

    let mut greeter = Greeter::default();
    greeter.mock = true;

    let control = tokio::time::timeout(Duration::from_millis(100), async {
      power(&mut greeter, PowerOption::Shutdown)
    })
    .await
    .expect("mock power action blocked on the full render/event queue");

    assert!(matches!(control, Some(Control::Exit(AuthStatus::Cancel))));
  }

  #[tokio::test]
  async fn real_power_does_not_block_on_a_full_event_queue() {
    let events = Events::new().await;
    fill_event_queue(&events);

    let mut greeter = Greeter::default();
    greeter.powers.options.push(Power {
      action: PowerOption::Shutdown,
      label: "Shutdown".into(),
      command: Some("shutdown -h now".into()),
    });

    let control = tokio::time::timeout(Duration::from_millis(100), async {
      power(&mut greeter, PowerOption::Shutdown)
    })
    .await
    .expect("real power action blocked on the full render/event queue");

    let Some(Control::PowerCommand(command)) = control else {
      panic!("power selection did not return its command to the main loop");
    };
    assert_eq!(command.as_std().get_program(), OsStr::new("shutdown"));
    assert_eq!(command.as_std().get_args().collect::<Vec<_>>(), [
      OsStr::new("-h"),
      OsStr::new("now")
    ]);
  }
}
