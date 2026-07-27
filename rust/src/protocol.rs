//! The `tty` WebSocket subprotocol.
//!
//! Every frame is binary and begins with a single command byte. The remainder is the
//! payload, whose meaning depends on that byte. These values are fixed by the frontend
//! bundle, so they must stay identical to the C `src/server.h` definitions.

/// The WebSocket subprotocol name negotiated during the handshake.
pub const SUBPROTOCOL: &str = "tty";

// Client to server.
/// Keystrokes to write to the terminal.
pub const INPUT: u8 = b'0';
/// A JSON `{"columns":N,"rows":N}` payload resizing the terminal.
pub const RESIZE_TERMINAL: u8 = b'1';
/// Stop reading from the terminal until resumed.
pub const PAUSE: u8 = b'2';
/// Resume reading from the terminal.
pub const RESUME: u8 = b'3';
/// The opening handshake payload; doubles as its own command byte because it is raw JSON.
pub const JSON_DATA: u8 = b'{';

// Server to client.
/// Terminal output.
pub const OUTPUT: u8 = b'0';
/// The window title to display.
pub const SET_WINDOW_TITLE: u8 = b'1';
/// The client preferences JSON blob.
pub const SET_PREFERENCES: u8 = b'2';

/// WebSocket close code sent when the child process exited cleanly.
pub const CLOSE_NORMAL: u16 = 1000;

/// Builds a server frame from a command byte and its payload.
pub fn frame(command: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 1);
    out.push(command);
    out.extend_from_slice(payload);
    out
}

/// The terminal size carried by `RESIZE_TERMINAL` and the opening `JSON_DATA` frame.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
pub struct WindowSize {
    #[serde(default)]
    pub columns: u16,
    #[serde(default)]
    pub rows: u16,
}

/// The opening frame the browser sends once the socket is up.
#[derive(Debug, Default, Clone, serde::Deserialize)]
pub struct OpenMessage {
    #[serde(default)]
    pub columns: u16,
    #[serde(default)]
    pub rows: u16,
    #[serde(rename = "AuthToken", default)]
    pub auth_token: Option<String>,
}

/// Parses a window size, tolerating malformed input the way the C version does — json-c
/// silently yields zero for missing or non-numeric fields, and zero means "leave unchanged".
pub fn parse_window_size(payload: &[u8]) -> WindowSize {
    serde_json::from_slice::<WindowSize>(payload).unwrap_or_default()
}

pub fn parse_open_message(payload: &[u8]) -> OpenMessage {
    serde_json::from_slice::<OpenMessage>(payload).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_prefix_the_command_byte() {
        assert_eq!(frame(OUTPUT, b"hi"), b"0hi");
        assert_eq!(frame(SET_WINDOW_TITLE, b"t"), b"1t");
        assert_eq!(frame(SET_PREFERENCES, b"{ }"), b"2{ }");
    }

    #[test]
    fn window_size_parses_both_fields() {
        let size = parse_window_size(br#"{"columns":120,"rows":40}"#);
        assert_eq!(
            size,
            WindowSize {
                columns: 120,
                rows: 40
            }
        );
    }

    #[test]
    fn malformed_window_size_yields_zeroes() {
        assert_eq!(parse_window_size(b"not json"), WindowSize::default());
        assert_eq!(parse_window_size(b"{}"), WindowSize::default());
    }

    #[test]
    fn open_message_carries_the_auth_token() {
        let msg = parse_open_message(br#"{"columns":80,"rows":24,"AuthToken":"abc"}"#);
        assert_eq!(msg.columns, 80);
        assert_eq!(msg.rows, 24);
        assert_eq!(msg.auth_token.as_deref(), Some("abc"));
    }

    #[test]
    fn open_message_without_a_token_parses() {
        let msg = parse_open_message(br#"{"columns":80,"rows":24}"#);
        assert_eq!(msg.auth_token, None);
    }
}
