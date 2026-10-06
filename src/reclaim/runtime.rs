//! Bounded subprocess execution shared by reclaim adapters.
use anyhow::{Context, Result};
use clap::Args;
use std::cell::Cell;
use std::collections::VecDeque;
use std::io::{IsTerminal, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const LOG_LIMIT: usize = 32 * 1024;
static INTERRUPTED: AtomicBool = AtomicBool::new(false);
thread_local! {
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

#[derive(Debug, Clone, Args)]
pub struct Limits {
    /// Reclaim runtime budget in seconds (including privilege checks)
    #[arg(long, default_value_t = 1200, value_parser = clap::value_parser!(u64).range(1..=86400))]
    pub timeout_seconds: u64,
    /// Total Nix logical-byte GC budget; physical filesystem gain is measured separately
    #[arg(long, default_value_t = 32 * 1024 * 1024 * 1024, value_parser = clap::value_parser!(u64).range(1..))]
    pub gc_max_bytes: u64,
    /// Maximum logical bytes requested per Nix GC pass
    #[arg(long, default_value_t = 4 * 1024 * 1024 * 1024, value_parser = clap::value_parser!(u64).range(1..))]
    pub gc_pass_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout_seconds: 1200,
            gc_max_bytes: 32 << 30,
            gc_pass_bytes: 4 << 30,
        }
    }
}

pub struct Runtime {
    pub deadline: Instant,
    pub remaining_gc_bytes: Cell<u64>,
}

impl Runtime {
    pub fn new(limits: &Limits) -> Self {
        Self {
            deadline: Instant::now() + Duration::from_secs(limits.timeout_seconds),
            remaining_gc_bytes: Cell::new(limits.gc_max_bytes),
        }
    }

    pub fn stop_reason(&self) -> Option<&'static str> {
        stop_reason(self.deadline)
    }
}

pub struct ActionContext<'a> {
    pub mount: Option<&'a str>,
    pub required_available_bytes: Option<u64>,
    pub limits: &'a Limits,
    pub runtime: &'a Runtime,
}

impl ActionContext<'_> {
    pub fn goal_met(&self) -> Result<bool> {
        match (self.mount, self.required_available_bytes) {
            (Some(path), Some(required)) => {
                Ok(crate::mount::stats(path)?.available_bytes >= required)
            }
            _ => Ok(false),
        }
    }
}

extern "C" fn interrupted(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

/// Keep the parent alive long enough to cancel its child and print the partial report.
pub struct SignalGuard {
    previous: [(libc::c_int, libc::sigaction); 2],
}

impl SignalGuard {
    pub fn install() -> Result<Self> {
        INTERRUPTED.store(false, Ordering::Relaxed);
        // SAFETY: zero is a valid initial sigaction; the handler only sets an atomic flag.
        let mut previous = unsafe {
            [
                (libc::SIGINT, std::mem::zeroed()),
                (libc::SIGTERM, std::mem::zeroed()),
            ]
        };
        for index in 0..previous.len() {
            // SAFETY: all pointers refer to initialized, live sigaction objects.
            let result = unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = interrupted as *const () as usize;
                libc::sigemptyset(&mut action.sa_mask);
                libc::sigaction(previous[index].0, &action, &mut previous[index].1)
            };
            if result != 0 {
                for (signal, old) in &previous[..index] {
                    // SAFETY: restoring a handler returned by sigaction.
                    unsafe {
                        libc::sigaction(*signal, old, std::ptr::null_mut());
                    }
                }
                return Err(std::io::Error::last_os_error().into());
            }
        }
        Ok(Self { previous })
    }
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        for (signal, old) in &self.previous {
            // SAFETY: restoring a handler returned by sigaction.
            unsafe {
                libc::sigaction(*signal, old, std::ptr::null_mut());
            }
        }
    }
}

fn stop_reason(deadline: Instant) -> Option<&'static str> {
    if INTERRUPTED.load(Ordering::Relaxed) {
        Some("reclaim interrupted")
    } else if Instant::now() >= deadline {
        Some("reclaim runtime budget exhausted")
    } else {
        None
    }
}

pub fn with_deadline<T>(deadline: Instant, action: impl FnOnce() -> T) -> T {
    struct Restore(Option<Instant>);
    impl Drop for Restore {
        fn drop(&mut self) {
            DEADLINE.set(self.0);
        }
    }
    let _restore = Restore(DEADLINE.replace(Some(deadline)));
    action()
}

pub fn current_deadline() -> Option<Instant> {
    DEADLINE.get()
}

#[derive(Default)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
}

struct Captured {
    text: String,
    truncated: bool,
}

fn drain(mut reader: impl Read, stream: bool) -> std::io::Result<Captured> {
    let mut tail = VecDeque::with_capacity(LOG_LIMIT);
    let mut truncated = false;
    let mut buffer = [0; 4096];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        if stream {
            let _ = std::io::stderr().write_all(&buffer[..count]);
        }
        for byte in &buffer[..count] {
            if tail.len() == LOG_LIMIT {
                tail.pop_front();
                truncated = true;
            }
            tail.push_back(*byte);
        }
    }
    Ok(Captured {
        text: String::from_utf8_lossy(tail.make_contiguous()).into_owned(),
        truncated,
    })
}

pub fn run_command(args: &[&str], deadline: Instant) -> Result<CommandOutput> {
    run_command_inner(args, deadline, true)
}

pub fn run_capture(args: &[&str], deadline: Instant) -> Result<CommandOutput> {
    run_command_inner(args, deadline, false)
}

fn run_command_inner(
    args: &[&str],
    deadline: Instant,
    stream_stdout: bool,
) -> Result<CommandOutput> {
    let (program, args) = args.split_first().context("empty reclaim command")?;
    if let Some(reason) = stop_reason(deadline) {
        anyhow::bail!("{reason}");
    }
    eprintln!("reclaim: running {program} {}", args.join(" "));
    // An interactive sudo credential check needs the caller's foreground terminal
    // group. It has no cleanup descendants; all cleanup commands get their own group.
    let own_group = !(*program == "sudo" && args == ["-v"] && std::io::stdin().is_terminal());
    let mut command = Command::new(program);
    command
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if own_group {
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to execute {program}"))?;
    let stdout = child.stdout.take().context("missing child stdout")?;
    let stderr = child.stderr.take().context("missing child stderr")?;
    let out = std::thread::spawn(move || drain(stdout, stream_stdout));
    let err = std::thread::spawn(move || drain(stderr, true));
    let mut cancellation = None;
    let mut cancelled_at = None;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if cancelled_at.is_none() {
            if let Some(reason) = stop_reason(deadline) {
                cancellation = Some(reason);
                cancelled_at = Some(Instant::now());
                signal_child(child.id(), libc::SIGINT, own_group);
            }
        } else if cancelled_at.is_some_and(|start| start.elapsed() >= Duration::from_secs(3)) {
            signal_child(child.id(), libc::SIGKILL, own_group);
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    // Descendants must not retain our pipes after the command exits or is cancelled.
    if own_group {
        signal_child(child.id(), libc::SIGKILL, true);
    }
    let stdout = out
        .join()
        .map_err(|_| anyhow::anyhow!("stdout reader panicked"))??;
    let stderr = err
        .join()
        .map_err(|_| anyhow::anyhow!("stderr reader panicked"))??;
    let stdout_truncated = stdout.truncated;
    let stdout = stdout.text;
    let stderr = stderr.text;
    if let Some(reason) = cancellation {
        anyhow::bail!(
            "{reason}; {program} cancelled\nstdout tail: {stdout}\nstderr tail: {stderr}"
        );
    }
    anyhow::ensure!(
        status.success(),
        "command {program} {args:?} failed ({status})\nstdout tail: {stdout}\nstderr tail: {stderr}"
    );
    Ok(CommandOutput {
        stdout,
        stderr,
        stdout_truncated,
    })
}

fn signal_child(pid: u32, signal: libc::c_int, group: bool) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: the negative pid selects the subprocess group we created above.
        unsafe {
            libc::kill(if group { -pid } else { pid }, signal);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_cancels_process_group_and_retains_partial_output() {
        let started = Instant::now();
        let error = run_command(
            &["sh", "-c", "echo started; sleep 30 & wait"],
            started + Duration::from_millis(100),
        )
        .err()
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        let message = error.to_string();
        assert!(message.contains("runtime budget exhausted"));
        assert!(message.contains("started"));
    }

    #[test]
    fn captured_output_is_bounded_and_exit_failures_are_preserved() {
        let error = run_command(
            &["sh", "-c", "printf failure >&2; exit 7"],
            Instant::now() + Duration::from_secs(2),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("exit status: 7"));
        assert!(error.to_string().contains("failure"));
        let text = drain(std::io::Cursor::new(vec![b'x'; LOG_LIMIT * 2]), false).unwrap();
        assert_eq!(text.text.len(), LOG_LIMIT);
        assert!(text.truncated);
    }

    #[test]
    fn semantic_stdout_is_never_silently_truncated() {
        let error = with_deadline(Instant::now() + Duration::from_secs(2), || {
            crate::exec::run(&["sh", "-c", "head -c 40000 /dev/zero"])
        })
        .unwrap_err();
        assert!(error.to_string().contains("stdout exceeded"));
    }
}
