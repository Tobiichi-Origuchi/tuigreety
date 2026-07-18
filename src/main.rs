#[macro_use]
mod macros;

mod cache;
mod config;
mod desktop_entry;
mod event;
mod greeter;
mod info;
mod ipc;
mod keyboard;
mod logger;
mod power;
mod reload;
mod terminal;
mod text;
mod ui;
mod watcher;

#[cfg(test)]
mod integration;

use std::{env, error::Error, io, process::ExitCode, sync::Arc, time::Duration};

use event::{Control, Event};
use ipc::AuthState;
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::sync::{RwLock, mpsc};

pub use self::greeter::*;
use self::{event::Events, ipc::Ipc};
use crate::{
  terminal::{TerminalSession, TerminationSignals},
  watcher::{ConfigWatcher, WatchOutcome},
};

#[cfg(not(test))]
const CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(500);
// Integration tests explicitly request renders and inspect the next frame. A
// background cursor-only frame would race that protocol; cursor drawing itself
// is covered by focused buffer tests.
#[cfg(test)]
const CURSOR_BLINK_INTERVAL: Duration = Duration::from_secs(60 * 60);

#[tokio::main]
async fn main() -> ExitCode {
  let invocation = CliInvocation::parse(env::args_os());
  invocation.report_warnings();
  if let Some(status) = invocation.handle_information() {
    return status;
  }

  let matches = invocation.matches();
  // Fingerprint before loading. If a config changes anywhere during startup,
  // the watcher observes it relative to this baseline and requests a reload.
  let watcher = ConfigWatcher::spawn(ConfigWatcher::capture(config::watched_paths(&matches)));
  let greeter = Greeter::new(matches).await;
  let backend = CrosstermBackend::new(io::stdout());
  let events = Events::new().await;
  events.set_refresh_rate(greeter.refresh_rate);

  match run(backend, greeter, events, watcher).await {
    Err(error) if matches!(error.downcast_ref::<AuthStatus>(), Some(AuthStatus::Success)) => ExitCode::SUCCESS,
    Ok(()) => ExitCode::SUCCESS,
    Err(_) => ExitCode::FAILURE,
  }
}

async fn run<B>(
  backend: B,
  mut greeter: Greeter,
  mut events: Events,
  mut watcher: ConfigWatcher,
) -> Result<(), Box<dyn Error>>
where
  B: ratatui::backend::Backend,
  B::Error: 'static,
{
  tracing::info!("tuigreet started");

  let ipc = Ipc::new();
  let has_preselected_user = !greeter.username.value.is_empty();
  if has_preselected_user {
    greeter.auth_state = AuthState::CreatingSession;
  }
  let greeter = Arc::new(RwLock::new(greeter));
  let mut reloads = reload::ReloadCoordinator::new()?;
  let mut power_supervisor = power::PowerSupervisor::new();
  let mut power_return_state = None;

  // Register signal listeners before changing terminal modes, then let the
  // guard own every mode change until the Ratatui terminal has been dropped.
  let mut termination_signals = TerminationSignals::new()?;
  let _terminal_session = TerminalSession::enter()?;
  let mut terminal = Terminal::new(backend)?;
  let mut cursor_interval = tokio::time::interval(CURSOR_BLINK_INTERVAL);
  cursor_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
  cursor_interval.tick().await;
  let mut cursor_on = true;

  let (control_tx, mut control_rx) = mpsc::unbounded_channel();
  let mut ipc_actor = tokio::task::spawn({
    let greeter = greeter.clone();
    let ipc = ipc.clone();
    let renders = events.sender();

    async move {
      ipc.run(greeter, control_tx, renders).await;
    }
  });

  if has_preselected_user {
    tracing::info!("creating initial session for preselected user");
    ipc
      .send(greetd_ipc::Request::CreateSession {
        username: greeter.read().await.username.value.clone(),
      })
      .await;
  }

  let mut ipc_actor_finished = false;
  let loop_result: Result<Option<AuthStatus>, String> = async {
    loop {
      if let Some(status) = greeter.read().await.exit {
        tracing::info!("exiting main loop");
        break Ok(Some(status));
      }

      let control = tokio::select! {
        biased;

        Some(control) = control_rx.recv() => Some(control),

        actor = &mut ipc_actor => {
          ipc_actor_finished = true;
          let message = match actor {
            Ok(()) => "greetd IPC actor stopped unexpectedly".to_string(),
            Err(error) => format!("greetd IPC actor failed: {error}"),
          };
          break Err(message);
        },

        signal = termination_signals.recv() => {
          tracing::warn!("received {signal}, shutting down");
          Some(Control::Exit(AuthStatus::Failure))
        },

        outcome = power_supervisor.next(), if power_supervisor.has_active() => {
          let Some(return_state) = power_return_state.take() else {
            break Err("power command completed without a return state".into());
          };
          if finish_power(&mut *greeter.write().await, return_state, outcome) {
            break Ok(None);
          }
          ui::draw(greeter.clone(), &mut terminal, cursor_on)
            .await
            .map_err(|error| error.to_string())?;
          None
        },

        outcome = watcher.next(), if watcher.has_active() => {
          match outcome {
            WatchOutcome::Changed => {
              let (config, snapshot) = {
                let greeter = greeter.read().await;
                (greeter.config_handle(), greeter.reload_snapshot())
              };
              if let Err(error) = reloads.request(config, snapshot) {
                report_reload_failure(&error);
                greeter.write().await.config_notice = Some("Configuration reload is unavailable".into());
                ui::draw(greeter.clone(), &mut terminal, cursor_on)
                  .await
                  .map_err(|error| error.to_string())?;
              }
            },
            WatchOutcome::Stopped(message) => {
              report_watcher_failure(&message);
              greeter.write().await.config_notice = Some("Configuration hot reload is unavailable".into());
              ui::draw(greeter.clone(), &mut terminal, cursor_on)
                .await
                .map_err(|error| error.to_string())?;
            },
          }
          None
        },

        outcome = reloads.next(), if reloads.has_pending() => {
          match outcome {
            reload::ReloadOutcome::Ready { plan, mut warnings } => {
              let mut greeter_state = greeter.write().await;
              let mut applied = greeter_state.apply_reload(*plan);
              warnings.append(&mut applied.warnings);
              greeter_state.config_notice = match warnings.len() {
                0 => None,
                1 => Some("Configuration reloaded with 1 warning".into()),
                count => Some(format!("Configuration reloaded with {count} warnings")),
              };
              let cache_action = applied.cache_action;
              drop(greeter_state);
              report_reload_diagnostics("warning", &warnings);
              if let Some(action) = cache_action {
                let cache_report = execute_cache_reload(&greeter, action).await;
                for warning in &cache_report.warnings {
                  report_cache_warning(warning);
                }
                if let Some(error) = cache_report.failure {
                  report_cache_failure(&error);
                  greeter.write().await.config_notice = Some(cache_report.operation.failure_notice().into());
                } else if !cache_report.warnings.is_empty() {
                  greeter.write().await.config_notice = Some(cache_report.operation.warning_notice().into());
                }
              }
              events.set_refresh_rate(applied.refresh_rate);
              ui::draw(greeter.clone(), &mut terminal, cursor_on)
                .await
                .map_err(|error| error.to_string())?;
            },
            reload::ReloadOutcome::Rejected { warnings } => {
              report_reload_diagnostics("rejected", &warnings);
              greeter.write().await.config_notice =
                Some("Configuration reload rejected; previous settings remain active".into());
              ui::draw(greeter.clone(), &mut terminal, cursor_on)
                .await
                .map_err(|error| error.to_string())?;
            },
            reload::ReloadOutcome::Failed => {
              let message = "configuration reload worker panicked; keeping the previous settings";
              report_reload_failure(message);
              greeter.write().await.config_notice =
                Some("Configuration reload failed; previous settings remain active".into());
              ui::draw(greeter.clone(), &mut terminal, cursor_on)
                .await
                .map_err(|error| error.to_string())?;
            },
            reload::ReloadOutcome::WorkerStopped => {
              let message = "configuration reload worker stopped unexpectedly";
              report_reload_failure(message);
              greeter.write().await.config_notice = Some("Configuration reload is unavailable".into());
              ui::draw(greeter.clone(), &mut terminal, cursor_on)
                .await
                .map_err(|error| error.to_string())?;
            },
          }
          None
        },

        _ = cursor_interval.tick() => {
          cursor_on = !cursor_on;
          ui::draw(greeter.clone(), &mut terminal, cursor_on)
            .await
            .map_err(|error| error.to_string())?;
          None
        },

        event = events.next_result() => match event {
          Ok(Event::Render) => {
            ui::draw(greeter.clone(), &mut terminal, cursor_on)
              .await
              .map_err(|error| error.to_string())?;
            None
          },
          Ok(Event::Key(key)) => {
            keyboard::handle_with_power(greeter.clone(), key, ipc.clone(), power_supervisor.has_active())
              .await
              .map_err(|error| error.to_string())?
          },
          Err(error) => break Err(format!("terminal event worker failed: {error}")),
        },
      };

      match control {
        Some(Control::Exit(status)) => crate::exit(&mut *greeter.write().await, status, &ipc),
        Some(Control::PowerCommand(request)) => {
          if power_supervisor.has_active() {
            tracing::warn!("ignored a second power command while one is already running");
          } else {
            let return_state = begin_power(&mut *greeter.write().await);
            ui::draw(greeter.clone(), &mut terminal, cursor_on)
              .await
              .map_err(|error| error.to_string())?;
            match power_supervisor.start(request) {
              Ok(()) => power_return_state = Some(return_state),
              Err(error) => {
                finish_power(
                  &mut *greeter.write().await,
                  return_state,
                  power::PowerOutcome::Failed(power::PowerFailure::Worker(error.to_string())),
                );
                ui::draw(greeter.clone(), &mut terminal, cursor_on)
                  .await
                  .map_err(|error| error.to_string())?;
              },
            }
          }
        },
        Some(Control::CancelPower) => {
          if !power_supervisor.cancel() {
            tracing::debug!("ignored power cancellation without an active command");
          }
        },
        None => {},
      }
    }
  }
  .await;

  watcher.shutdown().await;
  power_supervisor.shutdown().await;
  ipc.shutdown();
  if !ipc_actor_finished && let Err(error) = ipc_actor.await {
    tracing::error!("greetd IPC actor failed during shutdown: {error}");
  }
  if let Err(error) = events.shutdown().await {
    tracing::error!("terminal event worker failed during shutdown: {error}");
  }
  let exit_status = loop_result.map_err(io::Error::other)?;
  match exit_status {
    Some(status) => Err(status.into()),
    None => Ok(()),
  }
}

struct PowerReturnState {
  mode: Mode,
  message: Option<String>,
}

fn begin_power(greeter: &mut Greeter) -> PowerReturnState {
  let state = PowerReturnState {
    mode: greeter.mode,
    message: greeter.message.take(),
  };
  greeter.mode = Mode::Processing;
  state
}

/// Apply a completed power command. `true` means the application should exit.
fn finish_power(greeter: &mut Greeter, state: PowerReturnState, outcome: power::PowerOutcome) -> bool {
  match outcome {
    power::PowerOutcome::Success => true,
    power::PowerOutcome::Cancelled => {
      greeter.mode = state.mode;
      greeter.message = state.message;
      false
    },
    power::PowerOutcome::Failed(failure) => {
      greeter.mode = state.mode;
      greeter.message = Some(match failure {
        power::PowerFailure::Spawn(error) | power::PowerFailure::Wait(error) | power::PowerFailure::Worker(error) => {
          format!("{}: {error}", greeter.text.command_failed)
        },
        power::PowerFailure::Exit(status) => format!("{} {status}", greeter.text.command_exited),
        power::PowerFailure::Timeout(duration) => format!(
          "{}: timed out after {}",
          greeter.text.command_failed,
          format_power_duration(duration)
        ),
      });
      false
    },
  }
}

fn format_power_duration(duration: Duration) -> String {
  if duration.subsec_nanos() == 0 {
    format!("{} seconds", duration.as_secs())
  } else {
    format!("{duration:?}")
  }
}

fn report_reload_diagnostics(state: &str, diagnostics: &[String]) {
  for diagnostic in diagnostics {
    eprintln!("tuigreet: configuration reload {state}:\n{diagnostic}");
    tracing::warn!("configuration reload {state}: {diagnostic}");
  }
}

fn report_reload_failure(message: &str) {
  eprintln!("tuigreet: error: {message}");
  tracing::error!("{message}");
}

fn report_watcher_failure(message: &str) {
  eprintln!("tuigreet: warning: {message}");
  tracing::warn!("{message}");
}

fn report_cache_failure(message: &str) {
  eprintln!("tuigreet: warning: failed to update remembered state: {message}");
  tracing::warn!("failed to update remembered state: {message}");
}

fn report_cache_warning(message: &str) {
  eprintln!("tuigreet: warning: {message}");
  tracing::warn!("{message}");
}

struct CacheReloadReport {
  warnings: Vec<String>,
  failure: Option<String>,
  operation: CacheReloadOperation,
}

#[derive(Clone, Copy)]
enum CacheReloadOperation {
  Initialize,
  Cleanup,
}

impl CacheReloadOperation {
  fn failure_notice(self) -> &'static str {
    match self {
      Self::Initialize => "Configuration reloaded; remembered-state initialization failed",
      Self::Cleanup => "Configuration reloaded; remembered-state cleanup failed",
    }
  }

  fn warning_notice(self) -> &'static str {
    match self {
      Self::Initialize => "Configuration reloaded with remembered-state warnings",
      Self::Cleanup => "Configuration reloaded; damaged remembered state was repaired",
    }
  }
}

async fn execute_cache_reload(greeter: &Arc<RwLock<Greeter>>, action: CacheReloadAction) -> CacheReloadReport {
  match action {
    CacheReloadAction::Initialize {
      store,
      sessions,
      allow_commands,
      warn_if_missing,
    } => match tokio::task::spawn_blocking(move || store.load(&sessions, allow_commands, warn_if_missing)).await {
      Ok(load) => {
        // CacheStore serializes this read with a concurrent successful-login
        // commit. If the read won the race, the IPC actor has already changed
        // AuthState to Started (which blocks further session input), and queues
        // the exit control immediately after its commit. The briefly older
        // in-memory snapshot therefore cannot start or alter another session.
        greeter.write().await.finish_cache_initialization(load.state);
        CacheReloadReport {
          warnings: load.warnings,
          failure: None,
          operation: CacheReloadOperation::Initialize,
        }
      },
      Err(error) => CacheReloadReport {
        warnings: Vec::new(),
        failure: Some(format!("cache worker failed: {error}")),
        operation: CacheReloadOperation::Initialize,
      },
    },
    CacheReloadAction::PurgeCommands { store } => {
      match tokio::task::spawn_blocking(move || store.purge_commands()).await {
        Ok(Ok(commit)) => {
          greeter.write().await.cache_state = commit.state;
          CacheReloadReport {
            warnings: commit.warnings,
            failure: None,
            operation: CacheReloadOperation::Cleanup,
          }
        },
        Ok(Err(error)) => CacheReloadReport {
          warnings: Vec::new(),
          failure: Some(error.to_string()),
          operation: CacheReloadOperation::Cleanup,
        },
        Err(error) => CacheReloadReport {
          warnings: Vec::new(),
          failure: Some(format!("cache worker failed: {error}")),
          operation: CacheReloadOperation::Cleanup,
        },
      }
    },
  }
}

fn exit(greeter: &mut Greeter, status: AuthStatus, ipc: &Ipc) {
  tracing::info!("preparing exit with status {}", status);

  match status {
    AuthStatus::Success => {},
    AuthStatus::Cancel | AuthStatus::Failure => ipc.cancel(greeter),
  }

  greeter.exit = Some(status);
}

#[cfg(test)]
mod cache_reload_tests {
  use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

  use tempfile::tempdir;

  use super::*;
  use crate::{
    cache::CacheStore,
    ui::sessions::{Session, SessionType},
  };

  #[tokio::test]
  async fn deferred_command_cleanup_initializes_through_legacy_migration() {
    let directory = tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let desktop_path = PathBuf::from("/sessions/reloaded.desktop");
    let session = Session {
      slug: Some("reloaded.desktop".into()),
      name: "Reloaded".into(),
      command: "start-reloaded".into(),
      session_type: SessionType::Wayland,
      path: Some(desktop_path.clone()),
      xdg_desktop_names: None,
    };
    fs::write(directory.path().join("lastuser"), "alice").unwrap();
    fs::write(
      directory.path().join("lastsession-path"),
      desktop_path.to_string_lossy().as_bytes(),
    )
    .unwrap();
    fs::write(directory.path().join("lastsession"), "untrusted command").unwrap();
    let greeter = Arc::new(RwLock::new(Greeter::default()));

    let report = execute_cache_reload(&greeter, CacheReloadAction::Initialize {
      store: CacheStore::at(directory.path()),
      sessions: vec![session.clone()],
      allow_commands: false,
      warn_if_missing: false,
    })
    .await;

    assert!(report.failure.is_none());
    assert!(report.warnings.is_empty());
    let state = greeter.read().await;
    assert_eq!(state.cache_state.last_user().unwrap().username, "alice");
    assert_eq!(
      state.cache_state.global_selection().unwrap().resolve(&[session]),
      Some(0)
    );
    assert!(directory.path().join("state.json").is_file());
    assert!(!directory.path().join("lastuser").exists());
    assert!(!directory.path().join("lastsession-path").exists());
    assert!(!directory.path().join("lastsession").exists());
  }
}

#[cfg(test)]
mod power_state_tests {
  use std::time::Duration;

  use super::*;

  #[test]
  fn power_cancellation_restores_the_exact_mode_and_message() {
    let mut greeter = Greeter::default();
    greeter.mode = Mode::Password;
    greeter.message = Some("existing message".into());

    let state = begin_power(&mut greeter);
    assert_eq!(greeter.mode, Mode::Processing);
    assert!(greeter.message.is_none());
    assert!(!finish_power(&mut greeter, state, power::PowerOutcome::Cancelled));
    assert_eq!(greeter.mode, Mode::Password);
    assert_eq!(greeter.message.as_deref(), Some("existing message"));
  }

  #[test]
  fn power_failures_restore_the_mode_and_replace_the_message() {
    let mut greeter = Greeter::default();
    greeter.mode = Mode::Action;
    greeter.message = Some("old".into());

    let state = begin_power(&mut greeter);
    assert!(!finish_power(
      &mut greeter,
      state,
      power::PowerOutcome::Failed(power::PowerFailure::Timeout(Duration::from_secs(30))),
    ));
    assert_eq!(greeter.mode, Mode::Action);
    assert!(
      greeter
        .message
        .as_deref()
        .unwrap()
        .contains("timed out after 30 seconds")
    );
    assert!(!greeter.message.as_deref().unwrap().contains("old"));
  }

  #[test]
  fn successful_power_commands_request_application_exit() {
    let mut greeter = Greeter::default();
    let state = begin_power(&mut greeter);

    assert!(finish_power(&mut greeter, state, power::PowerOutcome::Success));
  }
}
