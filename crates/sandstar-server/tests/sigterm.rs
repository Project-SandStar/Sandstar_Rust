//! SIGTERM (`systemctl stop`) runs the graceful shutdown (R-13, S-43).
//!
//! Starts the real server binary, sends SIGTERM, and checks the exit code,
//! the shutdown log line and the cleanup of the PID file and IPC socket.
//! Unix only: there is no SIGTERM on Windows.
#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A running server; killed and reaped if the test fails before it exits.
struct Server {
    child: Child,
    lines: Arc<Mutex<Vec<String>>>,
    reader: Option<std::thread::JoinHandle<()>>,
    dir: PathBuf,
    pid_file: PathBuf,
    socket: PathBuf,
}

impl Server {
    fn start(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("sandstar-sigterm-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("engine.pid");
        let socket = dir.join("engine.sock");
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let mut child = Command::new(env!("CARGO_BIN_EXE_sandstar-engine-server"))
            .args(["--http-port", &port.to_string(), "--http-bind", "127.0.0.1"])
            .arg("--pid-file")
            .arg(&pid_file)
            .arg("--socket")
            .arg(&socket)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let stderr = child.stderr.take().unwrap();
        let reader = {
            let lines = lines.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    lines.lock().unwrap().push(line);
                }
            })
        };
        Self {
            child,
            lines,
            reader: Some(reader),
            dir,
            pid_file,
            socket,
        }
    }

    fn log(&self) -> String {
        self.lines.lock().unwrap().join("\n")
    }

    fn wait_for_line(&self, pred: impl Fn(&str) -> bool, within: Duration) {
        let start = Instant::now();
        while start.elapsed() < within {
            if self.lines.lock().unwrap().iter().any(|l| pred(l)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("expected log line did not appear:\n{}", self.log());
    }

    fn sigterm(&self) {
        let status = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(status.success(), "kill -TERM");
    }

    fn wait_exit(&mut self, within: Duration) -> ExitStatus {
        let start = Instant::now();
        loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                if let Some(r) = self.reader.take() {
                    let _ = r.join();
                }
                return s;
            }
            if start.elapsed() > within {
                panic!("server did not stop within {within:?}:\n{}", self.log());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn assert_clean_sigterm_exit(&mut self) {
        let exit = self.wait_exit(Duration::from_secs(20));
        let log = self.log();
        assert!(exit.success(), "exit after SIGTERM: {exit:?}\n{log}");
        assert!(
            log.lines()
                .any(|l| l.contains("engine stopped") && l.contains("SIGTERM")),
            "shutdown log line with the signal:\n{log}"
        );
        assert!(!self.pid_file.exists(), "PID file removed on shutdown");
        assert!(!self.socket.exists(), "IPC socket removed on shutdown");
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn sigterm_runs_the_graceful_shutdown() {
    let mut server = Server::start("running");
    server.wait_for_line(|l| l.contains("engine running"), Duration::from_secs(30));
    assert!(server.pid_file.exists(), "PID file written at start");
    assert!(server.socket.exists(), "IPC socket created at start");
    server.sigterm();
    server.assert_clean_sigterm_exit();
}

#[test]
fn sigterm_during_startup_is_held_until_the_loop_runs() {
    // The listener is installed before the first stderr line; a SIGTERM
    // sent then must not kill the process mid-startup.
    let mut server = Server::start("startup");
    server.wait_for_line(|_| true, Duration::from_secs(30));
    server.sigterm();
    server.assert_clean_sigterm_exit();
}
