//! Test harness for driving a ttyd binary as a black box.
//!
//! The binary under test is chosen by the `TTYD_BIN` environment variable, defaulting to the
//! Rust build. Pointing it at the C build runs the identical suite against the original
//! implementation, which is what makes these characterization tests differential: any
//! assertion that passes for one binary and fails for the other is a behavioural divergence.

#![allow(dead_code)]

use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

pub const TTY_SUBPROTOCOL: &str = "tty";

/// The loopback device, named `lo` on Linux and `lo0` on the BSDs, Apple platforms included.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub const LOOPBACK: &str = "lo";
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub const LOOPBACK: &str = "lo0";

/// The program `--browser` hands the URL to, which `utils::open_uri` selects per platform.
#[cfg(target_os = "macos")]
pub const SYSTEM_OPENER: &str = "open";
#[cfg(not(target_os = "macos"))]
pub const SYSTEM_OPENER: &str = "xdg-open";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Path of the binary under test.
pub fn binary() -> String {
    std::env::var("TTYD_BIN").unwrap_or_else(|_| env!("CARGO_BIN_EXE_ttyd").to_string())
}

/// True when the suite is running against the original C implementation, which lacks the
/// features this port adds.
///
/// Asked of the binary rather than of a second environment variable. `TTYD_REFERENCE` and
/// `TTYD_BIN` used to have to be set together, and setting only `TTYD_BIN` — which the module
/// docs above describe as the way to run against the C build — left the guards believing they
/// were on the Rust build. The tests that then passed a port-only option to the C binary got
/// getopt's "unrecognized option" warning and a *server*, because that is what the C build
/// does with an option it does not know: complains and starts anyway. `run_cli` waited for a
/// process that would never exit, and the whole suite hung with no output.
///
/// `TTYD_REFERENCE` still forces the answer, for a build whose `--help` cannot be trusted.
pub fn is_c_reference() -> bool {
    static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        if std::env::var("TTYD_REFERENCE").is_ok() {
            return true;
        }
        // `--auth-url` is added by this port, so its absence from the help text identifies
        // the reference build without hard-coding anything about where it lives on disk.
        let Ok(out) = Command::new(binary()).arg("--help").output() else {
            return false;
        };
        let help = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        !help.contains("--auth-url")
    })
}

/// A running ttyd process, torn down when the value is dropped.
pub struct Server {
    child: Child,
    pub port: u16,
    logs: mpsc::Receiver<String>,
    collected: Vec<String>,
}

impl Server {
    /// Starts ttyd on an ephemeral port with the given extra arguments, which must include
    /// the command to run.
    pub fn start(args: &[&str]) -> Server {
        Self::start_with_env(args, &[])
    }

    /// Starts ttyd holding the given supplementary groups, so a test can prove they are
    /// dropped. Requires root; the group ids need not exist in `/etc/group`.
    pub fn start_with_supplementary_groups(args: &[&str], groups: &[u32]) -> Server {
        let groups = groups.to_vec();
        Self::start_inner(args, &[], Some(groups))
    }

    pub fn start_with_env(args: &[&str], env: &[(&str, &str)]) -> Server {
        Self::start_inner(args, env, None)
    }

    fn start_inner(args: &[&str], env: &[(&str, &str)], groups: Option<Vec<u32>>) -> Server {
        let mut command = Command::new(binary());
        command.arg("-p").arg("0");
        command.args(args);
        for (key, value) in env {
            command.env(key, value);
        }
        if let Some(groups) = groups {
            // Safety: `setgroups` is async-signal-safe and the vector outlives the call.
            unsafe {
                command.pre_exec(move || {
                    let ids: Vec<libc::gid_t> = groups.iter().map(|g| *g as libc::gid_t).collect();
                    // `ngroups` is `size_t` on the Linux family and `int` everywhere else,
                    // the BSDs and Apple platforms included, so the axis is Linux rather
                    // than Apple. Keying this on Apple alone would leave the other BSDs
                    // failing to compile exactly as before.
                    #[cfg(any(target_os = "linux", target_os = "android"))]
                    let ngroups = ids.len();
                    #[cfg(not(any(target_os = "linux", target_os = "android")))]
                    let ngroups = ids.len() as libc::c_int;
                    if libc::setgroups(ngroups, ids.as_ptr()) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        // stdout is discarded rather than piped: nothing reads it, so a server that wrote
        // more than the pipe buffer would block in `write` and look like a hang. That is the
        // same trap the soak harness fell into with stderr, which this suite does drain.
        command
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());

        let mut child = command
            .spawn()
            .unwrap_or_else(|e| panic!("cannot start {}: {e}", binary()));

        let stderr = child.stderr.take().expect("stderr was piped");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });

        let mut collected = Vec::new();
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        let mut port = None;
        while Instant::now() < deadline && port.is_none() {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(line) => {
                    port = parse_port(&line);
                    collected.push(line);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        let port = match port {
            Some(port) => port,
            None => {
                // `Child::drop` neither kills nor reaps, so panicking here would leave this
                // ttyd and its shell running for the rest of the suite.
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "ttyd did not report a listening port; output so far:\n{}",
                    collected.join("\n")
                )
            }
        };

        Server {
            child,
            port,
            logs: rx,
            collected,
        }
    }

    pub fn http_url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    pub fn ws_url(&self, path: &str) -> String {
        format!("ws://127.0.0.1:{}{path}", self.port)
    }

    /// Everything the server has written to stderr so far.
    pub fn logs(&mut self) -> String {
        while let Ok(line) = self.logs.try_recv() {
            self.collected.push(line);
        }
        self.collected.join("\n")
    }

    /// Blocks until the log contains `needle`, or the timeout elapses.
    pub fn wait_for_log(&mut self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.logs().contains(needle) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Extracts the child process id from the `started process, pid: N` log line that both
    /// implementations emit.
    pub fn child_pid(&mut self, timeout: Duration) -> Option<i32> {
        let deadline = Instant::now() + timeout;
        loop {
            let logs = self.logs();
            if let Some(pid) = logs
                .lines()
                .filter_map(|line| line.split("pid:").nth(1))
                .filter_map(|rest| rest.split_whitespace().next())
                .filter_map(|token| token.trim_end_matches(',').parse::<i32>().ok())
                .next_back()
            {
                return Some(pid);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Sends SIGTERM and waits for the server to exit, the way a service manager would.
    pub fn terminate(&mut self) {
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        self.wait_for_exit(Duration::from_secs(10));
    }

    /// Waits for the process to exit and returns its status code, if it exits in time.
    pub fn wait_for_exit(&mut self, timeout: Duration) -> Option<i32> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status.code().unwrap_or(-1)),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                Ok(None) => return None,
                Err(_) => return None,
            }
        }
    }

    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Ask politely first so the server runs its shutdown path — that is what a coverage
        // build needs in order to flush its profile data, and it exercises signal handling.
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                _ => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Whether a process id is still alive, used to prove sessions clean up after themselves.
pub fn process_exists(pid: i32) -> bool {
    // Safety: signal 0 performs the permission and existence check without delivering.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn parse_port(line: &str) -> Option<u16> {
    let idx = line.find("Listening on port:")?;
    line[idx + "Listening on port:".len()..]
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Runs ttyd once and captures how it terminated, for command-line behaviour tests.
pub struct RunResult {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// How long `run_cli` waits for a binary that is supposed to exit on its own.
///
/// These tests all pass arguments that should make ttyd print a diagnostic and stop. When one
/// of them starts a server instead, `Command::output()` blocks forever and takes the suite
/// with it — silently, since a hung test prints nothing. Bounding the wait converts that into
/// a named failure with the captured output attached.
const CLI_TIMEOUT: Duration = Duration::from_secs(15);

pub fn run_cli(args: &[&str]) -> RunResult {
    let mut child = Command::new(binary())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("cannot run ttyd");

    // Drained on threads: a process killed at the deadline may already have filled a pipe
    // buffer, and reading only after the kill would deadlock against a writer blocked in
    // write(). This is the same undrained-pipe failure the soak harness hit.
    let mut out = child.stdout.take().expect("piped");
    let mut err = child.stderr.take().expect("piped");
    let out_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut out, &mut buf);
        buf
    });
    let err_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut err, &mut buf);
        buf
    });

    let deadline = Instant::now() + CLI_TIMEOUT;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait().expect("cannot poll ttyd") {
            Some(status) => break Some(status),
            None if Instant::now() >= deadline => {
                timed_out = true;
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };

    let stdout = String::from_utf8_lossy(&out_thread.join().expect("stdout drain")).into_owned();
    let stderr = String::from_utf8_lossy(&err_thread.join().expect("stderr drain")).into_owned();
    assert!(
        !timed_out,
        "ttyd {args:?} did not exit within {CLI_TIMEOUT:?} — it started a server instead.\n\
         stdout: {stdout:?}\nstderr: {stderr:?}"
    );

    RunResult {
        code: status.and_then(|s| s.code()).unwrap_or(-1),
        stdout,
        stderr,
    }
}

pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        // The bundle is served pre-compressed; decoding it would hide the wire encoding.
        .no_gzip()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client builds")
}

pub type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Opens a `tty` WebSocket, optionally with extra request headers.
pub async fn connect_ws(
    url: &str,
    headers: &[(&str, &str)],
) -> Result<WsStream, tokio_tungstenite::tungstenite::Error> {
    let mut request = url.into_client_request().expect("valid ws url");
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        TTY_SUBPROTOCOL.parse().expect("static header value"),
    );
    for (name, value) in headers {
        request.headers_mut().insert(
            reqwest::header::HeaderName::from_bytes(name.as_bytes()).expect("valid header name"),
            value.parse().expect("valid header value"),
        );
    }
    let (stream, _) = tokio_tungstenite::connect_async(request).await?;
    Ok(stream)
}

/// Receives the next binary or text frame, ignoring pings and pongs.
pub async fn next_data_frame(ws: &mut WsStream, timeout: Duration) -> Option<Vec<u8>> {
    use futures_util::StreamExt;
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, ws.next()).await {
            Ok(Some(Ok(Message::Binary(data)))) => return Some(data.to_vec()),
            Ok(Some(Ok(Message::Text(text)))) => return Some(text.as_bytes().to_vec()),
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) => return None,
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(_))) => return None,
            Err(_) => return None,
        }
    }
}

/// How a session ended, as observed by the client.
#[derive(Debug, PartialEq, Eq)]
pub enum Ending {
    /// A close frame carrying this code.
    Close(u16),
    /// The connection dropped without a close handshake, which browsers report as 1006.
    Abnormal,
    /// Nothing happened before the timeout.
    Timeout,
}

/// Drains the socket until it ends, returning both the accumulated output and the ending.
pub async fn drain_until_close(ws: &mut WsStream, timeout: Duration) -> (Vec<u8>, Ending) {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Error as WsError;

    let mut output = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return (output, Ending::Timeout);
        }
        match tokio::time::timeout(remaining, ws.next()).await {
            Ok(Some(Ok(Message::Binary(data)))) => output.extend_from_slice(&data),
            Ok(Some(Ok(Message::Text(text)))) => output.extend_from_slice(text.as_bytes()),
            Ok(Some(Ok(Message::Close(frame)))) => {
                let code = frame.map(|f| u16::from(f.code)).unwrap_or(1005);
                return (output, Ending::Close(code));
            }
            Ok(Some(Ok(_))) => continue,
            // A reset or a missing close handshake is what the browser surfaces as 1006.
            Ok(Some(Err(WsError::ConnectionClosed))) => return (output, Ending::Abnormal),
            Ok(Some(Err(_))) => return (output, Ending::Abnormal),
            Ok(None) => return (output, Ending::Abnormal),
            Err(_) => return (output, Ending::Timeout),
        }
    }
}

/// Sends the opening frame that makes the server spawn the child process.
pub async fn open_terminal(
    ws: &mut WsStream,
    columns: u16,
    rows: u16,
    token: Option<&str>,
) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    use futures_util::SinkExt;
    let payload = match token {
        Some(token) => format!(r#"{{"columns":{columns},"rows":{rows},"AuthToken":"{token}"}}"#),
        None => format!(r#"{{"columns":{columns},"rows":{rows}}}"#),
    };
    ws.send(Message::Binary(payload.into_bytes())).await
}

pub async fn send_command(
    ws: &mut WsStream,
    command: u8,
    payload: &[u8],
) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    use futures_util::SinkExt;
    let mut frame = Vec::with_capacity(payload.len() + 1);
    frame.push(command);
    frame.extend_from_slice(payload);
    ws.send(Message::Binary(frame)).await
}

/// Reads terminal output until `needle` appears or the timeout elapses.
pub async fn read_until(ws: &mut WsStream, needle: &str, timeout: Duration) -> String {
    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return seen;
        }
        match next_data_frame(ws, remaining).await {
            Some(frame) if frame.first() == Some(&b'0') => {
                seen.push_str(&String::from_utf8_lossy(&frame[1..]));
                if seen.contains(needle) {
                    return seen;
                }
            }
            Some(_) => continue,
            None => return seen,
        }
    }
}
