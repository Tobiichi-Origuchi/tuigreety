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
#[cfg(not(test))]
mod watcher;

#[cfg(test)]
mod integration;

use std::{env, error::Error, io, process, sync::Arc, time::Duration};

use event::{Control, Event};
use ipc::AuthState;
use power::PowerPostAction;
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::sync::{RwLock, mpsc};

pub use self::greeter::*;
use self::{event::Events, ipc::Ipc};
use crate::terminal::{TerminalSession, TerminationSignals};

#[cfg(not(test))]
const CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(500);
// Integration tests explicitly request renders and inspect the next frame. A
// background cursor-only frame would race that protocol; cursor drawing itself
// is covered by focused buffer tests.
#[cfg(test)]
const CURSOR_BLINK_INTERVAL: Duration = Duration::from_secs(60 * 60);

#[tokio::main]
async fn main() {
  let args = arguments_after_program(env::args_os());
  if print_information(&args) {
    return;
  }

  let backend = CrosstermBackend::new(io::stdout());
  let events = Events::new().await;
  let greeter = Greeter::new().await;
  events.set_refresh_rate(greeter.refresh_rate);

  if let Err(error) = run(backend, greeter, events).await {
    if let Some(AuthStatus::Success) = error.downcast_ref::<AuthStatus>() {
      return;
    }

    process::exit(1);
  }
}

fn arguments_after_program<T>(args: impl IntoIterator<Item = T>) -> Vec<T> {
  args.into_iter().skip(1).collect()
}

async fn run<B>(backend: B, mut greeter: Greeter, mut events: Events) -> Result<(), Box<dyn Error>>
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

  #[cfg(not(test))]
  watcher::spawn(config::watched_paths(greeter.read().await.config()), events.sender());

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
              let cache_store = applied.clear_command_cache.then(|| greeter_state.cache_store.clone());
              drop(greeter_state);
              report_reload_diagnostics("warning", &warnings);
              let (cache_warnings, cache_failure) = if let Some(store) = cache_store.filter(|store| store.is_enabled()) {
                match tokio::task::spawn_blocking(move || store.purge_commands()).await {
                  Ok(Ok(commit)) => {
                    greeter.write().await.cache_state = commit.state;
                    (commit.warnings, None)
                  },
                  Ok(Err(error)) => (Vec::new(), Some(error.to_string())),
                  Err(error) => (Vec::new(), Some(format!("cache worker failed: {error}"))),
                }
              } else {
                (Vec::new(), None)
              };
              for warning in &cache_warnings {
                report_cache_warning(warning);
              }
              if let Some(error) = cache_failure {
                report_cache_failure(&error);
                greeter.write().await.config_notice =
                  Some("Configuration reloaded; remembered-state cleanup failed".into());
              } else if !cache_warnings.is_empty() {
                greeter.write().await.config_notice =
                  Some("Configuration reloaded; damaged remembered state was repaired".into());
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

        event = events.next() => match event {
          Some(Event::Render) => {
            ui::draw(greeter.clone(), &mut terminal, cursor_on)
              .await
              .map_err(|error| error.to_string())?;
            None
          },
          Some(Event::Key(key)) => keyboard::handle(greeter.clone(), key, ipc.clone())
            .await
            .map_err(|error| error.to_string())?,
          Some(Event::ReloadConfig) => {
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
            None
          },
          None => None,
        },
      };

      match control {
        Some(Control::Exit(status)) => crate::exit(&mut *greeter.write().await, status, &ipc),
        Some(Control::PowerCommand(command)) => {
          if let PowerPostAction::ClearScreen = power::run(&greeter, *command).await {
            break Ok(None);
          }
        },
        None => {},
      }
    }
  }
  .await;

  ipc.shutdown();
  if !ipc_actor_finished && let Err(error) = ipc_actor.await {
    tracing::error!("greetd IPC actor failed during shutdown: {error}");
  }
  let exit_status = loop_result.map_err(io::Error::other)?;
  match exit_status {
    Some(status) => Err(status.into()),
    None => Ok(()),
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

fn report_cache_failure(message: &str) {
  eprintln!("tuigreet: warning: failed to update remembered state: {message}");
  tracing::warn!("failed to update remembered state: {message}");
}

fn report_cache_warning(message: &str) {
  eprintln!("tuigreet: warning: {message}");
  tracing::warn!("{message}");
}

fn exit(greeter: &mut Greeter, status: AuthStatus, ipc: &Ipc) {
  tracing::info!("preparing exit with status {}", status);

  match status {
    AuthStatus::Success => {},
    AuthStatus::Cancel | AuthStatus::Failure => ipc.cancel(greeter),
  }

  greeter.exit = Some(status);
}
