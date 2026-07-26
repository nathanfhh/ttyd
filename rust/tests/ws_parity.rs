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
use std::time::Duration;

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
    let mut ws = connect_ws(&server.ws_url("/ws?arg=first&arg=second"), &[])
        .await
        .expect("connect");
    open_session(&mut ws, 80, 24).await;

    let seen = read_until(&mut ws, "first second", LONG).await;
    assert!(seen.contains("first second"), "got {seen:?}");
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
    let mut ws = connect_ws(&server.ws_url("/ws"), &[("X-Remote-User", "alice")])
        .await
        .expect("connect");
    open_session(&mut ws, 80, 24).await;

    let seen = read_until(&mut ws, "USER=alice", LONG).await;
    assert!(seen.contains("USER=alice"), "got {seen:?}");
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
