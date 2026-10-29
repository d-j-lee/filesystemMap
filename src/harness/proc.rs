//! Run a tool with a deadline and bounded output. On expiry the whole process tree is
//! killed, so an orphaned `cargo` cannot keep holding the build directory lock. A
//! background process that outlives the tool and keeps its output open cannot hold the
//! caller either: the output readers get a short grace period and are then abandoned.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Bytes of output kept per stream (the tail; earlier output is dropped).
const KEEP: usize = 256 * 1024;
/// How long to wait for the output streams to close after the tool has ended.
const READER_GRACE: Duration = Duration::from_secs(2);

pub struct Outcome {
    /// Exit code; `None` when killed by a signal or on timeout.
    pub code: Option<i32>,
    pub timed_out: bool,
    /// The process could not be started (not installed, not executable).
    pub start_error: Option<String>,
    /// stdout followed by stderr, each possibly truncated at the front.
    pub output: String,
    pub ms: u64,
}

impl Outcome {
    pub fn success(&self) -> bool {
        self.code == Some(0) && !self.timed_out
    }
}

pub fn run(mut cmd: Command, deadline: Duration) -> Outcome {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let started = Instant::now();
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Outcome {
                code: None,
                timed_out: false,
                start_error: Some(e.to_string()),
                output: String::new(),
                ms: 0,
            };
        }
    };
    let (tx, rx) = mpsc::channel();
    let out = child.stdout.take();
    let err = child.stderr.take();
    let tx_err = tx.clone();
    std::thread::spawn(move || tx.send((0, read_tail(out))));
    std::thread::spawn(move || tx_err.send((1, read_tail(err))));
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if started.elapsed() >= deadline => {
                timed_out = true;
                kill_tree(&mut child);
                break child.wait().ok();
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break None,
        }
    };
    // Background processes the tool left in its group would keep the pipes open.
    #[cfg(unix)]
    kill_group(child.id());
    let mut parts = [String::new(), String::new()];
    let mut pending = 2;
    let until = Instant::now() + READER_GRACE;
    while pending > 0 {
        match rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok((i, text)) => {
                parts[i] = text;
                pending -= 1;
            }
            // A process outside the group (its own session) still holds a pipe: give up
            // on the rest of the output rather than wait for it.
            Err(_) => break,
        }
    }
    let mut output = parts.concat();
    if pending > 0 {
        output.push_str("\n[fsmap: output cut, a background process kept it open]\n");
    }
    Outcome {
        code: status.and_then(|s| s.code()),
        timed_out,
        start_error: None,
        output,
        ms: started.elapsed().as_millis() as u64,
    }
}

fn read_tail(stream: Option<impl Read>) -> String {
    let Some(mut s) = stream else {
        return String::new();
    };
    let mut kept: Vec<u8> = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    while let Ok(n) = s.read(&mut buf) {
        if n == 0 {
            break;
        }
        kept.extend_from_slice(&buf[..n]);
        if kept.len() > 2 * KEEP {
            kept.drain(..kept.len() - KEEP);
        }
    }
    if kept.len() > KEEP {
        kept.drain(..kept.len() - KEEP);
    }
    String::from_utf8_lossy(&kept).into_owned()
}

fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    kill_group(child.id());
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
}

/// Kill every process in the group the child leads (process_group(0) above). `--`
/// matters: procps `kill -KILL -<pgid>` reads the group id as an option and kills nothing.
#[cfg(unix)]
fn kill_group(id: u32) {
    let _ = Command::new("kill")
        .args(["-s", "KILL", "--", &format!("-{id}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn deadline_kills_the_whole_tree() {
        let marker = tempfile::tempdir().unwrap();
        let file = marker.path().join("late");
        // A grandchild that would write after the deadline.
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            &format!("(sleep 2; touch {}) & sleep 5", file.display()),
        ]);
        let o = run(cmd, Duration::from_millis(300));
        assert!(o.timed_out && !o.success());
        std::thread::sleep(Duration::from_millis(2500));
        assert!(!file.exists(), "grandchild survived the kill");
    }

    /// Review finding (final review M4): a background grandchild that keeps the output
    /// pipe open must not hold the caller past the deadline.
    #[test]
    fn a_background_grandchild_does_not_extend_the_deadline() {
        let started = Instant::now();
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 25 & exit 0"]);
        let o = run(cmd, Duration::from_secs(10));
        assert_eq!(o.code, Some(0));
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "{:?}",
            started.elapsed()
        );

        let started = Instant::now();
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 25 & sleep 30"]);
        let o = run(cmd, Duration::from_secs(1));
        assert!(o.timed_out);
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "{:?}",
            started.elapsed()
        );
    }

    /// A grandchild in its own session escapes the process group; the caller still
    /// returns shortly after the deadline instead of waiting for its pipe.
    #[test]
    fn an_escaped_grandchild_does_not_extend_the_deadline() {
        if Command::new("sh")
            .args(["-c", "command -v setsid"])
            .output()
            .map_or(true, |o| !o.status.success())
        {
            return; // no setsid (macOS): nothing to check
        }
        let started = Instant::now();
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "setsid sleep 25 & sleep 30"]);
        let o = run(cmd, Duration::from_secs(1));
        assert!(o.timed_out);
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn output_and_exit_code() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo out; echo err >&2; exit 3"]);
        let o = run(cmd, Duration::from_secs(10));
        assert_eq!(o.code, Some(3));
        assert_eq!(o.output, "out\nerr\n");
        let o = run(
            Command::new("definitely-not-a-command-xyz"),
            Duration::from_secs(1),
        );
        assert!(o.start_error.is_some());
    }
}
