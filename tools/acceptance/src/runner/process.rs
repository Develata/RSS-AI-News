//! Bounded child-process execution: output is captured with a byte cap and
//! every step has a wall-clock timeout.
//!
//! Invariants:
//! - Memory per step is O(`CAPTURE_LIMIT_BYTES`) per stream regardless of how
//!   much the child writes; the *last* bytes are kept, since failures are
//!   reported at the end of the output.
//! - A timed-out step is killed together with its descendants, best-effort
//!   (Linux: found by walking `ps --ppid`), so no orphan `rustc`/`cargo`
//!   keeps running.
//! - Children stay in the runner's process group, so a terminal Ctrl-C (sent
//!   to the foreground group) stops them together with the runner.
//! - `run_bounded` always reaps the child before returning. The pipe wait gets
//!   ~2 s of grace past the deadline (cleanup itself is not separately
//!   bounded), so the step returns even when a leftover background process keeps
//!   the child's output pipes open. The step then fails with `pipes_held` and
//!   keeps the output read so far; that leftover process is *not* killed (it
//!   was re-parented away from the tree), and its reader thread ends when it
//!   exits.
//! - Descendant discovery uses `ps --ppid` (procps, Linux). Elsewhere (macOS,
//!   Windows) only the direct child is killed on timeout.

use std::{
    io::{self, Read},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, RecvTimeoutError},
    },
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
    /// The child exited but its stdout/stderr stayed open past the deadline
    /// (a background process it started still holds them).
    pub(crate) pipes_held: bool,
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

    // A killed tree closes its pipes promptly; allow a short grace for that.
    let pipe_deadline = deadline.max(Instant::now()) + Duration::from_secs(2);
    let (stdout, stdout_held) = receive(stdout, pipe_deadline);
    let (stderr, stderr_held) = receive(stderr, pipe_deadline);
    Ok(Finished {
        exit_code: status.code(),
        timed_out,
        pipes_held: stdout_held || stderr_held,
        stdout,
        stderr,
    })
}

/// Bytes read so far from one stream, shared with its reader thread so a
/// step whose pipe is held open still reports what it printed.
#[derive(Default)]
struct Buffer {
    kept: Vec<u8>,
    truncated: bool,
}

impl Buffer {
    fn push(&mut self, bytes: &[u8]) {
        self.kept.extend_from_slice(bytes);
        // Amortised O(n): drain only once the buffer doubles.
        if self.kept.len() > 2 * CAPTURE_LIMIT_BYTES {
            self.trim();
        }
    }

    fn trim(&mut self) {
        if self.kept.len() > CAPTURE_LIMIT_BYTES {
            self.kept.drain(..self.kept.len() - CAPTURE_LIMIT_BYTES);
            self.truncated = true;
        }
    }

    fn snapshot(&mut self) -> Captured {
        self.trim();
        Captured {
            text: String::from_utf8_lossy(&self.kept).into_owned(),
            truncated: self.truncated,
        }
    }
}

struct Reader {
    buffer: Arc<Mutex<Buffer>>,
    done: Receiver<()>,
}

/// Waits for a reader to reach EOF until `deadline`, then snapshots what it
/// read; `true` when the pipe was still held open.
fn receive(reader: Option<Reader>, deadline: Instant) -> (Captured, bool) {
    let Some(reader) = reader else {
        return (Captured::default(), false);
    };
    let held = matches!(
        reader
            .done
            .recv_timeout(deadline.saturating_duration_since(Instant::now())),
        Err(RecvTimeoutError::Timeout)
    );
    let captured = match reader.buffer.lock() {
        Ok(mut buffer) => buffer.snapshot(),
        // A panicked reader still leaves consistent bytes behind.
        Err(poisoned) => poisoned.into_inner().snapshot(),
    };
    (captured, held)
}

fn spawn_reader(mut stream: impl Read + Send + 'static) -> Reader {
    let buffer = Arc::new(Mutex::new(Buffer::default()));
    let (done_tx, done) = mpsc::channel();
    let shared = Arc::clone(&buffer);
    thread::spawn(move || {
        let mut chunk = [0_u8; 8192];
        loop {
            match stream.read(&mut chunk) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Ok(0) | Err(_) => break,
                Ok(read) => match shared.lock() {
                    Ok(mut buffer) => buffer.push(&chunk[..read]),
                    Err(poisoned) => poisoned.into_inner().push(&chunk[..read]),
                },
            }
        }
        if done_tx.send(()).is_err() {
            // The receiver gave up (pipe held past the deadline) and already
            // took its snapshot; nothing is waiting for this signal.
        }
    });
    Reader { buffer, done }
}

fn kill_tree(child: &mut Child) {
    // Descendants first (deepest last in the list, killed first) while the
    // child is still alive, so none is re-parented away from the tree; repeat
    // to catch processes forked during the previous round.
    #[cfg(unix)]
    for _round in 0..5 {
        let pids = descendants(child.id());
        if pids.is_empty() {
            break;
        }
        for pid in pids.into_iter().rev() {
            match Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
            {
                // A non-zero status usually means the process already exited.
                Ok(_) => {}
                Err(error) => eprintln!("acceptance: failed to kill descendant {pid}: {error}"),
            }
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
    fn leftover_background_process_cannot_stall_the_step() {
        let mut command = Command::new("sh");
        // The child exits at once but its background `sleep` inherits stdout.
        command.args(["-c", "echo before; sleep 30 & exit 0"]);
        let started = std::time::Instant::now();
        let finished = run_bounded(command, Duration::from_millis(300)).unwrap();
        assert!(finished.pipes_held);
        // Output printed before the child exited is kept as evidence.
        assert_eq!(finished.stdout.text, "before\n");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

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
