//! Run a tool with a deadline and bounded output. On expiry the whole process tree is
//! killed, so an orphaned `cargo` cannot keep holding the build directory lock.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Bytes of output kept per stream (the tail; earlier output is dropped).
const KEEP: usize = 256 * 1024;

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
    let out = child.stdout.take();
    let err = child.stderr.take();
    let readers = [
        std::thread::spawn(move || read_tail(out)),
        std::thread::spawn(move || read_tail(err)),
    ];
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
    let output = readers.map(|r| r.join().unwrap_or_default()).concat();
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
    let id = child.id().to_string();
    #[cfg(unix)]
    {
        // The child leads its own process group (process_group(0) above). `--` matters:
        // procps `kill -KILL -<pgid>` reads the group id as an option and kills nothing.
        let _ = Command::new("kill")
            .args(["-s", "KILL", "--", &format!("-{id}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &id])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
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
