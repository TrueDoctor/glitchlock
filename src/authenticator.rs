use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use crate::helper_message::HelperMessage;
use crate::secret::Secret;

/// Helpers that exit sooner than this without a password are considered broken.
const QUICK_EXIT: Duration = Duration::from_secs(2);
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Debug, PartialEq)]
pub enum AuthenticationEvent {
    Unlocked,
    Rejected,
}

/// Runs one PAM helper process per service in parallel and restarts them when they exit.
pub struct Authenticator {
    executable: PathBuf,
    helpers: Vec<Helper>,
    queued_password: Option<Secret>,
}

struct Helper {
    service: String,
    state: HelperState,
    backoff: Duration,
}

enum HelperState {
    Running(RunningHelper),
    Waiting { restart_at: Instant },
}

struct RunningHelper {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    pending_output: Vec<u8>,
    started: Instant,
    awaits_password: bool,
    received_password: bool,
}

impl Authenticator {
    pub fn start(services: &[String]) -> Result<Self, String> {
        let executable = std::env::current_exe()
            .map_err(|error| format!("cannot find own executable: {error}"))?;
        let now = Instant::now();
        let helpers = services
            .iter()
            .map(|service| Helper {
                service: service.clone(),
                state: HelperState::Waiting { restart_at: now },
                backoff: Duration::ZERO,
            })
            .collect();

        let mut authenticator = Self {
            executable,
            helpers,
            queued_password: None,
        };
        authenticator.restart_due_helpers(now);
        Ok(authenticator)
    }

    /// The stdout file descriptors of all running helpers, paired with their helper index.
    pub fn output_fds(&self) -> Vec<(usize, RawFd)> {
        self.helpers
            .iter()
            .enumerate()
            .filter_map(|(index, helper)| match &helper.state {
                HelperState::Running(running) => Some((index, running.stdout.as_raw_fd())),
                HelperState::Waiting { .. } => None,
            })
            .collect()
    }

    pub fn next_restart(&self) -> Option<Instant> {
        self.helpers
            .iter()
            .filter_map(|helper| match helper.state {
                HelperState::Waiting { restart_at } => Some(restart_at),
                HelperState::Running(_) => None,
            })
            .min()
    }

    pub fn restart_due_helpers(&mut self, now: Instant) {
        for helper in &mut self.helpers {
            if let HelperState::Waiting { restart_at } = helper.state
                && restart_at <= now
            {
                helper.state = match spawn(&self.executable, &helper.service) {
                    Ok(running) => HelperState::Running(running),
                    Err(error) => {
                        eprintln!(
                            "glitchlock: cannot start PAM helper for {}: {error}",
                            helper.service
                        );
                        helper.backoff = next_backoff(helper.backoff);
                        HelperState::Waiting {
                            restart_at: now + helper.backoff,
                        }
                    }
                };
            }
        }
    }

    /// Hands the password to a helper waiting for one, or keeps it until a helper asks.
    pub fn submit(&mut self, password: Secret) {
        let waiting_helper = self
            .helpers
            .iter_mut()
            .find_map(|helper| match &mut helper.state {
                HelperState::Running(running) if running.awaits_password => Some(running),
                _ => None,
            });

        match waiting_helper {
            Some(running) => {
                if !running.send_password(&password) {
                    self.queued_password = Some(password);
                }
            }
            None => self.queued_password = Some(password),
        }
    }

    /// Reads everything the helper at `index` has written and reacts to it.
    pub fn read_output(&mut self, index: usize, now: Instant) -> Vec<AuthenticationEvent> {
        let Some(helper) = self.helpers.get_mut(index) else {
            return Vec::new();
        };
        let HelperState::Running(running) = &mut helper.state else {
            return Vec::new();
        };

        let exited = running.read_available();
        let mut events = Vec::new();
        for message in running.take_messages() {
            match message {
                HelperMessage::Prompt => {
                    running.awaits_password = true;
                    if let Some(password) = self.queued_password.take()
                        && !running.send_password(&password)
                    {
                        self.queued_password = Some(password);
                    }
                }
                HelperMessage::Info(_) => {}
                HelperMessage::Error(_) => events.push(AuthenticationEvent::Rejected),
                HelperMessage::Success => events.push(AuthenticationEvent::Unlocked),
                HelperMessage::Failure if running.received_password => {
                    events.push(AuthenticationEvent::Rejected)
                }
                HelperMessage::Failure => {}
            }
        }

        if exited {
            helper.schedule_restart(now);
        }
        events
    }
}

impl Drop for Authenticator {
    fn drop(&mut self) {
        for helper in &mut self.helpers {
            if let HelperState::Running(running) = &mut helper.state {
                let _ = running.child.kill();
                let _ = running.child.wait();
            }
        }
    }
}

impl Helper {
    fn schedule_restart(&mut self, now: Instant) {
        let HelperState::Running(running) = &mut self.state else {
            return;
        };
        let _ = running.child.wait();

        let lived_long_enough = now.duration_since(running.started) >= QUICK_EXIT;
        self.backoff = if running.received_password || lived_long_enough {
            Duration::ZERO
        } else {
            next_backoff(self.backoff)
        };
        self.state = HelperState::Waiting {
            restart_at: now + self.backoff,
        };
    }
}

impl RunningHelper {
    /// Reads all available output, returning `true` once the helper has exited.
    fn read_available(&mut self) -> bool {
        let mut chunk = [0; 1024];
        loop {
            match self.stdout.read(&mut chunk) {
                Ok(0) => return true,
                Ok(length) => self
                    .pending_output
                    .extend_from_slice(chunk.get(..length).unwrap_or_default()),
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == ErrorKind::WouldBlock => return false,
                Err(_) => return true,
            }
        }
    }

    fn take_messages(&mut self) -> Vec<HelperMessage> {
        let Some(last_newline) = self.pending_output.iter().rposition(|byte| *byte == b'\n') else {
            return Vec::new();
        };
        let complete: Vec<u8> = self.pending_output.drain(..=last_newline).collect();

        String::from_utf8_lossy(&complete)
            .lines()
            .filter_map(HelperMessage::decode)
            .collect()
    }

    fn send_password(&mut self, password: &Secret) -> bool {
        let sent = self.stdin.write_all(password.as_bytes()).is_ok()
            && self.stdin.write_all(b"\n").is_ok();
        if sent {
            self.awaits_password = false;
            self.received_password = true;
        }
        sent
    }
}

fn spawn(executable: &PathBuf, service: &str) -> std::io::Result<RunningHelper> {
    let mut child = Command::new(executable)
        .args(["--pam-helper", service])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;

    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(std::io::Error::other("helper pipes are missing"));
    };
    set_nonblocking(&stdout)?;

    Ok(RunningHelper {
        child,
        stdin,
        stdout,
        pending_output: Vec::new(),
        started: Instant::now(),
        awaits_password: false,
        received_password: false,
    })
}

fn set_nonblocking(file: &impl AsRawFd) -> std::io::Result<()> {
    let fd = file.as_raw_fd();
    // SAFETY: `fd` is an open file descriptor owned by `file`.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    // SAFETY: As above.
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn next_backoff(backoff: Duration) -> Duration {
    (backoff * 2).clamp(MIN_BACKOFF, MAX_BACKOFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_within_bounds() {
        assert_eq!(next_backoff(Duration::ZERO), MIN_BACKOFF);
        assert_eq!(next_backoff(Duration::from_secs(4)), Duration::from_secs(8));
        assert_eq!(next_backoff(MAX_BACKOFF), MAX_BACKOFF);
    }
}
