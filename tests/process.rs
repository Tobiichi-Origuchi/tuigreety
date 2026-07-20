#![cfg(target_os = "linux")]

use std::{
  ffi::OsStr,
  fs::{self, File},
  io::{self, Read, Write},
  path::{Path, PathBuf},
  process::{Child, Command, ExitStatus, Output, Stdio},
  thread,
  time::{Duration, Instant},
};

use greetd_ipc::{AuthMessageType, Request, Response, codec::TokioCodec};
use nix::{
  errno::Errno,
  fcntl::{FcntlArg, OFlag, fcntl},
  pty::{Winsize, openpty},
  sys::{
    signal::{Signal, kill},
    termios::{LocalFlags, Termios, tcgetattr},
  },
  unistd::Pid,
};
use tempfile::TempDir;
use tokio::net::UnixListener;

const PROCESS_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_DELAY: Duration = Duration::from_millis(10);
const OUTPUT_LIMIT: usize = 256 * 1024;
const ENTER_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049h";
const LEAVE_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049l";
const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";

struct PtyProcess {
  child: Child,
  master: File,
  slave: File,
  initial_termios: Termios,
  output: Vec<u8>,
  saw_enter_alternate_screen: bool,
  saw_leave_alternate_screen: bool,
  saw_hide_cursor: bool,
  saw_show_cursor: bool,
  reaped: bool,
}

impl PtyProcess {
  fn spawn<I, S>(args: I, environment: &[(&str, &OsStr)]) -> Self
  where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
  {
    let window = Winsize {
      ws_row: 40,
      ws_col: 120,
      ws_xpixel: 0,
      ws_ypixel: 0,
    };
    let pty = openpty(&window, None).expect("failed to open a pseudoterminal");
    let master = File::from(pty.master);
    let slave = File::from(pty.slave);
    let initial_termios = tcgetattr(&slave).expect("failed to read initial PTY settings");
    set_nonblocking(&master);

    let mut command = Command::new(env!("CARGO_BIN_EXE_tuigreet"));
    command
      .args(args)
      .env("TERM", "xterm-256color")
      .env_remove("GREETD_SOCK")
      .stdin(Stdio::from(slave.try_clone().expect("failed to clone PTY slave")))
      .stdout(Stdio::from(slave.try_clone().expect("failed to clone PTY slave")))
      .stderr(Stdio::from(slave.try_clone().expect("failed to clone PTY slave")));
    for (key, value) in environment {
      command.env(key, value);
    }

    let child = command.spawn().expect("failed to start tuigreet");
    Self {
      child,
      master,
      slave,
      initial_termios,
      output: Vec::new(),
      saw_enter_alternate_screen: false,
      saw_leave_alternate_screen: false,
      saw_hide_cursor: false,
      saw_show_cursor: false,
      reaped: false,
    }
  }

  fn wait_for_output(&mut self, needle: &str) {
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    loop {
      self.drain_output();
      if self.output_text().contains(needle) {
        return;
      }
      if let Some(status) = self.child.try_wait().expect("failed to poll tuigreet") {
        self.reaped = true;
        self.drain_output();
        panic!(
          "tuigreet exited with {status} before producing {needle:?}; output:\n{}",
          self.output_text()
        );
      }
      assert!(
        Instant::now() < deadline,
        "timed out waiting for {needle:?}; output:\n{}",
        self.output_text()
      );
      thread::sleep(POLL_DELAY);
    }
  }

  fn send(&mut self, bytes: &[u8]) {
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    let mut remaining = bytes;
    while !remaining.is_empty() {
      match self.master.write(remaining) {
        Ok(0) => panic!("PTY master stopped accepting input"),
        Ok(written) => remaining = &remaining[written..],
        Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {
          assert!(Instant::now() < deadline, "timed out writing to PTY");
          thread::sleep(POLL_DELAY);
        },
        Err(error) => panic!("failed to write to PTY: {error}"),
      }
    }
  }

  fn signal(&self, signal: Signal) {
    let pid = i32::try_from(self.child.id()).expect("child PID does not fit in i32");
    kill(Pid::from_raw(pid), signal).expect("failed to signal tuigreet");
  }

  fn wait(&mut self) -> ExitStatus {
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    loop {
      self.drain_output();
      if let Some(status) = self.child.try_wait().expect("failed to poll tuigreet") {
        self.reaped = true;
        // The kernel can report process exit before every queued PTY byte has
        // reached the master. Give the restoration sequence one short drain.
        let drain_deadline = Instant::now() + Duration::from_millis(100);
        while Instant::now() < drain_deadline {
          self.drain_output();
          thread::sleep(POLL_DELAY);
        }
        return status;
      }
      assert!(
        Instant::now() < deadline,
        "tuigreet did not exit; output:\n{}",
        self.output_text()
      );
      thread::sleep(POLL_DELAY);
    }
  }

  fn assert_terminal_restored(&self) {
    let restored = tcgetattr(&self.slave).expect("failed to read restored PTY settings");
    assert_eq!(restored, self.initial_termios, "tuigreet did not restore PTY settings");
    assert!(self.saw_enter_alternate_screen, "alternate screen was never entered");
    assert!(self.saw_hide_cursor, "cursor was never hidden");
    assert!(self.saw_show_cursor, "cursor was not shown during cleanup");
    assert!(self.saw_leave_alternate_screen, "alternate screen was not left");
  }

  fn assert_terminal_active(&self) {
    let active = tcgetattr(&self.slave).expect("failed to read active PTY settings");
    assert_ne!(active, self.initial_termios, "tuigreet never enabled raw mode");
    assert!(
      !active.local_flags.contains(LocalFlags::ICANON),
      "canonical mode is still enabled"
    );
    assert!(
      !active.local_flags.contains(LocalFlags::ECHO),
      "input echo is still enabled"
    );
    assert!(self.saw_enter_alternate_screen, "alternate screen was never entered");
    assert!(self.saw_hide_cursor, "cursor was never hidden");
  }

  fn clear_output(&mut self) {
    self.output.clear();
  }

  fn output_text(&self) -> String {
    String::from_utf8_lossy(&self.output).into_owned()
  }

  fn drain_output(&mut self) {
    let mut chunk = [0_u8; 8192];
    loop {
      match self.master.read(&mut chunk) {
        Ok(0) => return,
        Ok(read) => self.record_output(&chunk[..read]),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
        // Linux PTY masters report EIO once every slave descriptor is closed.
        Err(error) if error.raw_os_error() == Some(Errno::EIO as i32) => return,
        Err(error) => panic!("failed to read PTY output: {error}"),
      }
    }
  }

  fn record_output(&mut self, chunk: &[u8]) {
    self.output.extend_from_slice(chunk);
    // Inspect the accumulated bytes so an escape sequence split across two
    // reads is still observed.
    self.saw_enter_alternate_screen |= contains_bytes(&self.output, ENTER_ALTERNATE_SCREEN);
    self.saw_leave_alternate_screen |= contains_bytes(&self.output, LEAVE_ALTERNATE_SCREEN);
    self.saw_hide_cursor |= contains_bytes(&self.output, HIDE_CURSOR);
    self.saw_show_cursor |= contains_bytes(&self.output, SHOW_CURSOR);
    if self.output.len() > OUTPUT_LIMIT {
      self.output.drain(..self.output.len() - OUTPUT_LIMIT);
    }
  }
}

impl Drop for PtyProcess {
  fn drop(&mut self) {
    if self.reaped {
      return;
    }
    match self.child.try_wait() {
      Ok(Some(_)) => self.reaped = true,
      Ok(None) | Err(_) => {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.reaped = true;
      },
    }
  }
}

#[derive(Debug, Eq, PartialEq)]
struct IpcExchange {
  username: String,
  response: Option<String>,
  command: Vec<String>,
  environment: Vec<String>,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_process_completes_greetd_protocol_and_restores_terminal() {
  let temp = TempDir::new().expect("failed to create process-test directory");
  let config = temp.path().join("config.toml");
  write_config(&config, real_config("PROCESS-IPC"));
  let socket = temp.path().join("greetd.sock");
  let listener = UnixListener::bind(&socket).expect("failed to bind greetd test socket");
  let server = tokio::spawn(serve_authentication(listener));

  let mut process = PtyProcess::spawn(
    [
      "--config",
      config.to_str().expect("non-UTF-8 test path"),
      "--cmd",
      "process-session --flag",
      "--env",
      "PROCESS_TEST=1",
      "--env",
      "DISPLAY=:99",
      "--session-wrapper",
      "process-wrapper --setup",
      "--allow-command-editor",
      "--no-xsession-wrapper",
    ],
    &[("GREETD_SOCK", socket.as_os_str())],
  );
  process.wait_for_output("Username:");
  process.assert_terminal_active();
  process.send(b"alice\r");
  process.wait_for_output("Password:");
  process.send(b"correct horse\r");

  let status = process.wait();
  assert_eq!(status.code(), Some(0), "output:\n{}", process.output_text());
  process.assert_terminal_restored();

  let exchange = tokio::time::timeout(PROCESS_TIMEOUT, server)
    .await
    .expect("greetd test server timed out")
    .expect("greetd test server panicked")
    .expect("greetd test server failed");
  assert_eq!(exchange, IpcExchange {
    username: "alice".into(),
    response: Some("correct horse".into()),
    command: vec!["process-wrapper --setup process-session --flag".into()],
    environment: vec!["PROCESS_TEST=1".into(), "DISPLAY=:99".into()],
  });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_session_disconnect_reconnects_and_retries_safely() {
  let temp = TempDir::new().expect("failed to create process-test directory");
  let config = temp.path().join("config.toml");
  write_config(&config, real_config("PROCESS-RECONNECT"));
  let socket = temp.path().join("greetd.sock");
  let listener = UnixListener::bind(&socket).expect("failed to bind greetd test socket");
  let server = tokio::spawn(serve_authentication_after_create_disconnect(listener));

  let mut process = PtyProcess::spawn(
    [
      "--config",
      config.to_str().expect("non-UTF-8 test path"),
      "--cmd",
      "reconnected-session",
      "--allow-command-editor",
      "--no-xsession-wrapper",
    ],
    &[("GREETD_SOCK", socket.as_os_str())],
  );
  process.wait_for_output("Username:");
  process.send(b"alice\r");
  process.wait_for_output("Password:");
  process.send(b"retry-secret\r");

  let status = process.wait();
  assert_eq!(status.code(), Some(0), "output:\n{}", process.output_text());
  process.assert_terminal_restored();

  let exchange = tokio::time::timeout(PROCESS_TIMEOUT, server)
    .await
    .expect("greetd reconnect server timed out")
    .expect("greetd reconnect server panicked")
    .expect("greetd reconnect server failed");
  assert_eq!(exchange, IpcExchange {
    username: "alice".into(),
    response: Some("retry-secret".into()),
    command: vec!["reconnected-session".into()],
    environment: Vec::new(),
  });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_create_session_disconnects_fail_and_restore_terminal() {
  let temp = TempDir::new().expect("failed to create process-test directory");
  let config = temp.path().join("config.toml");
  write_config(&config, real_config("PROCESS-RECONNECT-FAILURE"));
  let socket = temp.path().join("greetd.sock");
  let listener = UnixListener::bind(&socket).expect("failed to bind greetd test socket");
  let server = tokio::spawn(serve_repeated_create_disconnects(listener));

  let mut process = PtyProcess::spawn(
    [
      "--config",
      config.to_str().expect("non-UTF-8 test path"),
      "--cmd",
      "unreachable-session",
      "--no-xsession-wrapper",
    ],
    &[("GREETD_SOCK", socket.as_os_str())],
  );
  process.wait_for_output("Username:");
  process.assert_terminal_active();
  process.send(b"alice\r");

  let status = process.wait();
  assert_eq!(status.code(), Some(1), "output:\n{}", process.output_text());
  process.assert_terminal_restored();

  tokio::time::timeout(PROCESS_TIMEOUT, server)
    .await
    .expect("greetd reconnect-failure server timed out")
    .expect("greetd reconnect-failure server panicked")
    .expect("greetd reconnect-failure server failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_session_disconnect_is_not_replayed_and_restores_terminal() {
  let temp = TempDir::new().expect("failed to create process-test directory");
  let config = temp.path().join("config.toml");
  write_config(&config, real_config("PROCESS-START-DISCONNECT"));
  let socket = temp.path().join("greetd.sock");
  let listener = UnixListener::bind(&socket).expect("failed to bind greetd test socket");
  let server = tokio::spawn(serve_start_disconnect(listener));

  let mut process = PtyProcess::spawn(
    [
      "--config",
      config.to_str().expect("non-UTF-8 test path"),
      "--cmd",
      "uncertain-session",
      "--allow-command-editor",
      "--no-xsession-wrapper",
    ],
    &[("GREETD_SOCK", socket.as_os_str())],
  );
  process.wait_for_output("Username:");
  process.send(b"alice\r");
  process.wait_for_output("Password:");
  process.send(b"one-shot-secret\r");

  let status = process.wait();
  assert_eq!(status.code(), Some(1), "output:\n{}", process.output_text());
  process.assert_terminal_restored();

  let exchange = tokio::time::timeout(PROCESS_TIMEOUT, server)
    .await
    .expect("greetd disconnect server timed out")
    .expect("greetd disconnect server panicked")
    .expect("greetd disconnect server failed");
  assert_eq!(exchange, IpcExchange {
    username: "alice".into(),
    response: Some("one-shot-secret".into()),
    command: vec!["uncertain-session".into()],
    environment: Vec::new(),
  });
}

#[test]
fn atomic_config_reload_rejects_invalid_data_then_recovers() {
  let temp = TempDir::new().expect("failed to create process-test directory");
  let config = temp.path().join("config.toml");
  write_config(&config, mock_config("OOOOOOOOOOOOOOOO"));

  let mut process = PtyProcess::spawn(
    [
      "--config",
      config.to_str().expect("non-UTF-8 test path"),
      "--mock",
      "--cmd",
      "mock-session",
      "--allow-command-editor",
      "--no-xsession-wrapper",
    ],
    &[],
  );
  process.wait_for_output("OOOOOOOOOOOOOOOO");
  process.assert_terminal_active();
  process.clear_output();

  atomic_replace(&config, "[display]\ngreeting = = 'broken'\n", 1);
  process.wait_for_output("configuration reload rejected");

  // Ratatui writes only changed cells after the first frame. Use a marker with
  // no bytes in common with the previous value so the complete replacement is
  // observable in the PTY stream.
  atomic_replace(&config, &mock_config("NNNNNNNNNNNNNNNN"), 2);
  process.wait_for_output("NNNNNNNNNNNNNNNN");
  process.send(b"reload-user\r");
  process.wait_for_output("Password:");
  process.send(b"anything\r");

  let status = process.wait();
  assert_eq!(status.code(), Some(0), "output:\n{}", process.output_text());
  process.assert_terminal_restored();
}

#[test]
fn termination_signals_restore_terminal_and_fail_cleanly() {
  let temp = TempDir::new().expect("failed to create process-test directory");
  let config = temp.path().join("config.toml");
  write_config(&config, mock_config("PROCESS-SIGNAL"));

  for signal in [Signal::SIGINT, Signal::SIGTERM] {
    let mut process = PtyProcess::spawn(
      [
        "--config",
        config.to_str().expect("non-UTF-8 test path"),
        "--mock",
        "--cmd",
        "signal-test-session",
        "--allow-command-editor",
        "--no-xsession-wrapper",
      ],
      &[],
    );
    process.wait_for_output("Username:");
    process.assert_terminal_active();
    process.signal(signal);

    let status = process.wait();
    assert_eq!(
      status.code(),
      Some(1),
      "{signal:?} produced an unexpected status; output:\n{}",
      process.output_text()
    );
    process.assert_terminal_restored();
  }
}

#[test]
fn kmscon_term_name_uses_the_standard_pty_path() {
  let temp = TempDir::new().expect("failed to create process-test directory");
  let config = temp.path().join("config.toml");
  write_config(&config, mock_config("PROCESS-KMSCON"));

  let mut process = PtyProcess::spawn(
    [
      "--config",
      config.to_str().expect("non-UTF-8 test path"),
      "--mock",
      "--cmd",
      "mock-session",
      "--no-xsession-wrapper",
    ],
    &[("TERM", OsStr::new("kmscon"))],
  );
  process.wait_for_output("Username:");
  process.assert_terminal_active();
  process.send(b"alice\r");
  process.wait_for_output("Password:");
  process.send(b"anything\r");

  let status = process.wait();
  assert_eq!(status.code(), Some(0), "output:\n{}", process.output_text());
  process.assert_terminal_restored();
}

#[test]
fn information_actions_need_neither_tty_nor_greetd_socket() {
  let temp = TempDir::new().expect("failed to create process-test directory");
  let config = temp.path().join("config.toml");
  write_config(&config, mock_config("PROCESS-CHECK"));

  let cases: &[(&[&str], &str, Option<bool>)] = &[
    (&["--help"], "Usage: tuigreet", Some(true)),
    (&["--version"], "tuigreet ", Some(true)),
    (
      &[
        "--check-config",
        "--config",
        config.to_str().expect("non-UTF-8 test path"),
      ],
      "Configuration files:",
      None,
    ),
  ];

  for (args, expected, expected_success) in cases {
    let output = run_information_action(args);
    let mut combined = output.stdout.clone();
    combined.extend_from_slice(&output.stderr);
    if let Some(expected_success) = expected_success {
      assert_eq!(
        output.status.success(),
        *expected_success,
        "{args:?} returned an unexpected status: {}",
        String::from_utf8_lossy(&combined)
      );
    }
    assert!(
      String::from_utf8_lossy(&combined).contains(expected),
      "{args:?} did not print {expected:?}: {}",
      String::from_utf8_lossy(&combined)
    );
    if args.contains(&"--check-config") {
      assert!(
        String::from_utf8_lossy(&combined).contains(config.to_str().expect("non-UTF-8 test path")),
        "check-config did not inspect the explicit file: {}",
        String::from_utf8_lossy(&combined)
      );
      let expected_result = if output.status.success() {
        "Configuration is valid."
      } else {
        "Configuration is invalid."
      };
      assert!(
        String::from_utf8_lossy(&combined).contains(expected_result),
        "check-config status and diagnostic disagree: {}",
        String::from_utf8_lossy(&combined)
      );
    }
    assert!(!contains_bytes(&combined, ENTER_ALTERNATE_SCREEN));
    assert!(!contains_bytes(&combined, LEAVE_ALTERNATE_SCREEN));
  }
}

#[test]
fn real_startup_requires_a_connectable_greetd_socket_before_terminal_setup() {
  let temp = TempDir::new().expect("failed to create process-test directory");
  let config = temp.path().join("config.toml");
  write_config(&config, real_config("PROCESS-PREFLIGHT"));
  let args = ["--config", config.to_str().expect("non-UTF-8 test path")];

  let missing = run_without_terminal(&args, &[]);
  assert_startup_transport_failure(&missing, "GREETD_SOCK must be defined");

  let unavailable = temp.path().join("missing-greetd.sock");
  let unconnectable = run_without_terminal(&args, &[("GREETD_SOCK", unavailable.as_os_str())]);
  assert_startup_transport_failure(&unconnectable, "failed to connect to greetd socket");
}

async fn serve_authentication(listener: UnixListener) -> Result<IpcExchange, String> {
  let (mut stream, _) = listener.accept().await.map_err(|error| error.to_string())?;

  authenticate_and_start(&mut stream).await
}

async fn serve_authentication_after_create_disconnect(listener: UnixListener) -> Result<IpcExchange, String> {
  let (mut abandoned, _) = listener.accept().await.map_err(|error| error.to_string())?;
  match Request::read_from(&mut abandoned)
    .await
    .map_err(|error| error.to_string())?
  {
    Request::CreateSession { username } if username == "alice" => {},
    request => return Err(format!("expected initial CreateSession, received {request:?}")),
  }
  drop(abandoned);

  let (mut stream, _) = listener.accept().await.map_err(|error| error.to_string())?;
  authenticate_and_start(&mut stream).await
}

async fn serve_repeated_create_disconnects(listener: UnixListener) -> Result<(), String> {
  for attempt in 0..3 {
    let (mut stream, _) = listener.accept().await.map_err(|error| error.to_string())?;
    match Request::read_from(&mut stream)
      .await
      .map_err(|error| error.to_string())?
    {
      Request::CreateSession { username } if username == "alice" => {},
      request => {
        return Err(format!(
          "attempt {} expected CreateSession for alice, received {request:?}",
          attempt + 1
        ));
      },
    }
  }
  Ok(())
}

async fn serve_start_disconnect(listener: UnixListener) -> Result<IpcExchange, String> {
  let (mut stream, _) = listener.accept().await.map_err(|error| error.to_string())?;
  let exchange = authenticate_until_start(&mut stream).await?;
  drop(stream);

  if tokio::time::timeout(Duration::from_millis(500), listener.accept())
    .await
    .is_ok()
  {
    return Err("tuigreet reconnected after an uncertain StartSession".into());
  }
  Ok(exchange)
}

async fn authenticate_and_start(stream: &mut tokio::net::UnixStream) -> Result<IpcExchange, String> {
  let exchange = authenticate_until_start(stream).await?;
  Response::Success
    .write_to(stream)
    .await
    .map_err(|error| error.to_string())?;
  Ok(exchange)
}

async fn authenticate_until_start(stream: &mut tokio::net::UnixStream) -> Result<IpcExchange, String> {
  let username = match Request::read_from(&mut *stream)
    .await
    .map_err(|error| error.to_string())?
  {
    Request::CreateSession { username } => username,
    request => return Err(format!("expected CreateSession, received {request:?}")),
  };
  Response::AuthMessage {
    auth_message_type: AuthMessageType::Secret,
    auth_message: "Password: ".into(),
  }
  .write_to(&mut *stream)
  .await
  .map_err(|error| error.to_string())?;

  let response = match Request::read_from(&mut *stream)
    .await
    .map_err(|error| error.to_string())?
  {
    Request::PostAuthMessageResponse { response } => response,
    request => return Err(format!("expected PostAuthMessageResponse, received {request:?}")),
  };
  Response::Success
    .write_to(&mut *stream)
    .await
    .map_err(|error| error.to_string())?;

  let (command, environment) = match Request::read_from(&mut *stream)
    .await
    .map_err(|error| error.to_string())?
  {
    Request::StartSession { cmd, env } => (cmd, env),
    request => return Err(format!("expected StartSession, received {request:?}")),
  };
  Ok(IpcExchange {
    username,
    response,
    command,
    environment,
  })
}

fn set_nonblocking(file: &File) {
  let current = fcntl(file, FcntlArg::F_GETFL).expect("failed to read PTY flags");
  let flags = OFlag::from_bits_truncate(current) | OFlag::O_NONBLOCK;
  fcntl(file, FcntlArg::F_SETFL(flags)).expect("failed to make PTY nonblocking");
}

fn run_information_action(args: &[&str]) -> Output {
  run_without_terminal(args, &[])
}

fn run_without_terminal(args: &[&str], environment: &[(&str, &OsStr)]) -> Output {
  let mut command = Command::new(env!("CARGO_BIN_EXE_tuigreet"));
  command
    .args(args)
    .env_remove("GREETD_SOCK")
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
  for (key, value) in environment {
    command.env(key, value);
  }
  let mut child = command.spawn().expect("failed to run tuigreet without a terminal");
  let deadline = Instant::now() + PROCESS_TIMEOUT;

  loop {
    if child.try_wait().expect("failed to poll information action").is_some() {
      return child
        .wait_with_output()
        .expect("failed to collect information action output");
    }
    if Instant::now() >= deadline {
      let _ = child.kill();
      let output = child
        .wait_with_output()
        .expect("failed to collect timed-out information action output");
      panic!(
        "noninteractive invocation {args:?} timed out: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
      );
    }
    thread::sleep(POLL_DELAY);
  }
}

fn assert_startup_transport_failure(output: &Output, expected: &str) {
  let mut combined = output.stdout.clone();
  combined.extend_from_slice(&output.stderr);
  assert!(
    !output.status.success(),
    "startup unexpectedly succeeded: {}",
    String::from_utf8_lossy(&combined)
  );
  assert!(
    contains_bytes(&combined, expected.as_bytes()),
    "startup did not report {expected:?}: {}",
    String::from_utf8_lossy(&combined)
  );
  assert!(
    !contains_bytes(&combined, ENTER_ALTERNATE_SCREEN) && !contains_bytes(&combined, HIDE_CURSOR),
    "startup touched terminal state before rejecting its greetd transport: {}",
    String::from_utf8_lossy(&combined)
  );
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
  haystack.windows(needle.len()).any(|window| window == needle)
}

fn write_config(path: &Path, contents: impl AsRef<[u8]>) {
  fs::write(path, contents).expect("failed to write test configuration");
}

fn atomic_replace(path: &Path, contents: &str, generation: u8) {
  let replacement = replacement_path(path, generation);
  fs::write(&replacement, contents).expect("failed to write replacement configuration");
  fs::rename(&replacement, path).expect("failed to atomically replace configuration");
}

fn replacement_path(path: &Path, generation: u8) -> PathBuf {
  path.with_extension(format!("toml.next-{generation}"))
}

fn common_config(mock: bool, greeting: &str) -> String {
  format!(
    "[general]\nmock = {mock}\ndebug = false\nipc-timeout = 5\n\n\
     [session]\ncommand = \"ignored-by-cli\"\nallow-command-editor = true\nenvironment = []\n\
     sessions = []\nxsessions = []\nwrapper = \"\"\n\
     xsession-wrapper = false\n\n\
     [display]\nwidth = 80\nissue = false\ngreeting = {greeting:?}\ntime = false\nrefresh-rate = 20\n\n\
     [remember]\nusername = false\nsession = false\nuser-session = false\n\n\
     [users]\nmenu = false\nautocomplete = false\n\n\
     [layout]\nwindow-padding = 0\ncontainer-padding = 1\nprompt-padding = 1\ngreet-align = \"center\"\n",
  )
}

fn real_config(greeting: &str) -> String {
  common_config(false, greeting)
}

fn mock_config(greeting: &str) -> String {
  common_config(true, greeting)
}
