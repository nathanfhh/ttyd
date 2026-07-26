//! The WebSocket endpoint: terminal sessions.
//!
//! One socket owns exactly one child process. Output is pumped from the PTY reader channel,
//! which is bounded, so a client that stops reading eventually stalls the reader thread and
//! the child blocks on write. That is the same backpressure the C version gets from libuv's
//! explicit read pause/resume.

use crate::cli::AuthMode;
use crate::http::AuthenticatedUser;
use crate::protocol::{self, OpenMessage};
use crate::pty::{self, ExitInfo, SpawnRequest};
use crate::state::{AppState, ConnInfo};
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Extension;
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::Duration;
use subtle::ConstantTimeEq;

/// How long to keep draining terminal output after the child has been reaped, so the last
/// bytes it wrote still reach the browser.
const DRAIN_AFTER_EXIT: Duration = Duration::from_millis(250);

/// How long to wait for the child to be reaped once its terminal has closed, so the close
/// code reflects the real exit status.
const EXIT_STATUS_GRACE: Duration = Duration::from_secs(2);

/// Terminal size used until the browser reports its own, matching the C `process_init()`.
const DEFAULT_COLUMNS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

pub async fn handler(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthenticatedUser>,
    conn: Option<Extension<ConnInfo>>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    upgrade: WebSocketUpgrade,
) -> Response {
    // The accept loop records the peer; without it the only log line naming who opened a
    // terminal would report every client as a UNIX socket.
    let conn = conn.map(|Extension(conn)| conn).unwrap_or_default();

    if state.cfg.check_origin && !origin_matches_host(&headers) {
        tracing::warn!(
            "refuse to serve WS client from different origin due to the --check-origin option."
        );
        return StatusCode::FORBIDDEN.into_response();
    }

    let Some(slot) = ClientSlot::acquire(&state) else {
        if state.cfg.once {
            tracing::warn!("refuse to serve WS client due to the --once option.");
        } else {
            tracing::warn!("refuse to serve WS client due to the --max-clients option.");
        }
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };

    let args = if state.cfg.url_arg {
        parse_url_args(query.as_deref())
    } else {
        Vec::new()
    };

    tracing::info!(
        "WS {} - {}, clients: {}",
        state.cfg.endpoints.ws,
        conn.peer_display(),
        state.client_count()
    );

    upgrade
        .protocols([protocol::SUBPROTOCOL])
        .on_upgrade(move |socket| async move {
            session(socket, state, user.0, args, slot).await;
        })
}

/// Holds a client slot for as long as the session lives, releasing it even if the socket
/// dies before the session task finishes.
struct ClientSlot {
    state: Arc<AppState>,
    released: bool,
}

impl ClientSlot {
    fn acquire(state: &Arc<AppState>) -> Option<Self> {
        state.try_acquire_client().then(|| Self {
            state: state.clone(),
            released: false,
        })
    }

    /// Releases the slot and reports how many clients remain.
    fn release(&mut self) -> i64 {
        if self.released {
            return self.state.client_count();
        }
        self.released = true;
        self.state.release_client()
    }
}

impl Drop for ClientSlot {
    fn drop(&mut self) {
        self.release();
    }
}

async fn session(
    socket: WebSocket,
    state: Arc<AppState>,
    user: Option<String>,
    url_args: Vec<String>,
    mut slot: ClientSlot,
) {
    let (mut sink, mut stream) = socket.split();

    // Phase one: nothing is sent and nothing runs until the browser sends its opening JSON
    // frame. Holding the title back matters — it contains the full command line, which an
    // unauthenticated client must not be able to read just by opening a socket.
    let Some(spawned) = await_open(&mut stream, &state, user, &url_args).await else {
        finish(&mut slot, &state, None).await;
        return;
    };
    let pty::Spawned {
        mut pty,
        mut output,
        mut exit,
    } = spawned;
    state.register_child(pty.pid);
    tracing::info!("started process, pid: {}", pty.pid);

    let title = state.cfg.window_title();
    for (command, payload) in [
        (protocol::SET_WINDOW_TITLE, title.as_bytes()),
        (protocol::SET_PREFERENCES, state.cfg.prefs_json.as_bytes()),
    ] {
        if sink
            .send(Message::Binary(protocol::frame(command, payload).into()))
            .await
            .is_err()
        {
            if pty.is_running() {
                pty.kill(state.cfg.sig_code);
            }
            state.unregister_child(pty.pid);
            finish(&mut slot, &state, Some(&pty)).await;
            return;
        }
    }

    // Idle sessions are kept alive with WebSocket pings, which is what stops reverse proxies
    // and NAT devices from dropping a terminal that nobody is typing into. A peer that stops
    // answering entirely is hung up on, matching the C retry policy of `interval + 7`.
    let ping_every = Duration::from_secs(state.cfg.ping_interval);
    let hangup_after = ping_every + Duration::from_secs(7);
    let pings_enabled = state.cfg.ping_interval > 0;
    let mut ping_timer = tokio::time::interval_at(
        tokio::time::Instant::now() + ping_every.max(Duration::from_secs(1)),
        ping_every.max(Duration::from_secs(1)),
    );
    ping_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_seen = tokio::time::Instant::now();

    let mut paused = false;
    let mut exit_info: Option<ExitInfo> = None;
    let mut drain_deadline: Option<tokio::time::Instant> = None;
    // Distinguishes "the terminal reached end of file" from "the browser hung up", because
    // only the former is worth waiting on an exit status for.
    let mut output_ended = false;

    loop {
        let drain_timer = async {
            match drain_deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };

        tokio::select! {
            _ = ping_timer.tick(), if pings_enabled => {
                if last_seen.elapsed() > hangup_after {
                    tracing::info!("closing idle session, no response for {hangup_after:?}");
                    break;
                }
                if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
            incoming = stream.next() => {
                last_seen = tokio::time::Instant::now();
                match incoming {
                    Some(Ok(Message::Binary(data))) => {
                        if !handle_client_message(&data, &state, &mut pty, &mut paused) {
                            break;
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        if !handle_client_message(text.as_bytes(), &state, &mut pty, &mut paused) {
                            break;
                        }
                    }
                    Some(Ok(_)) => {}
                    // Client went away, or the socket failed.
                    Some(Err(_)) | None => break,
                }
            }
            chunk = output.recv(), if !paused => {
                match chunk {
                    Some(data) => {
                        let frame = protocol::frame(protocol::OUTPUT, &data);
                        if sink.send(Message::Binary(frame.into())).await.is_err() {
                            break;
                        }
                    }
                    // End of terminal output: the session is over.
                    None => {
                        output_ended = true;
                        break;
                    }
                }
            }
            reaped = &mut exit, if exit_info.is_none() => {
                if let Ok(info) = reaped {
                    tracing::info!("process exited with code {}, pid: {}", info.code, pty.pid);
                    exit_info = Some(info);
                }
                // Keep pumping briefly so trailing output is not lost, then give up.
                drain_deadline = Some(tokio::time::Instant::now() + DRAIN_AFTER_EXIT);
            }
            _ = drain_timer => break,
            // A server-wide shutdown must take the child down with it, the same way
            // destroying the libwebsockets context closes every session in the C build.
            _ = state.wait_for_shutdown() => {
                tracing::info!("shutting down session, pid: {}", pty.pid);
                break;
            }
        }
    }

    // The terminal usually reaches end of file before the child is reaped, so the exit
    // status that decides the close code may still be in flight.
    if output_ended && exit_info.is_none() {
        if let Ok(Ok(info)) = tokio::time::timeout(EXIT_STATUS_GRACE, &mut exit).await {
            tracing::info!("process exited with code {}, pid: {}", info.code, pty.pid);
            exit_info = Some(info);
        }
    }

    // A clean exit tells the frontend not to reconnect; anything else must not, so the
    // socket is dropped without a close frame and the browser reports code 1006.
    let clean = exit_info.map(|i| i.success()).unwrap_or(false);
    if clean {
        let _ = sink
            .send(Message::Close(Some(CloseFrame {
                code: protocol::CLOSE_NORMAL,
                reason: "".into(),
            })))
            .await;
    }

    if pty.is_running() {
        tracing::info!("killing process, pid: {}", pty.pid);
        pty.kill(state.cfg.sig_code);
    }
    state.unregister_child(pty.pid);

    finish(&mut slot, &state, Some(&pty)).await;
}

/// Applies the `--once` / `--exit-no-conn` lifecycle rules once a session ends.
async fn finish(slot: &mut ClientSlot, state: &Arc<AppState>, pty: Option<&pty::Pty>) {
    let remaining = slot.release();
    tracing::info!("WS closed, clients: {remaining}");

    if !(state.cfg.once || state.cfg.exit_no_conn) || remaining > 0 {
        return;
    }

    tracing::info!("exiting due to the --once/--exit-no-conn option.");
    state.begin_shutdown();

    if pty.is_some_and(|p| p.is_running()) {
        // Wait for the child to actually die before tearing the process down.
        state.set_force_exit();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            if !pty.is_some_and(|p| p.is_running()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    std::process::exit(0);
}

/// Waits for the opening `{...}` frame, validates the token when basic auth is configured,
/// and spawns the child process.
async fn await_open(
    stream: &mut futures_util::stream::SplitStream<WebSocket>,
    state: &Arc<AppState>,
    user: Option<String>,
    url_args: &[String],
) -> Option<pty::Spawned> {
    while let Some(Ok(message)) = stream.next().await {
        let data = match &message {
            Message::Binary(data) => data.as_ref(),
            Message::Text(text) => text.as_bytes(),
            _ => continue,
        };
        let Some(&command) = data.first() else {
            continue;
        };

        if command != protocol::JSON_DATA {
            // Before the handshake completes only the opening frame is acceptable when a
            // credential is configured; the C version drops the connection here.
            if state.cfg.requires_ws_token() {
                tracing::warn!("WS client not authenticated");
                return None;
            }
            // Without a process there is nothing an input frame could be written to.
            if command == protocol::INPUT && state.cfg.writable {
                tracing::error!("input received before the terminal was opened");
                return None;
            }
            continue;
        }

        let open: OpenMessage = protocol::parse_open_message(data);

        if let Some(expected) = state.cfg.credential() {
            let presented = open.auth_token.clone().unwrap_or_default();
            let ok: bool = presented.as_bytes().ct_eq(expected.as_bytes()).into();
            if !ok {
                tracing::warn!("WS authentication failed with token: {presented}");
                return None;
            }
        }

        return spawn_child(state, user, url_args, open.columns, open.rows);
    }
    None
}

fn spawn_child(
    state: &Arc<AppState>,
    user: Option<String>,
    url_args: &[String],
    columns: u16,
    rows: u16,
) -> Option<pty::Spawned> {
    let mut argv = state.cfg.argv.clone();
    argv.extend_from_slice(url_args);

    let mut env = vec![("TERM".to_string(), state.cfg.terminal_type.clone())];
    if let Some(user) = user.filter(|u| !u.is_empty()) {
        env.push(("TTYD_USER".to_string(), user));
    }

    let request = SpawnRequest {
        argv: &argv,
        env: &env,
        cwd: state.cfg.cwd.as_deref(),
        read_chunk: state.cfg.srv_buf_size,
        columns: if columns > 0 {
            columns
        } else {
            DEFAULT_COLUMNS
        },
        rows: if rows > 0 { rows } else { DEFAULT_ROWS },
    };

    match pty::spawn(request) {
        Ok(spawned) => Some(spawned),
        Err(e) => {
            tracing::error!("pty_spawn: {e:#}");
            None
        }
    }
}

/// Handles one client frame. Returns false when the session must end.
fn handle_client_message(
    data: &[u8],
    state: &Arc<AppState>,
    pty: &mut pty::Pty,
    paused: &mut bool,
) -> bool {
    let Some((&command, payload)) = data.split_first() else {
        return true;
    };

    match command {
        protocol::INPUT => {
            if state.cfg.writable && !pty.write(payload.to_vec()) {
                return false;
            }
        }
        protocol::RESIZE_TERMINAL => {
            let size = protocol::parse_window_size(payload);
            let columns = if size.columns > 0 {
                size.columns
            } else {
                pty.columns
            };
            let rows = if size.rows > 0 { size.rows } else { pty.rows };
            pty.resize(columns, rows);
        }
        protocol::PAUSE => *paused = true,
        protocol::RESUME => *paused = false,
        // A second opening frame is ignored; the process is already running.
        protocol::JSON_DATA => {}
        other => tracing::warn!("ignored unknown message type: {}", other as char),
    }
    true
}

/// Extracts repeated `arg=` query parameters, preserving their order.
///
/// Values are percent-decoded before they reach the child, matching libwebsockets, which
/// hands the C implementation already-decoded fragments. Passing the raw form through would
/// silently corrupt any argument containing a space or a reserved character.
fn parse_url_args(query: Option<&str>) -> Vec<String> {
    let Some(query) = query else {
        return Vec::new();
    };
    query
        .split('&')
        .filter_map(|pair| pair.strip_prefix("arg="))
        .map(decode_query_value)
        .collect()
}

/// Decodes one query-string value: `+` means a space, `%XX` is a byte escape. Invalid
/// escapes are left as written rather than dropped, so nothing silently disappears.
fn decode_query_value(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => match u8::from_str_radix(&raw[i + 1..i + 3], 16) {
                Ok(byte) => {
                    out.push(byte);
                    i += 3;
                }
                Err(_) => {
                    out.push(bytes[i]);
                    i += 1;
                }
            },
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Compares the `Origin` header against `Host`, the way `--check-origin` does in C.
fn origin_matches_host(headers: &HeaderMap) -> bool {
    let Some(origin) = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Some(host) = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    normalize_origin(origin).is_some_and(|o| o.eq_ignore_ascii_case(host))
}

/// Reduces an origin URL to `host` or `host:port`, dropping the port when it is the default
/// for the scheme so that it can be compared with a `Host` header.
fn normalize_origin(origin: &str) -> Option<String> {
    let (scheme, rest) = origin.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    if authority.is_empty() {
        return None;
    }
    let default_port = match scheme.to_ascii_lowercase().as_str() {
        "https" | "wss" => 443,
        _ => 80,
    };

    // Only a trailing `:port` counts; an IPv6 literal keeps its brackets and colons.
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p))
            if !h.ends_with(']') && !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) =>
        {
            (h, p.parse::<u16>().ok()?)
        }
        _ => (authority, default_port),
    };

    if port == default_port {
        Some(host.to_string())
    } else {
        Some(format!("{host}:{port}"))
    }
}

/// Whether the WebSocket layer must additionally verify the `AuthToken` field.
pub fn ws_token_required(mode: &AuthMode) -> bool {
    matches!(mode, AuthMode::Basic { .. })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderName, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        map
    }

    #[test]
    fn url_args_are_percent_decoded() {
        assert_eq!(
            parse_url_args(Some("arg=hello%20world&arg=a%2Bb&arg=caf%C3%A9")),
            vec!["hello world", "a+b", "café"]
        );
        assert_eq!(parse_url_args(Some("arg=one+two")), vec!["one two"]);
    }

    #[test]
    fn a_malformed_escape_is_left_alone() {
        // Dropping it would silently change the argument; keeping it is the visible failure.
        assert_eq!(parse_url_args(Some("arg=100%zz")), vec!["100%zz"]);
        assert_eq!(parse_url_args(Some("arg=trailing%")), vec!["trailing%"]);
    }

    #[test]
    fn url_args_are_collected_in_order() {
        assert_eq!(
            parse_url_args(Some("arg=foo&arg=bar&other=x")),
            vec!["foo", "bar"]
        );
        assert_eq!(parse_url_args(Some("other=x")), Vec::<String>::new());
        assert_eq!(parse_url_args(None), Vec::<String>::new());
    }

    #[test]
    fn default_ports_are_dropped_from_the_origin() {
        assert_eq!(
            normalize_origin("http://example.com").unwrap(),
            "example.com"
        );
        assert_eq!(
            normalize_origin("http://example.com:80").unwrap(),
            "example.com"
        );
        assert_eq!(
            normalize_origin("https://example.com:443").unwrap(),
            "example.com"
        );
        assert_eq!(
            normalize_origin("http://example.com:8080").unwrap(),
            "example.com:8080"
        );
    }

    #[test]
    fn origin_must_match_host() {
        let same = headers(&[
            ("origin", "http://localhost:7681"),
            ("host", "localhost:7681"),
        ]);
        assert!(origin_matches_host(&same));

        let different = headers(&[("origin", "http://evil.test"), ("host", "localhost:7681")]);
        assert!(!origin_matches_host(&different));
    }

    #[test]
    fn missing_origin_fails_the_check() {
        let only_host = headers(&[("host", "localhost:7681")]);
        assert!(!origin_matches_host(&only_host));
    }

    #[test]
    fn ipv6_origins_keep_their_brackets() {
        assert_eq!(normalize_origin("http://[::1]:7681").unwrap(), "[::1]:7681");
        assert_eq!(normalize_origin("http://[::1]").unwrap(), "[::1]");
    }
}
