//! Options and protocol edge cases that coverage of the C implementation showed the rest of
//! the suite was leaving untouched: signal delivery, privilege dropping, the interface and
//! debug flags, and the less-travelled branches of the WebSocket message loop.

mod common;

use common::{
    connect_ws, drain_until_close, open_terminal, read_until, run_cli, send_command, Ending, Server,
};
use std::time::Duration;

const LONG: Duration = Duration::from_secs(15);

const INPUT: u8 = b'0';
const RESIZE_TERMINAL: u8 = b'1';

/// Reads past the two frames a session opens with.
async fn open_session(ws: &mut common::WsStream) {
    open_terminal(ws, 80, 24, None).await.expect("open");
    common::next_data_frame(ws, Duration::from_secs(5))
        .await
        .expect("window title frame");
    common::next_data_frame(ws, Duration::from_secs(5))
        .await
        .expect("preferences frame");
}

/// Spawns ttyd on a port chosen here rather than by the kernel, retrying when another test
/// wins the race for it.
///
/// Binding a port-0 listener and dropping it does not reserve the port, so a concurrently
/// running test can take it before the child does; the child then fails to start and the
/// failure surfaces as an unrelated assertion. Used only by tests that cannot discover the
/// port from the log — because they assert on the log itself, or on the URL containing it.
fn spawn_on_a_free_port(
    build: impl Fn(u16) -> std::process::Command,
    ready: impl Fn(u16) -> bool,
) -> (std::process::Child, u16) {
    for _ in 0..20 {
        let port = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe bind");
            probe.local_addr().expect("probe address").port()
        };
        let mut child = build(port).spawn().expect("spawn");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if ready(port) {
                return (child, port);
            }
            if let Ok(Some(_)) = child.try_wait() {
                break; // Lost the race, or failed to start — try another port.
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    panic!("could not start ttyd on a free port after 20 attempts");
}

#[test]
fn a_negative_port_is_rejected() {
    let result = run_cli(&["-p", "-1", "bash"]);
    assert_eq!(result.code, 255);
    assert!(
        result.stderr.contains("invalid port"),
        "stderr was {:?}",
        result.stderr
    );
}

#[tokio::test]
async fn the_debug_level_option_is_accepted() {
    let server = Server::start(&["-d", "15", "bash"]);
    let response = common::http_client()
        .get(server.http_url("/token"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn a_low_debug_level_silences_the_log_without_silencing_the_server() {
    // `-d` is the libwebsockets level bitmask: 1 is errors only, so the startup notices go
    // away while the server keeps serving. Verified to match the C build, which is equally
    // silent at `-d 1`. Without this the option is only proved to parse.
    let (mut child, port) = spawn_on_a_free_port(
        |port| {
            let mut c = std::process::Command::new(common::binary());
            c.args(["-p", &port.to_string(), "-d", "1", "bash"])
                .stderr(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null());
            c
        },
        |port| std::net::TcpStream::connect(("127.0.0.1", port)).is_ok(),
    );

    let client = common::http_client();
    let url = format!("http://127.0.0.1:{port}/token");
    let response = client.get(&url).send().await.expect("request");
    assert_eq!(
        response.status(),
        200,
        "the server must still serve at -d 1"
    );

    unsafe {
        libc::kill(child.id() as i32, libc::SIGKILL);
    }
    let _ = child.wait();

    use std::io::Read;
    let mut log = String::new();
    child
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut log)
        .expect("read log");
    assert!(
        log.is_empty(),
        "-d 1 must suppress the startup notices, got: {log}"
    );
}

#[tokio::test]
async fn binding_to_an_explicit_address_works() {
    let server = Server::start(&["-i", "127.0.0.1", "bash"]);
    let response = common::http_client()
        .get(server.http_url("/token"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn binding_to_an_interface_by_name_works() {
    // `-i` accepts a device name as well as an address — the C help documents `eth0`.
    let server = Server::start(&["-i", "lo", "bash"]);
    let response = common::http_client()
        .get(server.http_url("/token"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn ipv6_binding_serves_over_ipv6() {
    // Skipped where the kernel has no IPv6 at all; the address selection itself is covered
    // by a unit test that needs no network stack.
    if std::net::TcpListener::bind("[::1]:0").is_err() {
        return;
    }
    let server = Server::start(&["-6", "bash"]);
    let response = common::http_client()
        .get(format!("http://[::1]:{}/token", server.port))
        .send()
        .await
        .expect("request over IPv6");
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn the_browser_option_does_not_prevent_serving() {
    // There is no display in a test environment, so ttyd should note the failure and carry
    // on rather than treating it as fatal.
    let server = Server::start(&["-B", "bash"]);
    let response = common::http_client()
        .get(server.http_url("/token"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn the_browser_option_launches_the_system_opener_with_the_url() {
    // `-B` is otherwise untestable: with no X server both builds decline to launch anything,
    // so "it did not crash" is all you get. Stubbing `xset` and `xdg-open` onto PATH makes
    // the launch observable — the stub records the URL it was handed.
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tempdir");
    let marker = dir.path().join("opened-url");

    std::fs::write(dir.path().join("xset"), "#!/bin/sh\nexit 0\n").expect("write xset");
    std::fs::write(
        dir.path().join("xdg-open"),
        format!("#!/bin/sh\nprintf '%s' \"$1\" > {}\n", marker.display()),
    )
    .expect("write xdg-open");
    for stub in ["xset", "xdg-open"] {
        std::fs::set_permissions(
            dir.path().join(stub),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("chmod stub");
    }

    let path = format!(
        "{}:{}",
        dir.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let (mut child, port) = spawn_on_a_free_port(
        |port| {
            let mut c = std::process::Command::new(common::binary());
            c.args(["-p", &port.to_string(), "-B", "bash"])
                .env("PATH", &path)
                .stderr(std::process::Stdio::null())
                .stdout(std::process::Stdio::null());
            c
        },
        |port| std::net::TcpStream::connect(("127.0.0.1", port)).is_ok(),
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !marker.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let opened = std::fs::read_to_string(&marker).unwrap_or_default();

    unsafe {
        libc::kill(child.id() as i32, libc::SIGKILL);
    }
    let _ = child.wait();

    assert_eq!(
        opened,
        format!("http://localhost:{port}"),
        "-B must hand the system opener the served URL, as the C build does"
    );
}

#[tokio::test]
async fn the_configured_signal_is_sent_when_the_client_leaves() {
    let dir = tempfile::tempdir().expect("tempdir");
    let witness = dir.path().join("caught");
    // The child records which signal it received, which is the only way to tell -s apart
    // from the default from outside the process.
    let script = format!(
        "trap 'echo caught > {}; exit 0' USR1; echo ready; while true; do sleep 0.1; done",
        witness.display()
    );
    let server = Server::start(&["-s", "SIGUSR1", "-W", "sh", "-c", &script]);

    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_session(&mut ws).await;
    let seen = read_until(&mut ws, "ready", LONG).await;
    assert!(seen.contains("ready"), "the child never started: {seen:?}");

    drop(ws);

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !witness.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        witness.exists(),
        "the child was not signalled with SIGUSR1 as --signal requested"
    );
}

#[tokio::test]
async fn privileges_are_dropped_for_the_child() {
    /// A group id that exists nowhere else, so finding it in the child is unambiguous.
    const MARKER_GROUP: u32 = 60123;

    // Only meaningful when the test process can actually change user.
    if unsafe { libc::geteuid() } != 0 {
        return;
    }
    // The server is started holding a supplementary group so the test can prove it is
    // dropped; `id -G` is comma-joined because its native output is space-separated and
    // would otherwise be truncated when the line is parsed.
    // 65534 is `nobody` on Debian and Ubuntu, where this suite runs.
    let server = Server::start_with_supplementary_groups(
        &[
            "-u",
            "65534",
            "-g",
            "65534",
            "-W",
            "sh",
            "-c",
            "echo IDENTITY=$(id -u):$(id -g):$(id -G | tr ' ' ','); sleep 5",
        ],
        &[MARKER_GROUP],
    );

    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_session(&mut ws).await;

    let seen = read_until(&mut ws, "IDENTITY=", LONG).await;
    let identity = seen
        .split("IDENTITY=")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_default()
        .to_string();
    assert!(
        identity.starts_with("65534:65534:"),
        "the child did not run as the requested uid/gid: {seen:?}"
    );

    // Dropping privileges must also drop the supplementary groups the server was started
    // with — setgid() alone leaves them in place, so a shell would keep whatever groups the
    // operator's own session had (docker, adm, ...).
    let groups = identity.rsplit(':').next().unwrap_or_default();
    let leftovers: Vec<&str> = groups
        .split(',')
        .map(str::trim)
        .filter(|g| !g.is_empty() && *g != "65534")
        .collect();
    assert!(
        leftovers.is_empty(),
        "supplementary groups survived the privilege drop: {leftovers:?} (full: {identity})"
    );
}

#[tokio::test]
async fn once_refuses_a_second_concurrent_client() {
    let server = Server::start(&["-o", "-W", "sh", "-c", "sleep 30"]);

    let mut first = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_session(&mut first).await;

    assert!(
        connect_ws(&server.ws_url("/ws"), &[]).await.is_err(),
        "--once admitted a second client while the first was still connected"
    );
}

#[tokio::test]
async fn an_unknown_message_type_is_ignored() {
    let server = Server::start(&["-W", "sh", "-c", "echo alive; sleep 5"]);
    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_session(&mut ws).await;

    // 'Z' is not part of the protocol; the session must survive it.
    send_command(&mut ws, b'Z', b"nonsense")
        .await
        .expect("send unknown");
    send_command(&mut ws, INPUT, b"\n")
        .await
        .expect("send input");

    let seen = read_until(&mut ws, "alive", LONG).await;
    assert!(
        seen.contains("alive"),
        "an unknown message type ended the session: {seen:?}"
    );
}

#[tokio::test]
async fn a_command_before_authentication_ends_the_session() {
    let server = Server::start(&["-c", "user:pass", "-W", "sh", "-c", "echo started; sleep 5"]);
    let auth = "Basic dXNlcjpwYXNz";
    let mut ws = connect_ws(&server.ws_url("/ws"), &[("Authorization", auth)])
        .await
        .expect("connect");

    // Anything other than the opening frame before the token has been checked is refused.
    send_command(&mut ws, INPUT, b"whoami\n")
        .await
        .expect("send input");

    let (output, ending) = drain_until_close(&mut ws, Duration::from_secs(5)).await;
    assert!(
        !String::from_utf8_lossy(&output).contains("started"),
        "a process started before the token was checked"
    );
    assert_ne!(
        ending,
        Ending::Timeout,
        "the session should have been closed"
    );
}

#[tokio::test]
async fn a_large_input_message_is_reassembled() {
    // A single WebSocket message far larger than any read buffer exercises the server's
    // message reassembly. The payload is split into short lines on purpose: a terminal in
    // canonical mode caps how much it will buffer for one line, so a single enormous line
    // would measure the kernel line discipline rather than anything the server does.
    let server = Server::start(&["-W", "cat"]);
    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_session(&mut ws).await;

    let mut payload = String::with_capacity(80 * 1024);
    for i in 0..2000 {
        payload.push_str(&format!("bulk-line-{i:06}\n"));
    }
    payload.push_str("END-OF-BIG-INPUT\n");
    assert!(
        payload.len() > 32 * 1024,
        "payload must exceed a read buffer"
    );

    send_command(&mut ws, INPUT, payload.as_bytes())
        .await
        .expect("send large input");

    let seen = read_until(&mut ws, "END-OF-BIG-INPUT", Duration::from_secs(30)).await;
    assert!(
        seen.contains("END-OF-BIG-INPUT"),
        "a large input was not delivered intact"
    );
}

#[tokio::test]
async fn a_malformed_resize_message_is_survivable() {
    let server = Server::start(&["-W", "sh", "-c", "echo alive; sleep 5"]);
    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_session(&mut ws).await;

    send_command(&mut ws, RESIZE_TERMINAL, b"not json at all")
        .await
        .expect("send resize");

    let seen = read_until(&mut ws, "alive", LONG).await;
    assert!(
        seen.contains("alive"),
        "a malformed resize ended the session: {seen:?}"
    );
}

#[tokio::test]
async fn shutting_the_server_down_takes_the_child_with_it() {
    // A terminal whose server has gone away would otherwise keep running forever, since the
    // child lives in its own process group and is not signalled by the kernel.
    let mut server = Server::start(&["-W", "sh", "-c", "sleep 120"]);

    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_session(&mut ws).await;

    let pid = server
        .child_pid(Duration::from_secs(10))
        .expect("the server never reported starting a process");
    assert!(common::process_exists(pid), "the child was never running");

    server.terminate();

    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while common::process_exists(pid) && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        !common::process_exists(pid),
        "child process {pid} survived the server shutting down"
    );
}

#[tokio::test]
async fn the_send_buffer_size_bounds_one_output_frame() {
    // Regression: --srv-buf-size used to be parsed and then never read, so the option was a
    // silent no-op. It now bounds how much terminal output travels in a single frame.
    if common::is_c_reference() {
        // The C build spends this on the libwebsockets service buffer, which is not
        // observable as a frame size from the client side.
        return;
    }
    let server = Server::start(&[
        "-f",
        "1024",
        "-W",
        "sh",
        "-c",
        "i=0; while [ $i -lt 400 ]; do echo bulk-output-line-$i; i=$((i+1)); done; echo BURST-DONE; sleep 3",
    ]);
    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_session(&mut ws).await;

    let mut largest = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut done = false;
    while tokio::time::Instant::now() < deadline && !done {
        match common::next_data_frame(&mut ws, Duration::from_secs(3)).await {
            Some(frame) if frame.first() == Some(&b'0') => {
                largest = largest.max(frame.len() - 1);
                done = String::from_utf8_lossy(&frame[1..]).contains("BURST-DONE");
            }
            Some(_) => continue,
            None => break,
        }
    }
    assert!(
        done,
        "the burst never completed; largest frame seen {largest}"
    );
    assert!(
        largest <= 1024,
        "an output frame carried {largest} bytes despite --srv-buf-size 1024"
    );
}

#[tokio::test]
async fn the_credential_never_reaches_the_log() {
    // Deliberate divergence: the C build prints the base64 credential in its startup banner
    // and echoes the presented token when a WebSocket handshake fails. Both are reversible
    // `user:password`, so anything scraping stdout collects the password.
    if common::is_c_reference() {
        return;
    }
    let mut server = Server::start(&["-c", "user:pass", "-W", "sh", "-c", "sleep 5"]);

    // A failed handshake must not echo what was presented either.
    let mut ws = connect_ws(
        &server.ws_url("/ws"),
        &[("Authorization", "Basic dXNlcjpwYXNz")],
    )
    .await
    .expect("connect");
    open_terminal(&mut ws, 80, 24, Some("dXNlcjpwYXNz-wrong"))
        .await
        .expect("open");
    let _ = drain_until_close(&mut ws, Duration::from_secs(5)).await;

    let logs = server.logs();
    for secret in ["dXNlcjpwYXNz", "user:pass"] {
        assert!(
            !logs.contains(secret),
            "the credential appeared in the server log: {logs}"
        );
    }
}

#[tokio::test]
async fn the_websocket_log_records_the_client_address() {
    // The WS line is the only record of who opened a terminal, so it has to name the peer.
    let mut server = Server::start(&["-W", "sh", "-c", "sleep 5"]);
    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_session(&mut ws).await;
    assert!(
        server.wait_for_log("pid:", Duration::from_secs(10)),
        "the session never started"
    );

    let ws_line = server
        .logs()
        .lines()
        .find(|line| line.contains("WS ") && line.contains("clients:"))
        .map(str::to_string)
        .expect("no WS log line was emitted");
    assert!(
        ws_line.contains("127.0.0.1"),
        "the WS log line does not name the client address: {ws_line}"
    );
}

#[tokio::test]
async fn a_base_path_without_a_leading_slash_still_starts() {
    // Regression: this used to reach the router verbatim and panic after the banner. The C
    // build starts but builds unreachable endpoints (`mounted/token` matches no request
    // path), so normalizing is a deliberate divergence rather than shared behaviour.
    if common::is_c_reference() {
        return;
    }
    let server = Server::start(&["-b", "mounted", "bash"]);
    let response = common::http_client()
        .get(server.http_url("/mounted/token"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn the_server_pings_idle_sessions() {
    // Without periodic pings a terminal nobody is typing into gets dropped by reverse
    // proxies and NAT devices after their idle timeout.
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    let server = Server::start(&["-P", "1", "-W", "sh", "-c", "sleep 30"]);
    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_session(&mut ws).await;

    let mut saw_ping = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, ws.next()).await {
            Ok(Some(Ok(Message::Ping(_)))) => {
                saw_ping = true;
                break;
            }
            Ok(Some(Ok(_))) => continue,
            _ => break,
        }
    }
    assert!(
        saw_ping,
        "no WebSocket ping arrived within 8s of --ping-interval 1"
    );
}

/// Starts a server on a UNIX socket, waits for the file, and returns its uid, gid and mode.
async fn socket_metadata(extra: &[&str]) -> (u32, u32, u32) {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("owned.sock");
    let mut args = vec!["-i", socket.to_str().unwrap()];
    args.extend_from_slice(extra);
    args.push("bash");

    let mut child = std::process::Command::new(common::binary())
        .args(&args)
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn");

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !socket.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(socket.exists(), "the unix socket was never created");
    // The chown/chmod happen right after bind, but the file appears at bind — give the
    // permission calls a moment rather than racing them.
    tokio::time::sleep(Duration::from_millis(200)).await;

    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(&socket).expect("stat socket");
    let result = (meta.uid(), meta.gid(), meta.mode() & 0o777);

    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let _ = child.wait();
    result
}

#[tokio::test]
async fn a_unix_socket_is_not_world_connectable() {
    // libwebsockets chmods the socket to 0660 unconditionally after binding. Inheriting the
    // umask instead would leave it 0755 on a default umask, handing a terminal to every
    // local user — so this is the security-relevant half of the unix socket path.
    let (_, _, mode) = socket_metadata(&[]).await;
    assert_eq!(
        mode, 0o660,
        "unix socket mode must match the C build's 0660"
    );
}

#[tokio::test]
async fn the_gid_option_owns_the_unix_socket() {
    // The C build chowns the socket to the context uid/gid, which is what -u/-g set.
    if unsafe { libc::geteuid() } != 0 {
        return;
    }
    let (_, gid, mode) = socket_metadata(&["-g", "1"]).await;
    assert_eq!(gid, 1, "-g must own the socket, as it does in the C build");
    assert_eq!(mode, 0o660);
}

#[tokio::test]
async fn the_socket_owner_option_is_applied() {
    // chown to another owner requires root; without it the option cannot be exercised at all.
    if unsafe { libc::geteuid() } != 0 {
        return;
    }
    // `daemon` is uid/gid 1 on every distribution this runs on, and is not the test's own
    // identity — asserting against root would pass whether or not the chown happened.
    let (uid, gid, mode) = socket_metadata(&["-U", "daemon:daemon"]).await;
    assert_eq!((uid, gid), (1, 1), "socket owner was not applied");
    assert_eq!(mode, 0o660);
}

#[tokio::test]
async fn a_custom_index_path_expands_a_leading_tilde() {
    let home = std::env::var("HOME").expect("HOME must be set");
    let name = format!(".ttyd-test-index-{}.html", std::process::id());
    let path = std::path::Path::new(&home).join(&name);
    std::fs::write(&path, "<html>tilde index</html>").expect("write");

    let server = Server::start(&["-I", &format!("~/{name}"), "bash"]);
    let body = common::http_client()
        .get(server.http_url("/"))
        .send()
        .await
        .expect("request")
        .text()
        .await
        .expect("body");

    let _ = std::fs::remove_file(&path);
    assert_eq!(body, "<html>tilde index</html>");
}

#[tokio::test]
async fn a_large_burst_of_output_is_delivered_intact() {
    // Exercises the flow control path: the child writes far more than any socket buffer
    // holds, so the reader must throttle without dropping or reordering bytes.
    let server = Server::start(&[
        "-W",
        "sh",
        "-c",
        "i=0; while [ $i -lt 2000 ]; do echo line-$i; i=$((i+1)); done; echo BURST-DONE; sleep 3",
    ]);
    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_session(&mut ws).await;

    let seen = read_until(&mut ws, "BURST-DONE", Duration::from_secs(30)).await;
    assert!(seen.contains("BURST-DONE"), "the burst never completed");
    for probe in ["line-0", "line-999", "line-1999"] {
        assert!(seen.contains(probe), "output lost {probe}");
    }
}
