//! Bounded child-process execution: output is captured with a byte cap and
//! every step has a wall-clock timeout.
//!
//! Invariants:
//! - Memory per step is O(`CAPTURE_LIMIT_BYTES`) per stream regardless of how
//!   much the child writes; the *last* bytes are kept, since failures are
//!   reported at the end of the output.
//! - A timed-out step is killed together with its descendants (Unix: found by
//!   walking `ps --ppid`), so no orphan `rustc`/`cargo` keeps running.
//! - Children stay in the runner's process group, so a terminal Ctrl-C (sent
//!   to the foreground group) stops them together with the runner.
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
    // Descendants first (deepest last in the list, killed first), so none is
    // re-parented and missed while its parent dies.
    #[cfg(unix)]
    for pid in descendants(child.id()).into_iter().rev() {
        let status = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Err(error) = status {
            eprintln!("acceptance: failed to kill descendant {pid}: {error}");
        }
    }
    if let Err(error) = child.kill() {
        eprintln!("acceptance: failed to kill timed-out child: {error}");
    }
}

/// All descendants of `root`, parents before children (`ps` from procps).
#[cfg(unix)]
fn descendants(root: u32) -> Vec<u32> {
    let mut found = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        let Ok(output) = Command::new("ps")
            .args(["-o", "pid=", "--ppid", &parent.to_string()])
            .output()
        else {
            break;
        };
        for pid in String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .filter_map(|pid| pid.parse::<u32>().ok())
        {
            found.push(pid);
            frontier.push(pid);
        }
    }
    found
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
    fn timeout_kills_descendants() {
        let dir = std::env::temp_dir().join(format!("acceptance-kill-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("grandchild.pid");
        let mut command = Command::new("sh");
        // The grandchild `sleep` would keep the pipe open if it survived.
        command.args([
            "-c",
            &format!("sleep 30 & echo $! > {}; wait", pid_file.display()),
        ]);
        let started = std::time::Instant::now();
        let finished = run_bounded(command, Duration::from_millis(500)).unwrap();
        assert!(finished.timed_out);
        assert!(started.elapsed() < Duration::from_secs(10));

        let grandchild = std::fs::read_to_string(&pid_file).unwrap();
        let alive = Command::new("kill")
            .args(["-0", grandchild.trim()])
            .status()
            .unwrap()
            .success();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(!alive, "grandchild {grandchild} survived the timeout");
    }
}
