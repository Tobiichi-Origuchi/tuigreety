use std::{
  fmt,
  fs,
  io,
  os::unix::process::CommandExt,
  path::Path,
  process::{ExitStatus, Stdio},
  time::Duration,
};

use nix::{
  errno::Errno,
  sys::signal::{Signal, kill, killpg},
  unistd::{Pid, setsid},
};
use tokio::{
  process::{Child, Command},
  sync::oneshot,
  task::{JoinError, JoinHandle},
  time::{Instant, sleep_until, timeout},
};

use crate::{AuthStatus, Greeter, event::Control, ui::power::Power};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const TERM_GRACE: Duration = Duration::from_millis(500);
const KILL_REAP_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandLine {
  argv: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PowerRequest {
  command: CommandLine,
  setsid: bool,
}

impl PowerRequest {
  fn new(command: CommandLine, setsid: bool) -> Self {
    Self { command, setsid }
  }

  #[cfg(test)]
  fn command(&self) -> &CommandLine {
    &self.command
  }

  #[cfg(test)]
  fn uses_setsid(&self) -> bool {
    self.setsid
  }
}

impl CommandLine {
  pub fn from_argv(argv: Vec<String>) -> Result<Self, &'static str> {
    if argv.first().is_none_or(String::is_empty) {
      Err("the command must contain a non-empty program")
    } else if argv.iter().any(|argument| argument.contains('\0')) {
      Err("the command must not contain NUL bytes")
    } else {
      Ok(Self { argv })
    }
  }

  pub fn parse(value: &str) -> Result<Self, &'static str> {
    let argv = shlex::split(value).ok_or("the command contains an unmatched quote or trailing escape")?;
    Self::from_argv(argv)
  }

  fn direct(program: &str, arguments: &[&str]) -> Self {
    let mut argv = Vec::with_capacity(arguments.len() + 1);
    argv.push(program.to_string());
    argv.extend(arguments.iter().map(|value| (*value).to_string()));
    Self { argv }
  }

  pub fn program(&self) -> &str {
    &self.argv[0]
  }

  pub fn arguments(&self) -> &[String] {
    &self.argv[1..]
  }

  pub fn argv(&self) -> &[String] {
    &self.argv
  }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum PowerCommand {
  #[default]
  Auto,
  Disabled,
  Explicit(CommandLine),
}

impl PowerCommand {
  pub fn resolve(&self, option: PowerOption) -> Option<CommandLine> {
    self.resolve_with(option, default_command)
  }

  pub(crate) fn resolve_with(
    &self,
    option: PowerOption,
    default: impl FnOnce(PowerOption) -> Option<CommandLine>,
  ) -> Option<CommandLine> {
    match self {
      Self::Auto => default(option),
      Self::Disabled => None,
      Self::Explicit(command) => Some(command.clone()),
    }
  }
}

#[derive(Default, Clone, Copy, Debug, PartialEq, Eq, Hash)]
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

pub fn default_command(option: PowerOption) -> Option<CommandLine> {
  default_command_for(option, Path::new("/"))
}

fn default_command_for(option: PowerOption, root: &Path) -> Option<CommandLine> {
  match option {
    PowerOption::Shutdown => Some(CommandLine::direct("shutdown", &["-h", "now"])),
    PowerOption::Reboot => Some(CommandLine::direct("shutdown", &["-r", "now"])),
    PowerOption::Suspend | PowerOption::Hibernate => {
      let action = match option {
        PowerOption::Suspend => "suspend",
        PowerOption::Hibernate => "hibernate",
        _ => unreachable!(),
      };

      match login_manager(root) {
        Some(LoginManager::Systemd) => Some(CommandLine::direct("systemctl", &[action])),
        Some(LoginManager::Elogind) => Some(CommandLine::direct("loginctl", &[action])),
        None => None,
      }
    },
  }
}

pub fn power(greeter: &mut Greeter, option: PowerOption) -> Option<Control> {
  if greeter.mock {
    return Some(Control::Exit(AuthStatus::Cancel));
  }

  let request = match greeter.powers.options.iter().find(|opt| opt.action == option) {
    None => None,

    Some(Power {
      command: Some(args), ..
    }) => Some(PowerRequest::new(args.clone(), greeter.power_setsid)),

    Some(_) => None,
  };

  if let Some(request) = request {
    Some(Control::PowerCommand(request))
  } else {
    greeter.message = Some(text!(greeter, command_missing));
    None
  }
}

#[derive(Clone, Copy, Debug)]
struct PowerTimings {
  command_timeout: Duration,
  term_grace: Duration,
  kill_reap_timeout: Duration,
}

impl Default for PowerTimings {
  fn default() -> Self {
    Self {
      command_timeout: COMMAND_TIMEOUT,
      term_grace: TERM_GRACE,
      kill_reap_timeout: KILL_REAP_TIMEOUT,
    }
  }
}

#[derive(Debug)]
pub enum PowerFailure {
  Spawn(String),
  Wait(String),
  Exit(ExitStatus),
  Timeout(Duration),
  Worker(String),
}

#[derive(Debug)]
pub enum PowerOutcome {
  Success,
  Failed(PowerFailure),
  Cancelled,
}

#[derive(Debug)]
pub struct PowerAlreadyRunning;

impl fmt::Display for PowerAlreadyRunning {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str("a power command is already running")
  }
}

pub struct PowerSupervisor {
  active: Option<ActivePower>,
  timings: PowerTimings,
}

struct ActivePower {
  cancel: Option<oneshot::Sender<()>>,
  worker: JoinHandle<PowerOutcome>,
}

impl PowerSupervisor {
  pub fn new() -> Self {
    Self {
      active: None,
      timings: PowerTimings::default(),
    }
  }

  #[cfg(test)]
  fn with_timings(timings: PowerTimings) -> Self {
    Self { active: None, timings }
  }

  pub fn has_active(&self) -> bool {
    self.active.is_some()
  }

  pub fn start(&mut self, request: PowerRequest) -> Result<(), PowerAlreadyRunning> {
    if self.has_active() {
      return Err(PowerAlreadyRunning);
    }

    let (cancel, cancellation) = oneshot::channel();
    let timings = self.timings;
    let worker = tokio::spawn(run_command(request, cancellation, timings));
    self.active = Some(ActivePower {
      cancel: Some(cancel),
      worker,
    });
    Ok(())
  }

  pub fn cancel(&mut self) -> bool {
    self
      .active
      .as_mut()
      .and_then(|active| active.cancel.take())
      .is_some_and(|cancel| cancel.send(()).is_ok())
  }

  pub async fn next(&mut self) -> PowerOutcome {
    let Some(active) = self.active.as_mut() else {
      return PowerOutcome::Failed(PowerFailure::Worker("power supervisor has no active worker".into()));
    };

    let result = (&mut active.worker).await;
    self.active = None;
    join_outcome(result)
  }

  pub async fn shutdown(&mut self) {
    if self.has_active() {
      self.cancel();
      let shutdown_timeout = self
        .timings
        .term_grace
        .saturating_add(self.timings.kill_reap_timeout)
        .saturating_add(Duration::from_millis(250));
      if timeout(shutdown_timeout, self.next()).await.is_err()
        && let Some(mut active) = self.active.take()
      {
        tracing::warn!("power command worker did not stop during shutdown; aborting it");
        active.worker.abort();
        let _ = timeout(Duration::from_millis(100), &mut active.worker).await;
      }
    }
  }
}

impl Drop for PowerSupervisor {
  fn drop(&mut self) {
    if let Some(active) = self.active.as_mut() {
      if let Some(cancel) = active.cancel.take() {
        let _ = cancel.send(());
      }
      active.worker.abort();
    }
  }
}

fn join_outcome(result: Result<PowerOutcome, JoinError>) -> PowerOutcome {
  match result {
    Ok(outcome) => outcome,
    Err(error) => PowerOutcome::Failed(PowerFailure::Worker(error.to_string())),
  }
}

#[derive(Clone, Copy)]
struct ProcessTarget {
  pid: Pid,
  group: bool,
}

impl ProcessTarget {
  fn signal(self, signal: Signal) -> Result<(), Errno> {
    let result = if self.group {
      killpg(self.pid, signal)
    } else {
      kill(self.pid, signal)
    };
    match result {
      Ok(()) | Err(Errno::ESRCH) => Ok(()),
      Err(error) => Err(error),
    }
  }

  fn exists(self) -> Result<bool, Errno> {
    let result = if self.group {
      killpg(self.pid, None)
    } else {
      kill(self.pid, None)
    };
    match result {
      Ok(()) => Ok(true),
      Err(Errno::ESRCH) => Ok(false),
      Err(error) => Err(error),
    }
  }
}

struct ProcessGuard {
  target: ProcessTarget,
  armed: bool,
}

impl ProcessGuard {
  fn new(target: ProcessTarget) -> Self {
    Self { target, armed: true }
  }

  fn disarm(&mut self) {
    self.armed = false;
  }
}

impl Drop for ProcessGuard {
  fn drop(&mut self) {
    if self.armed {
      let _ = self.target.signal(Signal::SIGKILL);
    }
  }
}

async fn run_command(
  request: PowerRequest,
  mut cancellation: oneshot::Receiver<()>,
  timings: PowerTimings,
) -> PowerOutcome {
  tracing::info!("executing configured power command");

  let mut command = Command::new(request.command.program());
  command.args(request.command.arguments());
  command.stdin(Stdio::null());
  command.stdout(Stdio::null());
  command.stderr(Stdio::null());
  command.kill_on_drop(true);

  if request.setsid {
    // SAFETY: after fork and before exec, this closure performs exactly one
    // async-signal-safe system call and does not allocate or take locks.
    unsafe {
      command
        .as_std_mut()
        .pre_exec(|| setsid().map(|_| ()).map_err(io::Error::from));
    }
  }

  let mut child = match command.spawn() {
    Ok(child) => child,
    Err(error) => return PowerOutcome::Failed(PowerFailure::Spawn(error.to_string())),
  };
  let Some(raw_pid) = child.id() else {
    return PowerOutcome::Failed(PowerFailure::Spawn(
      "spawned power command did not expose a process ID".into(),
    ));
  };
  let Ok(raw_pid) = i32::try_from(raw_pid) else {
    return PowerOutcome::Failed(PowerFailure::Spawn("power command process ID is out of range".into()));
  };
  let target = ProcessTarget {
    pid: Pid::from_raw(raw_pid),
    group: request.setsid,
  };
  let mut guard = ProcessGuard::new(target);

  let deadline = Instant::now() + timings.command_timeout;
  enum Completion {
    Status(io::Result<ExitStatus>),
    Cancelled,
    TimedOut,
  }
  let completion = tokio::select! {
    biased;
    cancellation = &mut cancellation => {
      let _ = cancellation;
      Completion::Cancelled
    },
    status = child.wait() => Completion::Status(status),
    _ = sleep_until(deadline) => Completion::TimedOut,
  };

  match completion {
    Completion::Status(Ok(status)) => {
      if status.success() {
        guard.disarm();
        tracing::info!("power command completed successfully");
        PowerOutcome::Success
      } else {
        if guard.target.group {
          terminate(&mut child, &mut guard, timings, true).await;
        } else {
          guard.disarm();
        }
        tracing::warn!("power command exited unsuccessfully: {status}");
        PowerOutcome::Failed(PowerFailure::Exit(status))
      }
    },
    Completion::Status(Err(error)) => {
      let error = error.to_string();
      terminate(&mut child, &mut guard, timings, false).await;
      PowerOutcome::Failed(PowerFailure::Wait(error))
    },
    Completion::Cancelled => {
      tracing::info!("cancelling power command");
      terminate(&mut child, &mut guard, timings, false).await;
      PowerOutcome::Cancelled
    },
    Completion::TimedOut => {
      tracing::warn!("power command timed out after {:?}", timings.command_timeout);
      terminate(&mut child, &mut guard, timings, false).await;
      PowerOutcome::Failed(PowerFailure::Timeout(timings.command_timeout))
    },
  }
}

async fn terminate(child: &mut Child, guard: &mut ProcessGuard, timings: PowerTimings, already_reaped: bool) {
  if already_reaped && guard.target.group {
    match guard.target.exists() {
      Ok(false) => {
        guard.disarm();
        return;
      },
      Ok(true) => {},
      Err(error) => tracing::warn!("failed to inspect exited power command group: {error}"),
    }
  }

  if let Err(error) = guard.target.signal(Signal::SIGTERM) {
    tracing::warn!("failed to terminate power command: {error}");
  }

  let grace_deadline = Instant::now() + timings.term_grace;
  let mut reaped = if already_reaped {
    true
  } else {
    match timeout(timings.term_grace, child.wait()).await {
      Ok(Ok(_)) => true,
      Ok(Err(error)) => {
        tracing::warn!("failed to reap terminated power command: {error}");
        false
      },
      Err(_) => false,
    }
  };

  // Reaping the session leader does not prove that every descendant in its
  // process group has exited. Give the entire group the configured grace
  // period before checking it and escalating.
  if guard.target.group {
    sleep_until(grace_deadline).await;
  } else if reaped {
    // Once an individual child has been reaped, its numeric PID can be reused.
    // Never probe or signal that stale identifier.
    guard.disarm();
    return;
  }

  let target_alive = match guard.target.exists() {
    Ok(alive) => alive,
    Err(error) => {
      tracing::warn!("failed to inspect terminated power command: {error}");
      true
    },
  };
  let kill_secured = if target_alive {
    match guard.target.signal(Signal::SIGKILL) {
      Ok(()) => true,
      Err(error) => {
        tracing::warn!("failed to kill power command: {error}");
        false
      },
    }
  } else {
    true
  };

  if !reaped {
    match timeout(timings.kill_reap_timeout, child.wait()).await {
      Ok(Ok(_)) => reaped = true,
      Ok(Err(error)) => tracing::warn!("failed to reap killed power command: {error}"),
      Err(_) => tracing::warn!("timed out reaping killed power command"),
    }
  }

  if (guard.target.group && kill_secured) || (!guard.target.group && reaped) {
    // A group is safe once it has disappeared or accepted SIGKILL. An
    // individual process is safe only after its child handle has been reaped.
    guard.disarm();
  }
}

#[cfg(test)]
mod tests {
  use std::{
    fs::{self, File},
    path::Path,
    time::Duration,
  };

  use tempfile::tempdir;

  use super::*;
  use crate::event::{Events, fill_event_queue};

  fn request(argv: &[&str], setsid: bool) -> PowerRequest {
    PowerRequest::new(
      CommandLine::from_argv(argv.iter().map(|value| (*value).to_string()).collect()).unwrap(),
      setsid,
    )
  }

  fn short_timings(command_timeout: Duration) -> PowerTimings {
    PowerTimings {
      command_timeout,
      term_grace: Duration::from_millis(50),
      kill_reap_timeout: Duration::from_millis(200),
    }
  }

  async fn wait_for_path(path: &Path) {
    timeout(Duration::from_secs(1), async {
      while !path.exists() {
        tokio::time::sleep(Duration::from_millis(5)).await;
      }
    })
    .await
    .expect("power command did not create its readiness marker");
  }

  fn process_is_running(pid: Pid) -> bool {
    let Ok(stat) = fs::read_to_string(format!("/proc/{}/stat", pid.as_raw())) else {
      return false;
    };
    let Some((_, fields)) = stat.rsplit_once(") ") else {
      return true;
    };
    !matches!(fields.as_bytes().first(), Some(b'Z' | b'X'))
  }

  async fn wait_for_process_to_stop(pid: Pid) {
    timeout(Duration::from_secs(1), async {
      while process_is_running(pid) {
        tokio::time::sleep(Duration::from_millis(5)).await;
      }
    })
    .await
    .expect("power command process was left running");
  }

  #[test]
  fn detects_elogind_from_its_pid_file() {
    let root = tempdir().unwrap();
    fs::create_dir_all(root.path().join("run/systemd/system")).unwrap();
    fs::create_dir_all(root.path().join("proc/42")).unwrap();
    fs::write(root.path().join("run/elogind.pid"), "42\n").unwrap();
    fs::write(root.path().join("proc/42/comm"), "elogind-daemon\n").unwrap();

    assert_eq!(login_manager(root.path()), Some(LoginManager::Elogind));
    assert_eq!(
      default_command_for(PowerOption::Suspend, root.path()).unwrap().argv(),
      ["loginctl", "suspend"]
    );
    assert_eq!(
      default_command_for(PowerOption::Hibernate, root.path()).unwrap().argv(),
      ["loginctl", "hibernate"]
    );
  }

  #[test]
  fn detects_systemd_from_its_runtime_directory() {
    let root = tempdir().unwrap();
    fs::create_dir_all(root.path().join("run/systemd/system")).unwrap();

    assert_eq!(login_manager(root.path()), Some(LoginManager::Systemd));
    assert_eq!(
      default_command_for(PowerOption::Suspend, root.path()).unwrap().argv(),
      ["systemctl", "suspend"]
    );
    assert_eq!(
      default_command_for(PowerOption::Hibernate, root.path()).unwrap().argv(),
      ["systemctl", "hibernate"]
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

  #[test]
  fn parses_shell_quoted_text_into_literal_arguments() {
    let command = CommandLine::parse("program 'two words' \"\" '$HOME' '|' ").unwrap();

    assert_eq!(command.argv(), ["program", "two words", "", "$HOME", "|"]);
    assert!(CommandLine::parse("").is_err());
    assert!(CommandLine::parse("program 'unterminated").is_err());
    assert!(CommandLine::parse("program \0").is_err());
  }

  #[test]
  fn power_command_semantics_distinguish_auto_disabled_and_explicit() {
    assert_eq!(PowerCommand::Auto.resolve(PowerOption::Shutdown).unwrap().argv(), [
      "shutdown", "-h", "now"
    ]);
    assert_eq!(PowerCommand::Disabled.resolve(PowerOption::Shutdown), None);

    let explicit = CommandLine::from_argv(vec!["custom".into(), "two words".into()]).unwrap();
    assert_eq!(
      PowerCommand::Explicit(explicit.clone()).resolve(PowerOption::Shutdown),
      Some(explicit)
    );
  }

  #[tokio::test]
  async fn mock_power_does_not_block_on_a_full_event_queue() {
    let events = Events::testing().await;
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
    let events = Events::testing().await;
    fill_event_queue(&events);

    let mut greeter = Greeter::default();
    greeter.powers.options.push(Power {
      action: PowerOption::Shutdown,
      label: "Shutdown".into(),
      command: Some(
        CommandLine::from_argv(
          ["shutdown", "two words", "$HOME", "|", "touch"]
            .map(str::to_string)
            .to_vec(),
        )
        .unwrap(),
      ),
    });

    let control = tokio::time::timeout(Duration::from_millis(100), async {
      power(&mut greeter, PowerOption::Shutdown)
    })
    .await
    .expect("real power action blocked on the full render/event queue");

    let Some(Control::PowerCommand(request)) = control else {
      panic!("power selection did not return its command to the main loop");
    };
    assert_eq!(request.command().argv(), [
      "shutdown",
      "two words",
      "$HOME",
      "|",
      "touch"
    ]);
    assert!(!request.uses_setsid());
  }

  #[test]
  fn setsid_is_part_of_the_request_without_reparsing_the_command() {
    let mut greeter = Greeter::default();
    greeter.power_setsid = true;
    greeter.powers.options.push(Power {
      action: PowerOption::Shutdown,
      label: "Shutdown".into(),
      command: Some(CommandLine::from_argv(vec!["program".into(), "two words".into()]).unwrap()),
    });

    let Some(Control::PowerCommand(request)) = power(&mut greeter, PowerOption::Shutdown) else {
      panic!("power selection did not return its command to the main loop");
    };
    assert_eq!(request.command().argv(), ["program", "two words"]);
    assert!(request.uses_setsid());
  }

  #[tokio::test]
  async fn supervisor_reports_success() {
    let mut supervisor = PowerSupervisor::with_timings(short_timings(Duration::from_secs(1)));
    supervisor.start(request(&["/bin/true"], false)).unwrap();

    assert!(matches!(supervisor.next().await, PowerOutcome::Success));
    assert!(!supervisor.has_active());
  }

  #[tokio::test]
  async fn supervisor_reports_nonzero_exit_without_stderr_capture() {
    let mut supervisor = PowerSupervisor::with_timings(short_timings(Duration::from_secs(1)));
    supervisor.start(request(&["/bin/sh", "-c", "exit 7"], false)).unwrap();

    let PowerOutcome::Failed(PowerFailure::Exit(status)) = supervisor.next().await else {
      panic!("nonzero command did not report its exit status");
    };
    assert_eq!(status.code(), Some(7));
  }

  #[tokio::test]
  async fn supervisor_reports_spawn_failure() {
    let mut supervisor = PowerSupervisor::with_timings(short_timings(Duration::from_secs(1)));
    supervisor
      .start(request(&["/path/that/does/not/exist/tuigreet-power"], false))
      .unwrap();

    assert!(matches!(
      supervisor.next().await,
      PowerOutcome::Failed(PowerFailure::Spawn(_))
    ));
  }

  #[tokio::test]
  async fn supervisor_times_out_and_reaps_the_command() {
    let limit = Duration::from_millis(40);
    let mut supervisor = PowerSupervisor::with_timings(short_timings(limit));
    supervisor.start(request(&["/bin/sleep", "60"], false)).unwrap();

    assert!(matches!(
      supervisor.next().await,
      PowerOutcome::Failed(PowerFailure::Timeout(actual)) if actual == limit
    ));
    assert!(!supervisor.has_active());
  }

  #[tokio::test]
  async fn supervisor_cancels_and_reaps_the_command() {
    let mut supervisor = PowerSupervisor::with_timings(short_timings(Duration::from_secs(1)));
    supervisor.start(request(&["/bin/sleep", "60"], false)).unwrap();
    tokio::task::yield_now().await;

    assert!(supervisor.cancel());
    assert!(!supervisor.cancel());
    assert!(matches!(supervisor.next().await, PowerOutcome::Cancelled));
    assert!(!supervisor.has_active());
  }

  #[tokio::test]
  async fn shutdown_cancels_and_joins_the_worker_with_a_bound() {
    let timings = short_timings(Duration::from_secs(60));
    let mut supervisor = PowerSupervisor::with_timings(timings);
    supervisor
      .start(request(&["/bin/sh", "-c", "trap '' TERM; while :; do :; done"], false))
      .unwrap();
    tokio::task::yield_now().await;

    timeout(Duration::from_secs(1), supervisor.shutdown())
      .await
      .expect("power supervisor shutdown was not bounded");
    assert!(!supervisor.has_active());
  }

  #[tokio::test]
  async fn supervisor_rejects_a_second_command() {
    let mut supervisor = PowerSupervisor::with_timings(short_timings(Duration::from_secs(1)));
    supervisor.start(request(&["/bin/sleep", "60"], false)).unwrap();

    assert!(supervisor.start(request(&["/bin/true"], false)).is_err());
    assert!(supervisor.cancel());
    assert!(matches!(supervisor.next().await, PowerOutcome::Cancelled));
  }

  #[tokio::test]
  async fn waiting_for_an_outcome_is_cancel_safe() {
    let mut supervisor = PowerSupervisor::with_timings(short_timings(Duration::from_secs(2)));
    supervisor.start(request(&["/bin/sleep", "60"], false)).unwrap();

    assert!(timeout(Duration::from_millis(10), supervisor.next()).await.is_err());
    assert!(supervisor.has_active());
    assert!(supervisor.cancel());
    assert!(matches!(supervisor.next().await, PowerOutcome::Cancelled));
  }

  #[tokio::test]
  async fn dropping_the_supervisor_kills_the_active_process_group() {
    let directory = tempdir().unwrap();
    let ready = directory.path().join("ready");
    let leader_file = directory.path().join("leader");
    let descendant_file = directory.path().join("descendant");
    let script = format!(
      "trap '' TERM; (trap '' TERM; while :; do :; done) & printf '%s' $! > \"{}\"; \
       printf '%s' $$ > \"{}\"; printf ready > \"{}\"; wait",
      descendant_file.display(),
      leader_file.display(),
      ready.display()
    );
    let mut supervisor = PowerSupervisor::with_timings(short_timings(Duration::from_secs(60)));
    supervisor
      .start(PowerRequest::new(
        CommandLine::from_argv(vec!["/bin/sh".into(), "-c".into(), script]).unwrap(),
        true,
      ))
      .unwrap();
    wait_for_path(&ready).await;
    let leader = fs::read_to_string(leader_file).unwrap();
    let leader = Pid::from_raw(leader.parse().unwrap());
    let descendant = fs::read_to_string(descendant_file).unwrap();
    let descendant = Pid::from_raw(descendant.parse().unwrap());

    drop(supervisor);

    wait_for_process_to_stop(leader).await;
    wait_for_process_to_stop(descendant).await;
  }

  #[tokio::test]
  async fn cancellation_reaches_and_cleans_up_process_group_descendants() {
    let directory = tempdir().unwrap();
    let ready = directory.path().join("ready");
    let terminated = directory.path().join("terminated");
    let descendant = directory.path().join("descendant");
    let script = format!(
      "trap '' TERM; (trap 'printf terminated > \"{}\"' TERM; printf ready > \"{}\"; \
       while :; do :; done) & printf '%s' $! > \"{}\"; wait",
      terminated.display(),
      ready.display(),
      descendant.display()
    );
    let mut supervisor = PowerSupervisor::with_timings(short_timings(Duration::from_secs(2)));
    supervisor
      .start(PowerRequest::new(
        CommandLine::from_argv(vec!["/bin/sh".into(), "-c".into(), script]).unwrap(),
        true,
      ))
      .unwrap();
    wait_for_path(&ready).await;

    assert!(supervisor.cancel());
    assert!(matches!(supervisor.next().await, PowerOutcome::Cancelled));
    assert!(terminated.exists(), "SIGTERM did not reach the command's descendant");

    let pid = fs::read_to_string(descendant).unwrap();
    let pid = Pid::from_raw(pid.parse().unwrap());
    wait_for_process_to_stop(pid).await;
  }
}
