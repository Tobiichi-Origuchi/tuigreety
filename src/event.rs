use std::{io, time::Duration};

use crossterm::event::{Event as TermEvent, EventStream, KeyEvent};
use futures_util::{Stream, StreamExt};
use tokio::{
  sync::{
    mpsc::{self, Sender},
    oneshot,
    watch,
  },
  task::{JoinError, JoinHandle},
  time::{Instant, MissedTickBehavior},
};

use crate::{AuthStatus, power::PowerRequest};

pub const DEFAULT_REFRESH_RATE: u16 = 2;
pub const MAX_REFRESH_RATE: u16 = 240;

const EVENT_QUEUE_CAPACITY: usize = 10;
const WORKER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

pub enum Event {
  Key(KeyEvent),
  Render,
}

/// An action that must reach the main loop without competing with rendering
/// and terminal input for space in the bounded event queue.
pub enum Control {
  Exit(AuthStatus),
  PowerCommand(PowerRequest),
  CancelPower,
}

pub struct Events {
  rx: mpsc::Receiver<Event>,
  tx: mpsc::Sender<Event>,
  refresh_rate: watch::Sender<u16>,
  worker: EventWorker,
}

impl Events {
  pub async fn new() -> Events {
    Self::spawn(EventStream::new())
  }

  #[cfg(test)]
  pub(crate) async fn testing() -> Events {
    Self::with_stream(futures_util::stream::pending())
  }

  #[cfg(test)]
  pub(crate) fn with_stream<S>(stream: S) -> Events
  where
    S: Stream<Item = io::Result<TermEvent>> + Send + Unpin + 'static,
  {
    Self::spawn(stream)
  }

  fn spawn<S>(stream: S) -> Events
  where
    S: Stream<Item = io::Result<TermEvent>> + Send + Unpin + 'static,
  {
    let (tx, rx) = mpsc::channel(EVENT_QUEUE_CAPACITY);
    let (refresh_rate, refresh_rx) = watch::channel(DEFAULT_REFRESH_RATE);
    let worker = EventWorker::spawn(stream, tx.clone(), refresh_rx);

    Events {
      rx,
      tx,
      refresh_rate,
      worker,
    }
  }

  pub async fn next_result(&mut self) -> io::Result<Event> {
    if !self.worker.is_active() {
      return Err(io::Error::new(
        io::ErrorKind::BrokenPipe,
        "terminal event worker is unavailable",
      ));
    }

    let rx = &mut self.rx;
    let worker = &mut self.worker;
    tokio::select! {
      biased;

      result = worker.finished() => match result {
        Ok(()) => Err(io::Error::new(
          io::ErrorKind::BrokenPipe,
          "terminal event worker stopped unexpectedly",
        )),
        Err(error) => Err(error),
      },

      event = rx.recv() => event.ok_or_else(|| io::Error::new(
        io::ErrorKind::BrokenPipe,
        "terminal event queue closed unexpectedly",
      )),
    }
  }

  pub async fn shutdown(&mut self) -> io::Result<()> {
    self.worker.shutdown().await
  }

  pub fn sender(&self) -> Sender<Event> {
    self.tx.clone()
  }

  pub fn set_refresh_rate(&self, refresh_rate: u16) {
    let refresh_rate = refresh_rate.clamp(1, MAX_REFRESH_RATE);
    self.refresh_rate.send_if_modified(|current| {
      if *current == refresh_rate {
        return false;
      }

      *current = refresh_rate;
      true
    });
  }
}

struct EventWorker {
  shutdown: Option<oneshot::Sender<()>>,
  handle: Option<JoinHandle<io::Result<()>>>,
}

impl EventWorker {
  fn spawn<S>(stream: S, sender: mpsc::Sender<Event>, refresh_rate: watch::Receiver<u16>) -> EventWorker
  where
    S: Stream<Item = io::Result<TermEvent>> + Send + Unpin + 'static,
  {
    let (shutdown, shutdown_rx) = oneshot::channel();
    let handle = tokio::spawn(run_worker(stream, sender, refresh_rate, shutdown_rx));

    EventWorker {
      shutdown: Some(shutdown),
      handle: Some(handle),
    }
  }

  fn is_active(&self) -> bool {
    self.handle.is_some()
  }

  async fn finished(&mut self) -> io::Result<()> {
    let result = self
      .handle
      .as_mut()
      .expect("an active event worker must have a task")
      .await;
    self.handle = None;
    self.shutdown = None;
    worker_result(result)
  }

  async fn shutdown(&mut self) -> io::Result<()> {
    if let Some(shutdown) = self.shutdown.take() {
      let _ = shutdown.send(());
    }

    let Some(handle) = self.handle.as_mut() else {
      return Ok(());
    };

    let result = match tokio::time::timeout(WORKER_SHUTDOWN_TIMEOUT, &mut *handle).await {
      Ok(result) => Some(result),
      Err(_) => {
        handle.abort();
        let _ = tokio::time::timeout(WORKER_SHUTDOWN_TIMEOUT, &mut *handle).await;
        None
      },
    };
    self.handle = None;

    match result {
      Some(result) => worker_result(result),
      None => Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "terminal event worker did not stop in time",
      )),
    }
  }
}

impl Drop for EventWorker {
  fn drop(&mut self) {
    if let Some(shutdown) = self.shutdown.take() {
      let _ = shutdown.send(());
    }
    if let Some(handle) = self.handle.take() {
      handle.abort();
    }
  }
}

async fn run_worker<S>(
  mut stream: S,
  sender: mpsc::Sender<Event>,
  mut refresh_rate: watch::Receiver<u16>,
  mut shutdown: oneshot::Receiver<()>,
) -> io::Result<()>
where
  S: Stream<Item = io::Result<TermEvent>> + Unpin,
{
  request_render(&sender)?;
  let mut render_ticks = render_interval(*refresh_rate.borrow_and_update());

  loop {
    tokio::select! {
      biased;

      _ = &mut shutdown => return Ok(()),
      _ = sender.closed() => return Ok(()),

      changed = refresh_rate.changed() => {
        if changed.is_err() {
          return Ok(());
        }
        render_ticks = render_interval(*refresh_rate.borrow_and_update());
      },

      _ = render_ticks.tick() => request_render(&sender)?,

      event = stream.next() => match event {
        Some(Ok(TermEvent::Key(key))) => {
          if !send_required(&sender, Event::Key(key), &mut shutdown).await? {
            return Ok(());
          }
          if !send_required(&sender, Event::Render, &mut shutdown).await? {
            return Ok(());
          }
        },
        Some(Ok(TermEvent::Resize(_, _))) => request_render(&sender)?,
        Some(Ok(_)) => {},
        Some(Err(error)) => return Err(error),
        None => {
          return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "terminal event stream ended unexpectedly",
          ));
        },
      },
    }
  }
}

async fn send_required(
  sender: &mpsc::Sender<Event>,
  event: Event,
  shutdown: &mut oneshot::Receiver<()>,
) -> io::Result<bool> {
  tokio::select! {
    biased;
    _ = &mut *shutdown => Ok(false),
    _ = sender.closed() => Ok(false),
    result = sender.send(event) => result.map(|()| true).map_err(|_| queue_closed()),
  }
}

fn render_interval(refresh_rate: u16) -> tokio::time::Interval {
  let duration = frame_duration(refresh_rate);
  let mut interval = tokio::time::interval_at(Instant::now() + duration, duration);
  interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
  interval
}

fn request_render(sender: &mpsc::Sender<Event>) -> io::Result<()> {
  match sender.try_send(Event::Render) {
    Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => Ok(()),
    Err(mpsc::error::TrySendError::Closed(_)) => Err(queue_closed()),
  }
}

fn queue_closed() -> io::Error {
  io::Error::new(io::ErrorKind::BrokenPipe, "terminal event receiver closed unexpectedly")
}

fn worker_result(result: Result<io::Result<()>, JoinError>) -> io::Result<()> {
  match result {
    Ok(result) => result,
    Err(error) => Err(io::Error::other(format!("terminal event worker failed: {error}"))),
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
  use std::{
    pin::Pin,
    task::{Context, Poll},
  };

  use crossterm::event::{KeyCode, KeyModifiers};
  use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

  use super::*;

  const TEST_TIMEOUT: Duration = Duration::from_millis(250);

  struct ChannelStream {
    receiver: UnboundedReceiver<io::Result<TermEvent>>,
  }

  impl Stream for ChannelStream {
    type Item = io::Result<TermEvent>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
      self.receiver.poll_recv(context)
    }
  }

  fn channel_stream() -> (UnboundedSender<io::Result<TermEvent>>, ChannelStream) {
    let (sender, receiver) = mpsc::unbounded_channel();
    (sender, ChannelStream { receiver })
  }

  async fn next(events: &mut Events) -> Event {
    tokio::time::timeout(TEST_TIMEOUT, events.next_result())
      .await
      .expect("event worker did not respond")
      .expect("event worker failed unexpectedly")
  }

  async fn source_error(events: &mut Events) -> io::Error {
    for _ in 0..2 {
      match tokio::time::timeout(TEST_TIMEOUT, events.next_result())
        .await
        .expect("event worker did not report its source failure")
      {
        Ok(Event::Render) => {},
        Ok(_) => panic!("unexpected event before source failure"),
        Err(error) => return error,
      }
    }

    panic!("event worker did not report its source failure")
  }

  #[tokio::test]
  async fn stream_eof_is_reported() {
    let mut events = Events::with_stream(futures_util::stream::empty());

    let error = source_error(&mut events).await;

    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    assert!(error.to_string().contains("ended unexpectedly"));
  }

  #[tokio::test]
  async fn stream_errors_are_preserved() {
    let mut events = Events::with_stream(futures_util::stream::iter([Err(io::Error::new(
      io::ErrorKind::PermissionDenied,
      "input denied",
    ))]));

    let error = source_error(&mut events).await;

    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(error.to_string(), "input denied");
  }

  #[tokio::test]
  async fn worker_panics_are_reported_once_without_a_retry_loop() {
    struct PanicStream;

    impl Stream for PanicStream {
      type Item = io::Result<TermEvent>;

      fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        panic!("injected terminal stream panic");
      }
    }

    let mut events = Events::with_stream(PanicStream);
    let error = source_error(&mut events).await;
    assert_eq!(error.kind(), io::ErrorKind::Other);
    assert!(error.to_string().contains("terminal event worker failed"));

    let result = tokio::time::timeout(TEST_TIMEOUT, events.next_result())
      .await
      .expect("a stopped worker caused a retry loop");
    let Err(unavailable) = result else {
      panic!("a stopped worker became active again");
    };
    assert_eq!(unavailable.kind(), io::ErrorKind::BrokenPipe);
    assert!(unavailable.to_string().contains("unavailable"));
  }

  #[tokio::test]
  async fn keys_are_delivered_and_key_and_resize_events_request_renders() {
    let (input, stream) = channel_stream();
    let mut events = Events::with_stream(stream);
    assert!(matches!(next(&mut events).await, Event::Render));

    let key = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
    input.send(Ok(TermEvent::Key(key))).unwrap();
    assert!(matches!(next(&mut events).await, Event::Key(actual) if actual == key));
    assert!(matches!(next(&mut events).await, Event::Render));

    input.send(Ok(TermEvent::Resize(120, 40))).unwrap();
    assert!(matches!(next(&mut events).await, Event::Render));
    events.shutdown().await.unwrap();
  }

  #[tokio::test]
  async fn shutdown_interrupts_a_key_blocked_by_a_full_queue() {
    let (input, stream) = channel_stream();
    let mut events = Events::with_stream(stream);
    assert!(matches!(next(&mut events).await, Event::Render));
    fill_event_queue(&events);

    input
      .send(Ok(TermEvent::Key(KeyEvent::new(
        KeyCode::Char('x'),
        KeyModifiers::empty(),
      ))))
      .unwrap();
    tokio::task::yield_now().await;

    tokio::time::timeout(TEST_TIMEOUT, events.shutdown())
      .await
      .expect("a full event queue blocked worker shutdown")
      .unwrap();
  }

  #[tokio::test]
  async fn a_key_queued_after_render_backlog_keeps_its_following_render() {
    let (input, stream) = channel_stream();
    let mut events = Events::with_stream(stream);
    assert!(matches!(next(&mut events).await, Event::Render));
    fill_event_queue(&events);

    let key = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::empty());
    input.send(Ok(TermEvent::Key(key))).unwrap();
    tokio::task::yield_now().await;

    loop {
      if matches!(next(&mut events).await, Event::Key(actual) if actual == key) {
        break;
      }
    }
    assert!(matches!(next(&mut events).await, Event::Render));
    events.shutdown().await.unwrap();
  }

  #[tokio::test]
  async fn cancelling_next_does_not_lose_the_following_event() {
    let (input, stream) = channel_stream();
    let mut events = Events::with_stream(stream);
    assert!(matches!(next(&mut events).await, Event::Render));

    assert!(
      tokio::time::timeout(Duration::from_millis(10), events.next_result())
        .await
        .is_err()
    );

    let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::empty());
    input.send(Ok(TermEvent::Key(key))).unwrap();
    assert!(matches!(next(&mut events).await, Event::Key(actual) if actual == key));
    events.shutdown().await.unwrap();
  }

  #[tokio::test]
  async fn refresh_changes_rearm_a_skip_interval_immediately() {
    let mut events = Events::testing().await;
    assert!(matches!(next(&mut events).await, Event::Render));

    let interval = render_interval(DEFAULT_REFRESH_RATE);
    assert_eq!(interval.missed_tick_behavior(), MissedTickBehavior::Skip);

    events.set_refresh_rate(100);
    assert_eq!(*events.refresh_rate.borrow(), 100);
    assert!(matches!(next(&mut events).await, Event::Render));
    events.shutdown().await.unwrap();
  }

  #[tokio::test]
  async fn dropping_events_aborts_and_drops_the_stream() {
    struct DropStream(Option<oneshot::Sender<()>>);

    impl Stream for DropStream {
      type Item = io::Result<TermEvent>;

      fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Pending
      }
    }

    impl Drop for DropStream {
      fn drop(&mut self) {
        if let Some(dropped) = self.0.take() {
          let _ = dropped.send(());
        }
      }
    }

    let (dropped, observed) = oneshot::channel();
    let mut events = Events::with_stream(DropStream(Some(dropped)));
    assert!(matches!(next(&mut events).await, Event::Render));
    drop(events);

    tokio::time::timeout(TEST_TIMEOUT, observed)
      .await
      .expect("event stream was not dropped")
      .expect("event stream drop notification was lost");
  }

  #[test]
  fn frame_duration_matches_the_requested_rate() {
    assert_eq!(frame_duration(2), Duration::from_millis(500));
    assert_eq!(frame_duration(100), Duration::from_millis(10));
  }
}
