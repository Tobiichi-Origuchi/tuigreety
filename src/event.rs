use std::{
  sync::{
    Arc,
    atomic::{AtomicU16, Ordering},
  },
  time::Duration,
};

#[cfg(not(test))]
use crossterm::event::EventStream;
use crossterm::event::{Event as TermEvent, KeyEvent};
use futures_util::{StreamExt, future::FutureExt};
use tokio::{
  process::Command,
  sync::mpsc::{self, Sender},
};

use crate::AuthStatus;

pub const DEFAULT_REFRESH_RATE: u16 = 2;
pub const MAX_REFRESH_RATE: u16 = 240;

pub enum Event {
  Key(KeyEvent),
  Render,
  #[cfg_attr(test, allow(dead_code))]
  ReloadConfig,
}

/// An action that must reach the main loop without competing with rendering
/// and terminal input for space in the bounded event queue.
pub enum Control {
  Exit(AuthStatus),
  PowerCommand(Box<Command>),
}

pub struct Events {
  rx: mpsc::Receiver<Event>,
  tx: mpsc::Sender<Event>,
  refresh_rate: Arc<AtomicU16>,
}

impl Events {
  pub async fn new() -> Events {
    let (tx, rx) = mpsc::channel(10);
    let refresh_rate = Arc::new(AtomicU16::new(DEFAULT_REFRESH_RATE));

    tokio::task::spawn({
      let tx = tx.clone();
      let refresh_rate = refresh_rate.clone();

      async move {
        #[cfg(not(test))]
        let mut stream = EventStream::new();

        // In tests, we are not capturing events from the terminal, so we need
        // to replace the crossterm::EventStream with a dummy pending stream.
        #[cfg(test)]
        let mut stream = futures_util::stream::pending::<Result<TermEvent, ()>>();

        let mut current_rate = refresh_rate.load(Ordering::Relaxed);
        let mut render_interval = tokio::time::interval(frame_duration(current_rate));

        loop {
          let requested_rate = refresh_rate.load(Ordering::Relaxed);
          if requested_rate != current_rate {
            current_rate = requested_rate;
            render_interval = tokio::time::interval(frame_duration(current_rate));
            render_interval.tick().await;
          }

          let render = render_interval.tick();
          let event = stream.next().fuse();

          tokio::select! {
            event = event => {
              if let Some(Ok(term_event)) = event {
                match term_event {
                  TermEvent::Key(event) => {
                    let _ = tx.send(Event::Key(event)).await;
                    let _ = tx.send(Event::Render).await;
                  }
                  TermEvent::Resize(_, _) => {
                    let _ = tx.send(Event::Render).await;
                  }
                  _ => {}
                }
              }
            }

            _ = render => { let _ = tx.try_send(Event::Render); },
          }
        }
      }
    });

    Events { rx, tx, refresh_rate }
  }

  pub async fn next(&mut self) -> Option<Event> {
    self.rx.recv().await
  }

  pub fn sender(&self) -> Sender<Event> {
    self.tx.clone()
  }

  pub fn set_refresh_rate(&self, refresh_rate: u16) {
    self.refresh_rate.store(refresh_rate, Ordering::Relaxed);
  }
}

fn frame_duration(refresh_rate: u16) -> Duration {
  Duration::from_secs_f64(1.0 / f64::from(refresh_rate))
}

#[cfg(test)]
pub(crate) fn fill_event_queue(events: &Events) {
  loop {
    match events.tx.try_send(Event::Render) {
      Ok(()) => {},
      Err(mpsc::error::TrySendError::Full(_)) => return,
      Err(mpsc::error::TrySendError::Closed(_)) => panic!("event queue closed while filling it"),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn refresh_rate_can_be_changed() {
    let events = Events::new().await;
    events.set_refresh_rate(60);

    assert_eq!(events.refresh_rate.load(Ordering::Relaxed), 60);
    assert_eq!(frame_duration(2), Duration::from_millis(500));
  }
}
