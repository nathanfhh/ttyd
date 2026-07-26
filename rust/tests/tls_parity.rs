//! TLS termination, including the plain-HTTP-on-the-TLS-port redirect and client
//! certificate verification.

mod common;

use common::Server;
use reqwest::StatusCode;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn openssl(args: &[&str], what: &str) {
    let status = Command::new("openssl")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("openssl must be available to generate test certificates");
    assert!(status.success(), "openssl failed to {what}");
}

/// A throwaway certificate authority plus a leaf certificate it signed for `127.0.0.1`.
/// rustls refuses to use one self-signed certificate as both root and end entity, so the
/// test needs a real two-level chain.
struct TestPki {
    ca_cert: PathBuf,
    server_cert: PathBuf,
    server_key: PathBuf,
}

fn generate_pki(dir: &Path) -> TestPki {
    let ca_cert = dir.join("ca.crt");
    let ca_key = dir.join("ca.key");
    let server_cert = dir.join("server.crt");
    let server_key = dir.join("server.key");
    let csr = dir.join("server.csr");
    let ext = dir.join("server.ext");

    std::fs::write(&ext, "subjectAltName=IP:127.0.0.1,DNS:localhost\n").expect("write ext file");

    openssl(
        &[
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "1",
            "-subj",
            "/CN=ttyd-test-ca",
            "-keyout",
            ca_key.to_str().unwrap(),
            "-out",
            ca_cert.to_str().unwrap(),
        ],
        "create the test CA",
    );
    openssl(
        &[
            "req",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-subj",
            "/CN=127.0.0.1",
            "-keyout",
            server_key.to_str().unwrap(),
            "-out",
            csr.to_str().unwrap(),
        ],
        "create the server key and request",
    );
    openssl(
        &[
            "x509",
            "-req",
            "-in",
            csr.to_str().unwrap(),
            "-CA",
            ca_cert.to_str().unwrap(),
            "-CAkey",
            ca_key.to_str().unwrap(),
            "-CAcreateserial",
            "-days",
            "1",
            "-extfile",
            ext.to_str().unwrap(),
            "-out",
            server_cert.to_str().unwrap(),
        ],
        "sign the server certificate",
    );

    TestPki {
        ca_cert,
        server_cert,
        server_key,
    }
}

fn tls_client(ca: &Path) -> reqwest::Client {
    let pem = std::fs::read(ca).expect("read CA");
    reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(&pem).expect("parse CA"))
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("client builds")
}

#[tokio::test]
async fn https_serves_the_index() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pki = generate_pki(dir.path());

    let server = Server::start(&[
        "-S",
        "-C",
        pki.server_cert.to_str().unwrap(),
        "-K",
        pki.server_key.to_str().unwrap(),
        "bash",
    ]);

    let response = tls_client(&pki.ca_cert)
        .get(format!("https://127.0.0.1:{}/token", server.port))
        .send()
        .await
        .expect("https request");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.expect("body"), r#"{"token": ""}"#);
}

#[tokio::test]
async fn plain_http_on_the_tls_port_is_redirected() {
    // The C build enables both ALLOW_NON_SSL_ON_SSL_PORT and REDIRECT_HTTP_TO_HTTPS, so a
    // user who types http:// by mistake is sent to the secure URL instead of seeing a
    // protocol error.
    let dir = tempfile::tempdir().expect("tempdir");
    let pki = generate_pki(dir.path());

    let server = Server::start(&[
        "-S",
        "-C",
        pki.server_cert.to_str().unwrap(),
        "-K",
        pki.server_key.to_str().unwrap(),
        "bash",
    ]);

    let response = common::http_client()
        .get(format!("http://127.0.0.1:{}/token", server.port))
        .send()
        .await
        .expect("plain http request");

    assert!(
        response.status().is_redirection(),
        "expected a redirect, got {}",
        response.status()
    );
    let location = response.headers()["location"]
        .to_str()
        .expect("location header");
    assert!(
        location.starts_with("https://"),
        "expected an https target, got {location}"
    );
}

#[tokio::test]
async fn a_redirect_without_a_host_header_is_never_malformed() {
    // HTTP/1.0 has no required Host. Building the target unconditionally produced
    // `Location: https:///token` — a URL no client can follow — so the absence of an
    // authority has to be answered rather than papered over. The C build drops the
    // connection here; either way, what must never happen is a malformed redirect.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let dir = tempfile::tempdir().expect("tempdir");
    let pki = generate_pki(dir.path());
    let server = Server::start(&[
        "-S",
        "-C",
        pki.server_cert.to_str().unwrap(),
        "-K",
        pki.server_key.to_str().unwrap(),
        "bash",
    ]);

    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", server.port))
        .await
        .expect("connect");
    socket
        .write_all(b"GET /token HTTP/1.0\r\n\r\n")
        .await
        .expect("write request");

    let mut response = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), socket.read_to_end(&mut response)).await;
    let response = String::from_utf8_lossy(&response);

    assert!(
        !response.contains("https:///"),
        "emitted a malformed redirect target: {response}"
    );
    // Whatever the answer is, a Location that is present must be a usable absolute URL.
    for line in response.lines() {
        if let Some(target) = line.to_ascii_lowercase().strip_prefix("location:") {
            let target = target.trim().to_string();
            assert!(
                target.starts_with("https://") && target.len() > "https://".len(),
                "Location must name a host, got {target:?}"
            );
        }
    }
}

#[tokio::test]
async fn a_client_certificate_is_required_when_a_ca_is_configured() {
    if common::is_c_reference() {
        // The C build rejects the handshake at a different layer; the assertion below is
        // about this port's rustls verifier.
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let pki = generate_pki(dir.path());

    let server = Server::start(&[
        "-S",
        "-C",
        pki.server_cert.to_str().unwrap(),
        "-K",
        pki.server_key.to_str().unwrap(),
        "-A",
        pki.ca_cert.to_str().unwrap(),
        "bash",
    ]);

    let result = tls_client(&pki.ca_cert)
        .get(format!("https://127.0.0.1:{}/token", server.port))
        .send()
        .await;

    assert!(
        result.is_err(),
        "a client with no certificate must not be admitted"
    );
}
