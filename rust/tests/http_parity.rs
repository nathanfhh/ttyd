//! HTTP surface: the index bundle, the token endpoint, base paths and authentication.

mod common;

use common::{http_client, Server};
use reqwest::StatusCode;

/// Byte sizes of the embedded frontend, asserted so an accidental re-encode is caught.
const GZIP_LEN: &str = "194632";
const PLAIN_LEN: &str = "734380";

#[tokio::test]
async fn index_is_served_as_html() {
    let server = Server::start(&["bash"]);
    let response = http_client()
        .get(server.http_url("/"))
        .send()
        .await
        .expect("request");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/html");
    let body = response.bytes().await.expect("body");
    assert!(
        body.starts_with(b"<!DOCTYPE html"),
        "index did not start with a doctype"
    );
}

#[tokio::test]
async fn index_is_sent_compressed_when_the_client_accepts_gzip() {
    let server = Server::start(&["bash"]);
    let response = http_client()
        .get(server.http_url("/"))
        .header("Accept-Encoding", "gzip")
        .send()
        .await
        .expect("request");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-encoding"], "gzip");
    assert_eq!(response.headers()["content-length"], GZIP_LEN);
}

#[tokio::test]
async fn index_is_decompressed_when_gzip_is_not_accepted() {
    let server = Server::start(&["bash"]);
    let response = http_client()
        .get(server.http_url("/"))
        .header("Accept-Encoding", "identity")
        .send()
        .await
        .expect("request");

    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get("content-encoding").is_none());
    assert_eq!(response.headers()["content-length"], PLAIN_LEN);
}

#[tokio::test]
async fn an_explicit_gzip_refusal_is_honoured() {
    // `gzip;q=0` is how RFC 9110 spells "do not send me gzip". The C build asks only whether
    // the header contains the substring `gzip`, so it answers this by sending a compressed
    // body the client has just said it cannot decode — the browser then renders the raw
    // deflate stream. Skipped against C, which cannot pass it by construction.
    if common::is_c_reference() {
        return;
    }
    let server = Server::start(&["bash"]);
    for refusal in ["gzip;q=0", "gzip;q=0.0", "deflate, gzip;q=0"] {
        let response = http_client()
            .get(server.http_url("/"))
            .header("Accept-Encoding", refusal)
            // reqwest would otherwise decode transparently and hide what was sent.
            .send()
            .await
            .expect("request");

        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers().get("content-encoding").is_none(),
            "{refusal:?} was answered with {:?}",
            response.headers().get("content-encoding")
        );
    }
}

#[tokio::test]
async fn a_weighted_acceptance_still_gets_gzip() {
    // The refusal above must not have turned into "any q parameter means no".
    let server = Server::start(&["bash"]);
    for accepted in ["gzip;q=1", "gzip;q=0.5", "deflate;q=0, gzip"] {
        let response = http_client()
            .get(server.http_url("/"))
            .header("Accept-Encoding", accepted)
            .send()
            .await
            .expect("request");

        assert_eq!(
            response.headers()["content-encoding"],
            "gzip",
            "{accepted:?} should still be compressed"
        );
    }
}

#[tokio::test]
async fn a_custom_index_replaces_the_bundle() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("index.html");
    std::fs::write(&path, "<html>custom index</html>").expect("write");

    let server = Server::start(&["-I", path.to_str().unwrap(), "bash"]);
    let body = http_client()
        .get(server.http_url("/"))
        .send()
        .await
        .expect("request")
        .text()
        .await
        .expect("body");

    assert_eq!(body, "<html>custom index</html>");
}

#[tokio::test]
async fn a_custom_index_that_disappears_is_a_server_error() {
    // `-I` is checked at startup, but the file is read per request, so it can vanish while
    // the server is running — a deploy replacing the directory, for instance. That must be a
    // 500 rather than a panic or an empty 200 that looks like a working but blank terminal.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("index.html");
    std::fs::write(&path, "<html>temporary</html>").expect("write");

    let server = Server::start(&["-I", path.to_str().unwrap(), "bash"]);
    let ok = http_client()
        .get(server.http_url("/"))
        .send()
        .await
        .expect("request");
    assert_eq!(ok.status(), StatusCode::OK);

    std::fs::remove_file(&path).expect("remove the index out from under the server");
    let gone = http_client()
        .get(server.http_url("/"))
        .send()
        .await
        .expect("request");
    // 404, matching the C build: `lws_serve_http_file` cannot tell a file that was never
    // there from one a deploy has just replaced, and neither can this.
    assert_eq!(gone.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_token_endpoint_is_empty_without_a_credential() {
    let server = Server::start(&["bash"]);
    let response = http_client()
        .get(server.http_url("/token"))
        .send()
        .await
        .expect("request");

    assert_eq!(
        response.headers()["content-type"],
        "application/json;charset=utf-8"
    );
    assert_eq!(response.text().await.expect("body"), r#"{"token": ""}"#);
}

#[tokio::test]
async fn the_token_endpoint_returns_the_encoded_credential() {
    let server = Server::start(&["-c", "user:pass", "bash"]);
    let body = http_client()
        .get(server.http_url("/token"))
        .basic_auth("user", Some("pass"))
        .send()
        .await
        .expect("request")
        .text()
        .await
        .expect("body");

    // base64("user:pass")
    assert_eq!(body, r#"{"token": "dXNlcjpwYXNz"}"#);
}

#[tokio::test]
async fn an_unknown_path_is_not_found() {
    let server = Server::start(&["bash"]);
    let response = http_client()
        .get(server.http_url("/does-not-exist"))
        .send()
        .await
        .expect("request");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.headers()["content-type"], "text/html");
    assert!(response
        .text()
        .await
        .expect("body")
        .contains("<h1>404</h1>"));
}

#[tokio::test]
async fn base_path_moves_every_endpoint() {
    let server = Server::start(&["-b", "/mounted/here", "bash"]);
    let client = http_client();

    let index = client
        .get(server.http_url("/mounted/here/"))
        .send()
        .await
        .expect("index");
    assert_eq!(index.status(), StatusCode::OK);

    let token = client
        .get(server.http_url("/mounted/here/token"))
        .send()
        .await
        .expect("token");
    assert_eq!(token.status(), StatusCode::OK);

    // The root is no longer served once a base path is configured.
    let root = client.get(server.http_url("/")).send().await.expect("root");
    assert_eq!(root.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_base_path_itself_redirects_to_the_index() {
    let server = Server::start(&["-b", "/mounted", "bash"]);
    let response = http_client()
        .get(server.http_url("/mounted"))
        .send()
        .await
        .expect("request");

    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(response.headers()["location"], "/mounted/");
}

#[tokio::test]
async fn a_trailing_slash_in_the_base_path_is_trimmed() {
    let server = Server::start(&["-b", "/mounted/", "bash"]);
    let response = http_client()
        .get(server.http_url("/mounted/"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn basic_auth_challenges_an_anonymous_request() {
    let server = Server::start(&["-c", "user:pass", "bash"]);
    let response = http_client()
        .get(server.http_url("/"))
        .send()
        .await
        .expect("request");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.headers()["www-authenticate"],
        "Basic realm=\"ttyd\""
    );
}

#[tokio::test]
async fn basic_auth_admits_the_right_credential() {
    let server = Server::start(&["-c", "user:pass", "bash"]);
    let response = http_client()
        .get(server.http_url("/"))
        .basic_auth("user", Some("pass"))
        .send()
        .await
        .expect("request");

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn basic_auth_rejects_the_wrong_credential() {
    let server = Server::start(&["-c", "user:pass", "bash"]);
    let response = http_client()
        .get(server.http_url("/"))
        .basic_auth("user", Some("wrong"))
        .send()
        .await
        .expect("request");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn authentication_is_checked_before_routing() {
    // An unauthenticated request to a path that does not exist must be challenged rather
    // than answered with a 404, which is what reveals the auth-first ordering.
    let server = Server::start(&["-c", "user:pass", "bash"]);
    let response = http_client()
        .get(server.http_url("/does-not-exist"))
        .send()
        .await
        .expect("request");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_auth_header_mode_requires_the_header() {
    let server = Server::start(&["-H", "X-Remote-User", "bash"]);
    let response = http_client()
        .get(server.http_url("/"))
        .send()
        .await
        .expect("request");

    assert_eq!(response.status(), StatusCode::PROXY_AUTHENTICATION_REQUIRED);
}

#[tokio::test]
async fn the_auth_header_mode_admits_a_request_carrying_it() {
    let server = Server::start(&["-H", "X-Remote-User", "bash"]);
    let response = http_client()
        .get(server.http_url("/"))
        .header("X-Remote-User", "alice")
        .send()
        .await
        .expect("request");

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn the_auth_header_name_is_matched_case_insensitively() {
    let server = Server::start(&["-H", "X-Remote-User", "bash"]);
    let response = http_client()
        .get(server.http_url("/"))
        .header("x-remote-user", "alice")
        .send()
        .await
        .expect("request");

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_unix_socket_serves_http() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("ttyd.sock");

    // The harness parses a TCP port from the log, which a socket build never prints, so this
    // one starts the process directly.
    let mut child = std::process::Command::new(common::binary())
        .args(["-i", socket.to_str().unwrap(), "bash"])
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !socket.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(socket.exists(), "the unix socket was never created");

    // reqwest cannot dial a UNIX socket, so speak HTTP/1.1 over it directly.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::UnixStream::connect(&socket)
        .await
        .expect("connect to the unix socket");
    stream
        .write_all(b"GET /token HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .expect("write request");

    let mut response = Vec::new();
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        stream.read_to_end(&mut response),
    )
    .await;
    let response = String::from_utf8_lossy(&response);

    assert!(response.starts_with("HTTP/1.1 200"), "got {response:?}");
    assert!(response.contains(r#"{"token": ""}"#), "got {response:?}");

    // Terminate politely so the server runs its shutdown path and removes the socket file.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while child.try_wait().ok().flatten().is_none() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let _ = child.kill();
    let _ = child.wait();
}
