#![cfg(unix)]
#![allow(clippy::disallowed_methods)] // tests may unwrap and sleep on a child process

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::Duration;

/// Both listeners pick an ephemeral port; the control plane logs the address it
/// actually bound, and it only starts once every DNS socket is listening.
fn config(port: u16) -> String {
    format!(
        r#"
[server]
listen_addr = "127.0.0.1"
port = {port}
worker_threads = 1
shutdown_timeout = 2

[middleware]
log_format = "text"

[control]
enabled = true
listen_addr = "127.0.0.1"
port = 0
endpoints = ["healthz"]
"#
    )
}

fn temp_config_path(label: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    std::env::temp_dir().join(format!(
        "doggy-dns-{label}-{}-{}.toml",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Write the fixture to a per-test temp path and remove it on drop.
fn write_config(label: &str, port: u16) -> (PathBuf, RemoveFileOnDrop) {
    let path = temp_config_path(label);
    std::fs::write(&path, config(port)).expect("write smoke-test config");
    (path.clone(), RemoveFileOnDrop(path))
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct RemoveFileOnDrop(PathBuf);

impl Drop for RemoveFileOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Spawn the binary and start draining its output. The reader threads end on
/// EOF, which happens once the child exits and both pipes close.
fn start_binary(config_path: &Path) -> (KillOnDrop, mpsc::Receiver<String>, Vec<JoinHandle<()>>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_doggy-dns"))
        .arg(config_path)
        .env_remove("RUST_LOG")
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn doggy-dns");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let guard = KillOnDrop(child);

    let (lines_tx, lines_rx) = mpsc::channel::<String>();
    let mut readers = Vec::with_capacity(2);
    for stream in [
        Box::new(stdout) as Box<dyn Read + Send>,
        Box::new(stderr) as Box<dyn Read + Send>,
    ] {
        let lines_tx = lines_tx.clone();
        readers.push(std::thread::spawn(move || {
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            while let Ok(read) = reader.read_line(&mut line) {
                if read == 0 {
                    break;
                }
                let _ = lines_tx.send(std::mem::take(&mut line));
            }
        }));
    }
    drop(lines_tx);

    (guard, lines_rx, readers)
}

/// Wait for both readers to hit EOF, then drain everything they forwarded.
fn collect_output(readers: Vec<JoinHandle<()>>, lines: &mpsc::Receiver<String>) -> String {
    for reader in readers {
        reader.join().expect("log reader thread");
    }
    let mut output = String::new();
    while let Ok(line) = lines.try_recv() {
        output.push_str(&line);
    }
    output
}

/// Wait up to 30 s for the child to exit.
fn wait_for_exit(child: &mut Child) -> Option<std::process::ExitStatus> {
    for _ in 0..600 {
        if let Some(exit) = child.try_wait().expect("try_wait") {
            return Some(exit);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

#[test]
fn dns_and_management_runtime_start_and_stop() {
    let (config_path, _config_guard) = write_config("runtime-smoke", 0);

    let (mut guard, lines_rx, readers) = start_binary(&config_path);

    let mut seen = String::new();
    let mut control_port = None;
    for _ in 0..300 {
        match lines_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(line) => {
                if let Some(port) = control_port_from_log(&line) {
                    control_port = Some(port);
                    seen.push_str(&line);
                    break;
                }
                seen.push_str(&line);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let control_port =
        control_port.unwrap_or_else(|| panic!("control plane never listened; output:\n{seen}"));

    let response = get_healthz(control_port);
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "unexpected /healthz response:\n{response}"
    );

    #[cfg(target_os = "linux")]
    {
        let names = thread_names(guard.0.id()).expect("read /proc thread names");
        assert!(
            names.iter().any(|name| name.starts_with("doggy-dns-worke")),
            "no dns worker thread; threads: {names:?}"
        );
        assert!(
            !names.iter().any(|name| name.starts_with("doggy-dns-runti")),
            "the old single-runtime name is still present; threads: {names:?}"
        );
    }

    let kill = Command::new("kill")
        .arg("-TERM")
        .arg(guard.0.id().to_string())
        .status()
        .expect("run kill");
    assert!(kill.success(), "failed to send SIGTERM: {kill}");

    let status = match wait_for_exit(&mut guard.0) {
        Some(status) => status,
        None => panic!("doggy-dns did not exit after SIGTERM; output:\n{seen}"),
    };
    let output = collect_output(readers, &lines_rx);
    assert!(
        status.success(),
        "doggy-dns exited with {status}:\n{output}"
    );
    assert!(
        output.contains("doggy-dns shut down gracefully"),
        "expected a graceful drain after SIGTERM; output:\n{output}"
    );
    assert!(
        !output.contains("forcing exit"),
        "the drain was forced instead of graceful; output:\n{output}"
    );
}

#[test]
fn bind_failure_exits_non_zero() {
    // A plain listener without SO_REUSEPORT blocks the server's TCP bind on
    // every platform (on Linux all sockets on the port must set the option).
    let held = std::net::TcpListener::bind("127.0.0.1:0").expect("hold a tcp port");
    let port = held.local_addr().expect("held address").port();

    let (config_path, _config_guard) = write_config("bind-failure", port);

    let (mut guard, lines_rx, readers) = start_binary(&config_path);

    let status = match wait_for_exit(&mut guard.0) {
        Some(status) => status,
        None => panic!("doggy-dns did not exit after a bind failure"),
    };
    let output = collect_output(readers, &lines_rx);
    assert!(
        !status.success(),
        "expected a non-zero exit after a bind failure:\n{output}"
    );
    assert!(
        output.contains("failed to start the dns server"),
        "expected the startup error in the output:\n{output}"
    );
}

fn control_port_from_log(line: &str) -> Option<u16> {
    if !line.contains("control plane listening") {
        return None;
    }
    let addr = line.split_once("addr=")?.1.split_whitespace().next()?;
    addr.rsplit_once(':')?.1.parse().ok()
}

fn get_healthz(port: u16) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to /healthz");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .expect("send /healthz request");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("read /healthz response");
    response
}

#[cfg(target_os = "linux")]
fn thread_names(pid: u32) -> std::io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(format!("/proc/{pid}/task"))? {
        let comm = std::fs::read_to_string(entry?.path().join("comm"))?;
        names.push(comm.trim_end().to_string());
    }
    Ok(names)
}
