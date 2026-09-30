//! Exercise the proxy process with a stalled terminal, rather than only its queue.
#![cfg(unix)]

use nix::fcntl::{FcntlArg, fcntl};
use nix::unistd::{dup, pipe};
use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::fd::AsFd;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

struct ProxyRun {
    child: Child,
    root: tempfile::TempDir,
}

impl ProxyRun {
    fn spawn(stdout: Stdio) -> Self {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(".tmp")).unwrap();
        fs::write(root.path().join(".tmp/pty_debug_on"), "1").unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_hcom"))
            .args([
                "pty",
                "sh",
                "-c",
                "head -c 524288 /dev/zero; printf final-sentinel; touch \"$HCOM_TEST_DONE\"",
            ])
            // Keep synthetic PTY diagnostics out of the live agents' database.
            .env("HCOM_DIR", root.path())
            .env("HCOM_TEST_DONE", root.path().join("child-done"))
            .env_remove("HCOM_INSTANCE_NAME")
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        Self { child, root }
    }

    fn port(&mut self) -> u16 {
        let log = self.root.path().join(format!(
            ".tmp/logs/pty_debug/unknown_{}.log",
            self.child.id()
        ));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(text) = fs::read_to_string(&log)
                && let Some(port) = text.lines().find_map(|line| {
                    line.strip_prefix("Inject port: ")
                        .and_then(|value| value.parse().ok())
                })
            {
                return port;
            }
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "proxy exited before ready"
            );
            assert!(Instant::now() < deadline, "no injection port");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_child_done(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.root.path().join("child-done").exists() {
            assert!(Instant::now() < deadline, "wrapped child did not finish");
            assert!(self.child.try_wait().unwrap().is_none());
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for ProxyRun {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn query_screen(port: u16) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream.write_all(b"\0SCREEN\n").unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let screen: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert!(screen["lines"].is_array());
}

#[test]
fn exited_child_output_survives_slow_terminal_and_screen_queries() {
    let mut proxy = ProxyRun::spawn(Stdio::piped());
    let port = proxy.port();
    proxy.wait_child_done();
    // Exceed the previous 250ms shutdown deadline, keeping stdout unread.
    thread::sleep(Duration::from_millis(600));
    assert!(proxy.child.try_wait().unwrap().is_none());
    query_screen(port);

    let mut stdout = proxy.child.stdout.take().unwrap();
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = proxy.child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "proxy did not finish draining stdout"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let output = reader.join().unwrap();
    assert_eq!(output.len(), 524288 + b"final-sentinel".len());
    assert!(output[..524288].iter().all(|byte| *byte == 0));
    assert_eq!(&output[524288..], b"final-sentinel");
}

#[test]
fn sigkill_cannot_change_launchers_stdout_flags() {
    let (_reader, writer) = pipe().unwrap();
    let original_flags = fcntl(writer.as_fd(), FcntlArg::F_GETFL).unwrap() & libc::O_NONBLOCK;
    let mut proxy = ProxyRun::spawn(Stdio::from(dup(&writer).unwrap()));
    let port = proxy.port();
    proxy.wait_child_done();
    query_screen(port);
    assert_eq!(
        fcntl(writer.as_fd(), FcntlArg::F_GETFL).unwrap() & libc::O_NONBLOCK,
        original_flags
    );
    proxy.child.kill().unwrap(); // SIGKILL bypasses every RAII destructor.
    proxy.child.wait().unwrap();
    assert_eq!(
        fcntl(writer.as_fd(), FcntlArg::F_GETFL).unwrap() & libc::O_NONBLOCK,
        original_flags
    );
}
