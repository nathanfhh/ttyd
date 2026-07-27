//! The `tty` WebSocket subprotocol and the terminal sessions behind it.
//!
//! The frame ordering asserted here was established by observing the C implementation: the
//! server stays silent until the browser sends its opening JSON frame, and only then emits
//! the window title, the client preferences, and terminal output in that order.

mod common;

use common::{
    connect_ws, drain_until_close, next_data_frame, open_terminal, read_until, send_command,
    Ending, Server, WsStream,
};
use futures_util::SinkExt;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

const SHORT: Duration = Duration::from_secs(5);
const LONG: Duration = Duration::from_secs(15);

// Client-to-server command bytes.
const INPUT: u8 = b'0';
const RESIZE_TERMINAL: u8 = b'1';
const PAUSE: u8 = b'2';
const RESUME: u8 = b'3';

// Server-to-client command bytes.
const OUTPUT: u8 = b'0';
const SET_WINDOW_TITLE: u8 = b'1';
const SET_PREFERENCES: u8 = b'2';

/// Reads the two frames the server sends once a terminal has been opened.
async fn read_handshake(ws: &mut WsStream) -> (String, String) {
    let title = next_data_frame(ws, SHORT)
        .await
        .expect("window title frame");
    assert_eq!(title[0], SET_WINDOW_TITLE, "first frame must be the title");

    let prefs = next_data_frame(ws, SHORT).await.expect("preferences frame");
    assert_eq!(
        prefs[0], SET_PREFERENCES,
        "second frame must be preferences"
    );

    (
        String::from_utf8_lossy(&title[1..]).into_owned(),
        String::from_utf8_lossy(&prefs[1..]).into_owned(),
    )
}

/// Opens a terminal and consumes the resulting handshake frames.
async fn open_session(ws: &mut WsStream, columns: u16, rows: u16) -> (String, String) {
    open_terminal(ws, columns, rows, None).await.expect("open");
    read_handshake(ws).await
}

/// Waits for a marker file to have content, not merely to exist.
///
/// `echo x > file` creates the directory entry and writes to it as separate syscalls, so a
/// loop that only checks `exists()` can read the empty file in between and assert against a
/// truncated string. Guarding on length is what `e2e-soak.py` already does.
///
/// The blocking `std::fs` read is deliberate. `#[tokio::test]` builds a current-thread
/// runtime, so there is no other task on the worker to starve, and `tokio::fs` would answer
/// a microsecond-long read of a tiny local file by dispatching it to the blocking pool and
/// waiting for it to come back — more machinery, more latency, for a loop that then sleeps
/// 100 ms anyway.
async fn wait_for_content(path: &std::path::Path) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path) {
            if !text.trim().is_empty() {
                return text;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    std::fs::read_to_string(path).unwrap_or_default()
}

#[tokio::test]
async fn nothing_is_sent_before_the_opening_frame() {
    // The title carries the full command line, so a client that has not identified itself
    // must not receive anything at all.
    let server = Server::start(&["-W", "sh", "-c", "echo early-output; sleep 30"]);
    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");

    assert!(
        next_data_frame(&mut ws, Duration::from_millis(1500))
            .await
            .is_none(),
        "the server spoke before the opening frame"
    );
}

#[tokio::test]
async fn the_session_opens_with_a_title_and_preferences() {
    let server = Server::start(&["bash"]);
    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");

    let (title, prefs) = open_session(&mut ws, 80, 24).await;
    assert!(title.starts_with("bash ("), "title was {title:?}");
    assert!(title.ends_with(')'), "title was {title:?}");
    assert_eq!(prefs, "{ }");
}

#[tokio::test]
async fn the_title_carries_the_full_command_line() {
    let server = Server::start(&["-W", "sh", "-c", "sleep 30"]);
    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");

    let (title, _) = open_session(&mut ws, 80, 24).await;
    assert!(title.starts_with("sh -c sleep 30 ("), "title was {title:?}");
}

#[tokio::test]
async fn the_title_option_hides_the_command_line() {
    // Added by this port: the default title is the full command line, which anyone who can
    // open a terminal can read. The C build has no way to suppress it.
    if common::is_c_reference() {
        return;
    }
    let server = Server::start(&[
        "--title",
        "Support Console",
        "-W",
        "sh",
        "-c",
        "sleep 30 # deploy-key-abc123",
    ]);
    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");

    let (title, _) = open_session(&mut ws, 80, 24).await;
    assert_eq!(title, "Support Console");
    assert!(
        !title.contains("deploy-key-abc123"),
        "the command line leaked into the title: {title:?}"
    );
}

#[tokio::test]
async fn client_options_reach_the_browser_as_json() {
    let server = Server::start(&[
        "-t",
        "fontSize=20",
        "-t",
        r#"theme={"background":"red"}"#,
        "bash",
    ]);
    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");

    let (_, prefs) = open_session(&mut ws, 80, 24).await;
    let parsed: serde_json::Value = serde_json::from_str(&prefs).expect("preferences are JSON");
    assert_eq!(parsed["fontSize"], 20);
    assert_eq!(parsed["theme"]["background"], "red");
}

#[tokio::test]
async fn the_process_starts_only_after_the_opening_frame() {
    let server = Server::start(&["-W", "sh", "-c", "echo terminal-is-live; sleep 30"]);
    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");

    open_session(&mut ws, 80, 24).await;
    let seen = read_until(&mut ws, "terminal-is-live", LONG).await;
    assert!(seen.contains("terminal-is-live"), "got {seen:?}");
}

#[tokio::test]
async fn output_frames_are_prefixed_with_the_output_command() {
    let server = Server::start(&["-W", "sh", "-c", "echo hello; sleep 5"]);
    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_session(&mut ws, 80, 24).await;

    let frame = next_data_frame(&mut ws, LONG).await.expect("output frame");
    assert_eq!(frame[0], OUTPUT);
}

#[tokio::test]
async fn input_is_written_to_the_terminal_when_writable() {
    let server = Server::start(&["-W", "cat"]);
    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_session(&mut ws, 80, 24).await;

    send_command(&mut ws, INPUT, b"typed-input\n")
        .await
        .expect("send input");
    let seen = read_until(&mut ws, "typed-input", LONG).await;
    assert!(seen.contains("typed-input"), "got {seen:?}");
}

#[tokio::test]
async fn input_arriving_as_a_text_frame_is_handled_too() {
    // The frontend sends binary, so nothing in the suite had ever sent a text frame — but
    // both builds accept one, and an intermediary that transcodes frames would produce them.
    let dir = tempfile::tempdir().expect("tempdir");
    let marker = dir.path().join("via-text");
    let server = Server::start(&["-W", "bash"]);
    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_session(&mut ws, 80, 24).await;

    let command = format!("0echo TEXT-FRAME-OK > {}\n", marker.display());
    ws.send(Message::Text(command.as_str().into()))
        .await
        .expect("send text frame");

    let seen = wait_for_content(&marker).await;
    assert!(
        seen.contains("TEXT-FRAME-OK"),
        "a text frame did not reach the terminal, got {seen:?}"
    );
}

#[tokio::test]
async fn a_ping_from_the_client_does_not_disturb_the_session() {
    // Control frames the server has no use for must be ignored rather than treated as input
    // or as a reason to hang up. Browsers do not send these, but proxies and health checks do.
    let dir = tempfile::tempdir().expect("tempdir");
    let marker = dir.path().join("after-ping");
    let server = Server::start(&["-W", "bash"]);
    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_session(&mut ws, 80, 24).await;

    ws.send(Message::Ping(Vec::new())).await.expect("send ping");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The session must still work afterwards, which is the whole point.
    let command = format!("0echo STILL-ALIVE > {}\n", marker.display());
    ws.send(Message::Binary(command.into_bytes()))
        .await
        .expect("send input");

    let seen = wait_for_content(&marker).await;
    assert!(
        seen.contains("STILL-ALIVE"),
        "the session stopped working after a client ping, got {seen:?}"
    );
}

#[tokio::test]
async fn input_is_dropped_in_readonly_mode() {
    // Without -W the terminal is read-only, so `cat` must never echo what we send.
    let server = Server::start(&["cat"]);
    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_session(&mut ws, 80, 24).await;

    send_command(&mut ws, INPUT, b"must-not-appear\n")
        .await
        .expect("send input");
    let seen = read_until(&mut ws, "must-not-appear", Duration::from_secs(3)).await;
    assert!(
        !seen.contains("must-not-appear"),
        "read-only mode echoed input: {seen:?}"
    );
}

#[tokio::test]
async fn the_opening_frame_sets_the_initial_window_size() {
    let server = Server::start(&["-W", "sh", "-c", "sleep 0.5; stty size < /dev/tty; sleep 5"]);
    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_session(&mut ws, 132, 43).await;

    let seen = read_until(&mut ws, "43 132", LONG).await;
    assert!(seen.contains("43 132"), "expected '43 132', got {seen:?}");
}

#[tokio::test]
async fn a_zero_sized_opening_frame_falls_back_to_the_default_size() {
    // `process_init()` in the C build starts at 80x24, and a browser that has not measured
    // itself yet sends zeros. Falling through to a 0x0 terminal would make every full-screen
    // program misbehave.
    let server = Server::start(&["-W", "bash"]);
    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_session(&mut ws, 0, 0).await;

    send_command(&mut ws, INPUT, b"stty size\n")
        .await
        .expect("send input");
    let seen = read_until(&mut ws, "24 80", LONG).await;
    assert!(
        seen.contains("24 80"),
        "expected the 80x24 default, got {seen:?}"
    );
}

#[tokio::test]
async fn a_second_json_frame_mid_session_is_accepted() {
    // The opening frame is JSON; nothing stops a client sending another one later, and both
    // builds ignore it rather than treating it as input or as a protocol error.
    let server = Server::start(&["-W", "cat"]);
    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_session(&mut ws, 80, 24).await;

    ws.send(Message::Binary(br#"{"columns":100,"rows":40}"#.to_vec()))
        .await
        .expect("send a second json frame");

    // The session must still carry input afterwards.
    send_command(&mut ws, INPUT, b"still-here\n")
        .await
        .expect("send input");
    let seen = read_until(&mut ws, "still-here", LONG).await;
    assert!(seen.contains("still-here"), "got {seen:?}");
}

#[tokio::test]
async fn resize_changes_the_window_size() {
    let server = Server::start(&["-W", "sh", "-c", "sleep 1; stty size < /dev/tty; sleep 5"]);
    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_session(&mut ws, 80, 24).await;

    send_command(&mut ws, RESIZE_TERMINAL, br#"{"columns":120,"rows":40}"#)
        .await
        .expect("resize");

    let seen = read_until(&mut ws, "40 120", LONG).await;
    assert!(seen.contains("40 120"), "expected '40 120', got {seen:?}");
}

#[tokio::test]
async fn pause_stops_output_and_resume_restarts_it() {
    // Deliberate divergence from the C implementation, where PAUSE does nothing. `pty_pause`
    // begins with `if (process->paused) return;` and `pty_resume` with
    // `if (!process->paused) return;`, but neither function ever assigns to `paused` — it is
    // set to true once in `pty_spawn` and stays that way. So `pty_pause` always returns
    // early and never stops reading. This port implements the flow control the protocol
    // describes.
    if common::is_c_reference() {
        return;
    }

    // A steady trickle of output makes the difference between paused and running obvious:
    // roughly five lines per second should arrive unless the server has stopped reading.
    let server = Server::start(&[
        "-W",
        "sh",
        "-c",
        "i=0; while [ $i -lt 300 ]; do echo tick-$i; i=$((i+1)); sleep 0.2; done",
    ]);
    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_session(&mut ws, 80, 24).await;

    let started = read_until(&mut ws, "tick-2", LONG).await;
    assert!(
        started.contains("tick-2"),
        "terminal never started: {started:?}"
    );

    send_command(&mut ws, PAUSE, b"").await.expect("pause");
    // Let whatever was already in flight arrive, then confirm the stream has gone quiet.
    let _ = read_until(&mut ws, "\u{0}never-matches", Duration::from_millis(700)).await;
    let while_paused = read_until(&mut ws, "\u{0}never-matches", Duration::from_secs(2)).await;
    assert!(
        while_paused.is_empty(),
        "output continued while paused: {while_paused:?}"
    );

    send_command(&mut ws, RESUME, b"").await.expect("resume");
    let after_resume = read_until(&mut ws, "tick-", LONG).await;
    assert!(
        !after_resume.is_empty(),
        "output did not resume after RESUME"
    );
}

#[tokio::test]
async fn a_clean_exit_closes_with_code_1000() {
    // Deliberate divergence from the C implementation. `callback_tty` does call
    // `lws_close_reason(wsi, 1000)`, but it returns 1 from the writable callback in the same
    // breath, which makes libwebsockets drop the connection instead of completing the close
    // handshake — the code never reaches the wire. The frontend keys its "should I
    // reconnect?" decision on `event.code !== 1000`, so in the C build a shell that exits
    // cleanly still triggers a reconnect. This port completes the handshake instead.
    if common::is_c_reference() {
        return;
    }

    let server = Server::start(&["-W", "sh", "-c", "exit 0"]);
    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_terminal(&mut ws, 80, 24, None).await.expect("open");

    let (_, ending) = drain_until_close(&mut ws, LONG).await;
    assert_eq!(
        ending,
        Ending::Close(1000),
        "a clean exit must tell the frontend not to reconnect"
    );
}

#[tokio::test]
async fn a_failing_exit_does_not_close_with_1000() {
    // The frontend reconnects for any code other than 1000, which is how a crashed shell is
    // meant to behave.
    let server = Server::start(&["-W", "sh", "-c", "exit 3"]);
    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_terminal(&mut ws, 80, 24, None).await.expect("open");

    let (_, ending) = drain_until_close(&mut ws, LONG).await;
    assert_ne!(ending, Ending::Close(1000), "a failure must not look clean");
    assert_ne!(ending, Ending::Timeout, "the session should have ended");
}

#[tokio::test]
async fn terminal_output_is_flushed_before_the_session_closes() {
    let server = Server::start(&["-W", "sh", "-c", "echo final-output"]);
    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_terminal(&mut ws, 80, 24, None).await.expect("open");

    let (output, _) = drain_until_close(&mut ws, LONG).await;
    let text = String::from_utf8_lossy(&output);
    assert!(text.contains("final-output"), "got {text:?}");
}

#[tokio::test]
async fn the_terminal_type_is_exported_as_term() {
    let server = Server::start(&["-T", "vt220", "-W", "sh", "-c", "echo TERM=$TERM; sleep 5"]);
    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_session(&mut ws, 80, 24).await;

    let seen = read_until(&mut ws, "TERM=vt220", LONG).await;
    assert!(seen.contains("TERM=vt220"), "got {seen:?}");
}

#[tokio::test]
async fn the_default_terminal_type_is_xterm_256color() {
    let server = Server::start(&["-W", "sh", "-c", "echo TERM=$TERM; sleep 5"]);
    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_session(&mut ws, 80, 24).await;

    let seen = read_until(&mut ws, "TERM=xterm-256color", LONG).await;
    assert!(seen.contains("TERM=xterm-256color"), "got {seen:?}");
}

#[tokio::test]
async fn the_working_directory_is_applied() {
    let server = Server::start(&["-w", "/tmp", "-W", "sh", "-c", "pwd; sleep 5"]);
    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_session(&mut ws, 80, 24).await;

    let seen = read_until(&mut ws, "/tmp", LONG).await;
    assert!(seen.contains("/tmp"), "got {seen:?}");
}

#[tokio::test]
async fn url_arguments_are_appended_to_the_command() {
    // The child outlives its own output on purpose: a process that exits the instant it has
    // written can lose that output to the C build's abrupt connection teardown, and this
    // test is about argument passing, not shutdown timing.
    let server = Server::start(&["-a", "-W", "sh", "-c", r#"echo "$@"; sleep 3"#, "sh"]);
    // Percent-encoded values are the interesting case: a value that survives whether or not
    // decoding happens proves nothing, which is how an encoding divergence stayed hidden.
    let mut ws = connect_ws(&server.ws_url("/ws?arg=hello%20world&arg=second"), &[])
        .await
        .expect("connect");
    open_session(&mut ws, 80, 24).await;

    let seen = read_until(&mut ws, "hello world second", LONG).await;
    assert!(
        seen.contains("hello world second"),
        "url arguments were not decoded before reaching the child: {seen:?}"
    );
}

#[tokio::test]
async fn url_arguments_are_ignored_without_the_flag() {
    let server = Server::start(&["-W", "sh", "-c", r#"echo "fixed-output $@"; sleep 3"#, "sh"]);
    let mut ws = connect_ws(&server.ws_url("/ws?arg=injected"), &[])
        .await
        .expect("connect");
    open_session(&mut ws, 80, 24).await;

    let seen = read_until(&mut ws, "fixed-output", LONG).await;
    assert!(seen.contains("fixed-output"), "got {seen:?}");
    assert!(
        !seen.contains("injected"),
        "url arguments leaked without -a: {seen:?}"
    );
}

#[tokio::test]
async fn the_authenticated_user_is_exported_as_ttyd_user() {
    let server = Server::start(&[
        "-H",
        "X-Remote-User",
        "-W",
        "sh",
        "-c",
        "echo USER=$TTYD_USER; sleep 5",
    ]);
    // Deliberately longer than the C build's 29-byte `pss_tty.user` buffer. There
    // `lws_hdr_custom_copy` refuses a value that does not fit, so such an account cannot open
    // a terminal at all; this port has no fixed buffer and passes the name through whole.
    let long_name = "alice-with-a-very-long-account-name";
    if common::is_c_reference() {
        assert!(
            connect_ws(&server.ws_url("/ws"), &[("X-Remote-User", long_name)])
                .await
                .is_err(),
            "the C build is expected to refuse an identity that overflows its buffer"
        );
        return;
    }
    let mut ws = connect_ws(&server.ws_url("/ws"), &[("X-Remote-User", long_name)])
        .await
        .expect("connect");
    open_session(&mut ws, 80, 24).await;

    let seen = read_until(&mut ws, &format!("USER={long_name}"), LONG).await;
    assert!(
        seen.contains(&format!("USER={long_name}")),
        "the identity did not reach the child intact: {seen:?}"
    );
}

#[tokio::test]
async fn a_websocket_without_credentials_is_refused() {
    let server = Server::start(&["-c", "user:pass", "bash"]);
    assert!(
        connect_ws(&server.ws_url("/ws"), &[]).await.is_err(),
        "the upgrade must be refused without credentials"
    );
}

#[tokio::test]
async fn the_opening_frame_must_carry_a_matching_token() {
    let server = Server::start(&["-c", "user:pass", "-W", "sh", "-c", "echo started; sleep 5"]);
    // base64("user:pass")
    let auth = "Basic dXNlcjpwYXNz";
    let mut ws = connect_ws(&server.ws_url("/ws"), &[("Authorization", auth)])
        .await
        .expect("connect");

    open_terminal(&mut ws, 80, 24, Some("wrong-token"))
        .await
        .expect("open");
    let (output, ending) = drain_until_close(&mut ws, SHORT).await;
    assert!(
        !String::from_utf8_lossy(&output).contains("started"),
        "a bad token still started a process"
    );
    assert_ne!(
        ending,
        Ending::Timeout,
        "the session should have been closed"
    );
}

#[tokio::test]
async fn a_matching_token_opens_the_terminal() {
    let server = Server::start(&["-c", "user:pass", "-W", "sh", "-c", "echo started; sleep 5"]);
    let auth = "Basic dXNlcjpwYXNz";
    let mut ws = connect_ws(&server.ws_url("/ws"), &[("Authorization", auth)])
        .await
        .expect("connect");

    open_terminal(&mut ws, 80, 24, Some("dXNlcjpwYXNz"))
        .await
        .expect("open");
    let seen = read_until(&mut ws, "started", LONG).await;
    assert!(seen.contains("started"), "got {seen:?}");
}

#[tokio::test]
async fn max_clients_limits_concurrent_sessions() {
    let server = Server::start(&["-m", "1", "-W", "sh", "-c", "sleep 30"]);

    let mut first = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_session(&mut first, 80, 24).await;

    assert!(
        connect_ws(&server.ws_url("/ws"), &[]).await.is_err(),
        "a second client was admitted despite --max-clients 1"
    );
}

#[tokio::test]
async fn once_serves_a_single_client_then_exits() {
    let mut server = Server::start(&["-o", "-W", "sh", "-c", "exit 0"]);

    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_terminal(&mut ws, 80, 24, None).await.expect("open");
    let _ = drain_until_close(&mut ws, LONG).await;
    drop(ws);

    assert_eq!(
        server.wait_for_exit(Duration::from_secs(10)),
        Some(0),
        "--once should exit after the client disconnects"
    );
}

#[tokio::test]
async fn exit_no_conn_exits_when_the_last_client_leaves() {
    let mut server = Server::start(&["-q", "-W", "sh", "-c", "exit 0"]);

    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_terminal(&mut ws, 80, 24, None).await.expect("open");
    let _ = drain_until_close(&mut ws, LONG).await;
    drop(ws);

    assert_eq!(
        server.wait_for_exit(Duration::from_secs(10)),
        Some(0),
        "--exit-no-conn should exit once no clients remain"
    );
}

#[tokio::test]
async fn check_origin_rejects_a_foreign_origin() {
    let server = Server::start(&["-O", "bash"]);
    assert!(
        connect_ws(&server.ws_url("/ws"), &[("Origin", "http://evil.example")])
            .await
            .is_err(),
        "a foreign origin must be refused"
    );
}

#[tokio::test]
async fn check_origin_accepts_a_matching_origin() {
    let server = Server::start(&["-O", "bash"]);
    let origin = format!("http://127.0.0.1:{}", server.port);
    assert!(
        connect_ws(&server.ws_url("/ws"), &[("Origin", &origin)])
            .await
            .is_ok(),
        "a matching origin must be accepted"
    );
}

#[tokio::test]
async fn an_origin_carrying_the_other_schemes_default_port_is_accepted() {
    // `check_host_origin` drops `:80` and `:443` whatever the scheme, so `https://host:80`
    // compares equal to `Host: host`. Dropping only the scheme's own default rejected it,
    // which this asserts against both builds so the two cannot drift apart again.
    // `Host` is overridden so it carries no port; otherwise it would carry the random test
    // port and none of the default-port cases could arise at all.
    let server = Server::start(&["-O", "bash"]);
    for (origin, expected) in [
        ("http://127.0.0.1", true),
        ("http://127.0.0.1:80", true),
        ("https://127.0.0.1:443", true),
        ("https://127.0.0.1:80", true),
        ("http://127.0.0.1:443", true),
        ("http://127.0.0.1:8080", false),
    ] {
        let accepted = connect_ws(
            &server.ws_url("/ws"),
            &[("Origin", origin), ("Host", "127.0.0.1")],
        )
        .await
        .is_ok();
        assert_eq!(
            accepted, expected,
            "origin {origin} against Host: 127.0.0.1"
        );
    }
}

#[tokio::test]
async fn an_origin_with_an_unrecognised_scheme_is_refused() {
    // The C build parses the origin scheme exactly and case-sensitively, so it turns these
    // away. Accepting more than the reference does on a security control is the wrong
    // direction to differ in.
    let server = Server::start(&["-O", "bash"]);
    for origin in ["ftp://127.0.0.1", "HTTP://127.0.0.1", "127.0.0.1", "null"] {
        assert!(
            connect_ws(
                &server.ws_url("/ws"),
                &[("Origin", origin), ("Host", "127.0.0.1")]
            )
            .await
            .is_err(),
            "origin {origin} must be refused"
        );
    }
}

#[tokio::test]
async fn the_websocket_path_follows_the_base_path() {
    let server = Server::start(&["-b", "/mounted", "bash"]);

    let mut ws = connect_ws(&server.ws_url("/mounted/ws"), &[])
        .await
        .expect("connect on the mounted path");
    open_session(&mut ws, 80, 24).await;

    assert!(
        connect_ws(&server.ws_url("/ws"), &[]).await.is_err(),
        "the default path must not serve once a base path is set"
    );
}

#[tokio::test]
async fn closing_the_socket_terminates_the_child_process() {
    let mut server = Server::start(&["-W", "sh", "-c", "sleep 120"]);

    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_session(&mut ws, 80, 24).await;

    let pid = server
        .child_pid(Duration::from_secs(10))
        .expect("the server never reported starting a process");
    assert!(common::process_exists(pid), "the child was never running");

    drop(ws);

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while common::process_exists(pid) && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        !common::process_exists(pid),
        "child process {pid} outlived its WebSocket session"
    );
}
