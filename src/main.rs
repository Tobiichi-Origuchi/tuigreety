#[macro_use]
extern crate smart_default;

#[macro_use]
mod macros;

mod config;
mod event;
mod greeter;
mod info;
mod ipc;
mod keyboard;
mod power;
mod text;
mod ui;

#[cfg(test)]
mod integration;

use std::{env, error::Error, fs::OpenOptions, io, process, sync::Arc, time::Duration};

#[cfg(not(test))]
use crossterm::{
  cursor::Hide,
  terminal::{Clear, ClearType, EnterAlternateScreen, enable_raw_mode},
};
use crossterm::{
  cursor::Show,
  execute,
  terminal::{LeaveAlternateScreen, disable_raw_mode},
};
use event::Event;
use greetd_ipc::Request;
use power::PowerPostAction;
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::sync::RwLock;
use tracing_appender::non_blocking::WorkerGuard;

pub use self::greeter::*;
use self::{event::Events, ipc::Ipc};

#[cfg(not(test))]
const CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(500);
// Integration tests explicitly request renders and inspect the next frame. A
// background cursor-only frame would race that protocol; cursor drawing itself
// is covered by focused buffer tests.
#[cfg(test)]
const CURSOR_BLINK_INTERVAL: Duration = Duration::from_secs(60 * 60);

#[tokio::main]
async fn main() {
  let args = env::args().collect::<Vec<_>>();
  if print_information(&args) {
    return;
  }

  let backend = CrosstermBackend::new(io::stdout());
  let events = Events::new().await;
  let greeter = Greeter::new(events.sender()).await;
  events.set_refresh_rate(greeter.refresh_rate);

  if let Err(error) = run(backend, greeter, events).await {
    if let Some(AuthStatus::Success) = error.downcast_ref::<AuthStatus>() {
      return;
    }

    process::exit(1);
  }
}

async fn run<B>(backend: B, mut greeter: Greeter, mut events: Events) -> Result<(), Box<dyn Error>>
where
  B: ratatui::backend::Backend,
  B::Error: 'static,
{
  tracing::info!("tuigreet started");

  register_panic_handler();

  let ipc = Ipc::new();
  let has_preselected_user = !greeter.username.value.is_empty();
  if has_preselected_user {
    greeter.working = true;
  }
  let greeter = Arc::new(RwLock::new(greeter));

  if has_preselected_user {
    tracing::info!("creating initial session for preselected user");

    ipc
      .send(Request::CreateSession {
        username: greeter.read().await.username.value.clone(),
      })
      .await;

    // Resolve the first real greetd prompt before entering the alternate
    // screen. This keeps the first visible frame at its final height without
    // guessing whether PAM will ask for a password, MFA token, or other input.
    let mut initial_ipc = ipc.clone();
    initial_ipc.handle(greeter.clone()).await?;
  }

  #[cfg(not(test))]
  {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, Clear(ClearType::All), Hide)?;
  }

  let mut terminal = Terminal::new(backend)?;
  let mut cursor_interval = tokio::time::interval(CURSOR_BLINK_INTERVAL);
  cursor_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
  cursor_interval.tick().await;
  let mut cursor_on = true;

  tokio::task::spawn({
    let greeter = greeter.clone();
    let mut ipc = ipc.clone();

    async move {
      loop {
        let _ = ipc.handle(greeter.clone()).await;
      }
    }
  });

  loop {
    if let Some(status) = greeter.read().await.exit {
      tracing::info!("exiting main loop");

      return Err(status.into());
    }

    tokio::select! {
      event = events.next() => match event {
      Some(Event::Render) => {
        ui::draw(greeter.clone(), &mut terminal, cursor_on).await?;
      },
      Some(Event::Key(key)) => {
        let requested_exit = keyboard::handle(greeter.clone(), key, ipc.clone()).await?;
        if let Some(status) = requested_exit {
          crate::exit(&mut *greeter.write().await, status).await;
        }
      },

      Some(Event::Exit(status)) => {
        crate::exit(&mut *greeter.write().await, status).await;
      },

      Some(Event::PowerCommand(command)) => {
        if let PowerPostAction::ClearScreen = power::run(&greeter, command).await {
          execute!(io::stdout(), Show, LeaveAlternateScreen)?;
          terminal.set_cursor_position((1, 1))?;
          terminal.clear()?;
          disable_raw_mode()?;

          break;
        }
      },

      _ => {},
      },

      _ = cursor_interval.tick() => {
        cursor_on = !cursor_on;
        ui::draw(greeter.clone(), &mut terminal, cursor_on).await?;
      },
    }
  }

  Ok(())
}

async fn exit(greeter: &mut Greeter, status: AuthStatus) {
  tracing::info!("preparing exit with status {}", status);

  match status {
    AuthStatus::Success => {},
    AuthStatus::Cancel | AuthStatus::Failure => Ipc::cancel(greeter).await,
  }

  #[cfg(not(test))]
  clear_screen();

  let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);
  let _ = disable_raw_mode();

  greeter.exit = Some(status);
}

fn register_panic_handler() {
  let hook = std::panic::take_hook();

  std::panic::set_hook(Box::new(move |info| {
    #[cfg(not(test))]
    clear_screen();

    let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);
    let _ = disable_raw_mode();

    hook(info);
  }));
}

#[cfg(not(test))]
pub fn clear_screen() {
  let backend = CrosstermBackend::new(io::stdout());

  if let Ok(mut terminal) = Terminal::new(backend) {
    let _ = terminal.hide_cursor();
    let _ = terminal.clear();
  }
}

fn init_logger(greeter: &Greeter) -> Option<WorkerGuard> {
  use tracing_subscriber::{
    filter::{LevelFilter, Targets},
    prelude::*,
  };

  let logfile = OpenOptions::new().write(true).create(true).append(true).clone();

  match (greeter.debug, logfile.open(&greeter.logfile)) {
    (true, Ok(file)) => {
      let (appender, guard) = tracing_appender::non_blocking(file);
      let target = Targets::new().with_target("tuigreet", LevelFilter::DEBUG);

      tracing_subscriber::registry()
        .with(
          tracing_subscriber::fmt::layer()
            .with_writer(appender)
            .with_line_number(true),
        )
        .with(target)
        .init();

      Some(guard)
    },

    _ => None,
  }
}
