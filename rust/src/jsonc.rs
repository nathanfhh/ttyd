//! Serialisation that reproduces json-c's default `JSON_C_TO_STRING_SPACED` layout.
//!
//! The client preferences blob is pushed to the browser verbatim, so matching json-c's
//! spacing (`{ "key": value }`, `{ }` when empty) keeps the Rust port byte-compatible
//! with the C implementation on the wire.

use serde_json::ser::Formatter;
use std::io;

#[derive(Default)]
pub struct SpacedFormatter;

impl Formatter for SpacedFormatter {
    fn begin_object<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        w.write_all(b"{")
    }

    fn end_object<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        w.write_all(b" }")
    }

    fn begin_object_key<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> io::Result<()> {
        w.write_all(if first { b" " } else { b", " })
    }

    fn begin_object_value<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        w.write_all(b": ")
    }

    fn begin_array<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        w.write_all(b"[")
    }

    fn end_array<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        w.write_all(b" ]")
    }

    fn begin_array_value<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> io::Result<()> {
        w.write_all(if first { b" " } else { b", " })
    }
}

/// Renders a JSON value the way json-c's `json_object_to_json_string` would.
pub fn to_string(value: &serde_json::Value) -> String {
    let mut buf = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, SpacedFormatter);
    serde::Serialize::serialize(value, &mut ser).expect("serializing to a Vec cannot fail");
    String::from_utf8(buf).expect("serde_json emits valid UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_object_matches_json_c() {
        assert_eq!(to_string(&json!({})), "{ }");
    }

    #[test]
    fn populated_object_matches_json_c() {
        let mut map = serde_json::Map::new();
        map.insert("fontSize".into(), json!(20));
        map.insert("titleFixed".into(), json!("hi"));
        assert_eq!(
            to_string(&serde_json::Value::Object(map)),
            r#"{ "fontSize": 20, "titleFixed": "hi" }"#
        );
    }

    #[test]
    fn nested_values_match_json_c() {
        let mut map = serde_json::Map::new();
        map.insert("theme".into(), json!({"background": "red"}));
        map.insert("list".into(), json!([1, 2]));
        assert_eq!(
            to_string(&serde_json::Value::Object(map)),
            r#"{ "theme": { "background": "red" }, "list": [ 1, 2 ] }"#
        );
    }
}
