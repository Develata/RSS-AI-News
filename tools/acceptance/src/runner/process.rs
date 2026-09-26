//! Bounded child-process execution: output is captured with a byte cap and
//! every step has a wall-clock timeout.
//!
//! Invariants:
//! - Memory per step is O(`CAPTURE_LIMIT_BYTES`) per stream regardless of how
//!   much the child writes; the *last* bytes are kept, since failures are
//!   reported at the end of the output.
//! - A timed-out step is killed together with its descendants (Unix: the child
//!   leads its own process group), so no orphan `rustc`/`cargo` keeps running.
//! - `run_bounded` always reaps the child before returning.

use std::{
    io::{self, Read},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

/// Bytes kept per stream. Contract checks parse small JSON documents; build
/// and test logs only need their tail for failure evidence.
pub(crate) const CAPTURE_LIMIT_BYTES: usize = 4 * 1024 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Default)]
pub(crate) struct Captured {
    /// Last `CAPTURE_LIMIT_BYTES` of the stream (lossy UTF-8).
    pub(crate) text: String,
    /// `true` when earlier bytes were dropped.
    pub(crate) truncated: bool,
}

#[derive(Debug)]
pub(crate) struct Finished {
    pub(crate) exit_code: Option<i32>,
    pub(crate) timed_out: bool,
    pub(crate) stdout: Captured,
    pub(crate) stderr: Captured,
}

pub(crate) fn run_bounded(mut command: Command, timeout: Duration) -> io::Result<Finished> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().map(spawn_reader);
    let stderr = child.stderr.take().map(spawn_reader);

    let deadline = Instant::now() + timeout;
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait()? {
            break (status, false);
        }
        if Instant::now() >= deadline {
            kill_tree(&mut child);
            break (child.wait()?, true);
        }
        thread::sleep(POLL_INTERVAL);
    };

    Ok(Finished {
        exit_code: status.code(),
        timed_out,
        stdout: join_reader(stdout),
        stderr: join_reader(stderr),
    })
}

fn spawn_reader(mut stream: impl Read + Send + 'static) -> thread::JoinHandle<Captured> {
    thread::spawn(move || {
        let mut kept = Vec::new();
        let mut truncated = false;
        let mut chunk = [0_u8; 8192];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    kept.extend_from_slice(&chunk[..read]);
                    // Amortised O(n): drain only once the buffer doubles.
                    if kept.len() > 2 * CAPTURE_LIMIT_BYTES {
                        kept.drain(..kept.len() - CAPTURE_LIMIT_BYTES);
                        truncated = true;
                    }
                }
            }
        }
        if kept.len() > CAPTURE_LIMIT_BYTES {
            kept.drain(..kept.len() - CAPTURE_LIMIT_BYTES);
            truncated = true;
        }
        Captured {
            text: String::from_utf8_lossy(&kept).into_owned(),
            truncated,
        }
    })
}

fn join_reader(handle: Option<thread::JoinHandle<Captured>>) -> Captured {
    handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default()
}

fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    {
        // The child leads its own process group (process_group(0) above), so
        // signalling -pid reaches cargo's rustc/linker descendants as well.
        let group = format!("-{}", child.id());
        if let Ok(status) = Command::new("kill")
            .args(["-KILL", "--", &group])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            && status.success()
        {
            return;
        }
    }
    // Fallback (non-Unix, or `kill` unavailable): at least stop the child.
    if let Err(error) = child.kill() {
        eprintln!("acceptance: failed to kill timed-out child: {error}");
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{process::Command, time::Duration};

    use super::{CAPTURE_LIMIT_BYTES, run_bounded};

    #[test]
    fn output_is_capped_to_the_tail() {
        let mut command = Command::new("sh");
        command.args(["-c", "head -c 9000000 /dev/zero | tr '\\0' x; printf END"]);
        let finished = run_bounded(command, Duration::from_secs(30)).unwrap();
        assert_eq!(finished.exit_code, Some(0));
        assert!(finished.stdout.truncated);
        assert_eq!(finished.stdout.text.len(), CAPTURE_LIMIT_BYTES);
        assert!(finished.stdout.text.ends_with("END"));
    }

    #[test]
    fn timeout_kills_the_process_group() {
        let mut command = Command::new("sh");
        // The grandchild `sleep` would keep the pipe open if it survived.
        command.args(["-c", "sleep 30 & sleep 30"]);
        let started = std::time::Instant::now();
        let finished = run_bounded(command, Duration::from_millis(300)).unwrap();
        assert!(finished.timed_out);
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
