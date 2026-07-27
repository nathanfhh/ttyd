//! Forward authentication: delegating each request to an external endpoint, the way nginx's
//! `auth_request` and Traefik's ForwardAuth middleware do.
//!
//! These cover behaviour this port adds, so they are skipped when the suite runs against the
//! C reference build.

mod common;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use common::{connect_ws, http_client, is_c_reference, open_terminal, read_until, Server};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const LONG: Duration = Duration::from_secs(15);

/// One request the mock endpoint received, kept so tests can assert what was forwarded.
#[derive(Clone, Debug)]
struct Recorded {
    method: String,
    headers: HeaderMap,
}

/// How the mock endpoint should answer.
#[derive(Clone)]
struct Reply {
    status: StatusCode,
    headers: Vec<(String, String)>,
}

impl Reply {
    fn ok() -> Self {
        Reply {
            status: StatusCode::OK,
            headers: Vec::new(),
        }
    }

    fn status(status: StatusCode) -> Self {
        Reply {
            status,
            headers: Vec::new(),
        }
    }

    fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }
}

#[derive(Clone)]
struct MockState {
    reply: Reply,
    seen: Arc<Mutex<Vec<Recorded>>>,
}

/// A stand-in authentication service.
struct MockAuth {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Recorded>>>,
}

impl MockAuth {
    async fn start(reply: Reply) -> MockAuth {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let state = MockState {
            reply,
            seen: seen.clone(),
        };

        let app = Router::new().fallback(any(handle)).with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock auth");
        let addr = listener.local_addr().expect("mock auth address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        MockAuth { addr, seen }
    }

    fn url(&self) -> String {
        format!("http://{}/verify", self.addr)
    }

    fn requests(&self) -> Vec<Recorded> {
        self.seen.lock().expect("mock auth lock").clone()
    }
}

async fn handle(
    State(state): State<MockState>,
    method: axum::http::Method,
    headers: HeaderMap,
) -> Response {
    state.seen.lock().expect("mock auth lock").push(Recorded {
        method: method.to_string(),
        headers,
    });

    let mut response = state.reply.status.into_response();
    for (name, value) in &state.reply.headers {
        if let (Ok(name), Ok(value)) = (
            axum::http::HeaderName::from_bytes(name.as_bytes()),
            value.parse::<axum::http::HeaderValue>(),
        ) {
            response.headers_mut().append(name, value);
        }
    }
    response
}

#[tokio::test]
async fn a_successful_verdict_admits_the_request() {
    if is_c_reference() {
        return;
    }
    let auth = MockAuth::start(Reply::ok()).await;
    let server = Server::start(&["--auth-url", &auth.url(), "bash"]);

    let response = http_client()
        .get(server.http_url("/"))
        .send()
        .await
        .expect("request");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(auth.requests().len(), 1, "the endpoint should be consulted");
}

#[tokio::test]
async fn a_rejection_is_relayed_to_the_browser() {
    if is_c_reference() {
        return;
    }
    let auth = MockAuth::start(
        Reply::status(StatusCode::UNAUTHORIZED)
            .with_header("WWW-Authenticate", "Bearer realm=\"corp\""),
    )
    .await;
    let server = Server::start(&["--auth-url", &auth.url(), "bash"]);

    let response = http_client()
        .get(server.http_url("/"))
        .send()
        .await
        .expect("request");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.headers()["www-authenticate"],
        "Bearer realm=\"corp\"",
        "the challenge must reach the browser or the user cannot log in"
    );
}

#[tokio::test]
async fn a_login_redirect_is_passed_through() {
    if is_c_reference() {
        return;
    }
    let auth = MockAuth::start(
        Reply::status(StatusCode::FOUND).with_header("Location", "https://sso.example/login"),
    )
    .await;
    let server = Server::start(&["--auth-url", &auth.url(), "bash"]);

    let response = http_client()
        .get(server.http_url("/"))
        .send()
        .await
        .expect("request");

    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(response.headers()["location"], "https://sso.example/login");
}

#[tokio::test]
async fn an_unreachable_endpoint_fails_closed() {
    if is_c_reference() {
        return;
    }
    // Port 1 is reserved and nothing listens there, so the subrequest cannot complete.
    let server = Server::start(&["--auth-url", "http://127.0.0.1:1/verify", "bash"]);

    let response = http_client()
        .get(server.http_url("/"))
        .send()
        .await
        .expect("request");

    assert_eq!(
        response.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "an auth outage must not admit anyone"
    );
}

#[tokio::test]
async fn cookies_are_forwarded_by_default() {
    if is_c_reference() {
        return;
    }
    let auth = MockAuth::start(Reply::ok()).await;
    let server = Server::start(&["--auth-url", &auth.url(), "bash"]);

    http_client()
        .get(server.http_url("/"))
        .header("Cookie", "session=abc123")
        .send()
        .await
        .expect("request");

    let requests = auth.requests();
    let seen = requests.first().expect("one request");
    assert_eq!(seen.headers["cookie"], "session=abc123");
}

#[tokio::test]
async fn the_forwarded_header_list_is_configurable() {
    if is_c_reference() {
        return;
    }
    let auth = MockAuth::start(Reply::ok()).await;
    let server = Server::start(&[
        "--auth-url",
        &auth.url(),
        "--auth-request-header",
        "X-Api-Key",
        "bash",
    ]);

    http_client()
        .get(server.http_url("/"))
        .header("X-Api-Key", "secret-key")
        .header("Cookie", "session=abc123")
        .send()
        .await
        .expect("request");

    let requests = auth.requests();
    let seen = requests.first().expect("one request");
    assert_eq!(seen.headers["x-api-key"], "secret-key");
    assert!(
        !seen.headers.contains_key("cookie"),
        "an explicit list replaces the default rather than adding to it"
    );
}

#[tokio::test]
async fn the_original_request_is_described_to_the_endpoint() {
    if is_c_reference() {
        return;
    }
    let auth = MockAuth::start(Reply::ok()).await;
    let server = Server::start(&["--auth-url", &auth.url(), "bash"]);

    http_client()
        .get(server.http_url("/token?x=1"))
        .send()
        .await
        .expect("request");

    let requests = auth.requests();
    let seen = requests.first().expect("one request");
    assert_eq!(seen.headers["x-original-method"], "GET");
    assert_eq!(seen.headers["x-original-uri"], "/token?x=1");
    assert_eq!(seen.headers["x-forwarded-uri"], "/token?x=1");
    assert_eq!(seen.headers["x-forwarded-proto"], "http");
    assert_eq!(seen.headers["x-forwarded-for"], "127.0.0.1");
    assert!(seen.headers.contains_key("x-forwarded-host"));
}

#[tokio::test]
async fn the_auth_method_can_be_changed() {
    if is_c_reference() {
        return;
    }
    let auth = MockAuth::start(Reply::ok()).await;
    let server = Server::start(&["--auth-url", &auth.url(), "--auth-method", "POST", "bash"]);

    http_client()
        .get(server.http_url("/"))
        .send()
        .await
        .expect("request");

    assert_eq!(auth.requests().first().expect("one request").method, "POST");
}

#[tokio::test]
async fn results_are_cached_for_the_configured_window() {
    if is_c_reference() {
        return;
    }
    let auth = MockAuth::start(Reply::ok()).await;
    let server = Server::start(&["--auth-url", &auth.url(), "--auth-cache-ttl", "60", "bash"]);

    let client = http_client();
    for _ in 0..3 {
        client
            .get(server.http_url("/"))
            .header("Cookie", "session=same")
            .send()
            .await
            .expect("request");
    }

    assert_eq!(
        auth.requests().len(),
        1,
        "identical requests should only be verified once within the TTL"
    );
}

#[tokio::test]
async fn distinct_credentials_are_cached_separately() {
    if is_c_reference() {
        return;
    }
    let auth = MockAuth::start(Reply::ok()).await;
    let server = Server::start(&["--auth-url", &auth.url(), "--auth-cache-ttl", "60", "bash"]);

    let client = http_client();
    for session in ["a", "b"] {
        client
            .get(server.http_url("/"))
            .header("Cookie", format!("session={session}"))
            .send()
            .await
            .expect("request");
    }

    assert_eq!(
        auth.requests().len(),
        2,
        "one user's verdict must never be reused for another"
    );
}

#[tokio::test]
async fn a_verdict_is_not_reused_across_different_request_metadata() {
    // Regression: the cache key must cover everything the auth subrequest carries, not just
    // the headers named by --auth-request-header. ttyd also sends X-Forwarded-For,
    // X-Original-Method and friends, and an auth service is entitled to decide on them; if
    // they are missing from the key, a grant issued for one request is replayed for another
    // the endpoint would have rejected.
    if is_c_reference() {
        return;
    }
    let auth = MockAuth::start(Reply::ok()).await;
    let server = Server::start(&["--auth-url", &auth.url(), "--auth-cache-ttl", "60", "bash"]);

    let client = http_client();
    // Same path, same (absent) cookie — the two requests differ only in the Host, which ttyd
    // forwards as X-Forwarded-Host and an auth service may legitimately decide on.
    for host in ["alpha.example", "beta.example"] {
        client
            .get(server.http_url("/token"))
            .header("Host", host)
            .send()
            .await
            .expect("request");
    }

    let hosts: Vec<String> = auth
        .requests()
        .iter()
        .filter_map(|r| r.headers.get("x-forwarded-host")?.to_str().ok())
        .map(str::to_string)
        .collect();
    assert_eq!(
        hosts,
        vec!["alpha.example", "beta.example"],
        "a verdict was replayed for a request carrying different forwarded metadata"
    );
}

#[tokio::test]
async fn the_method_is_part_of_the_cache_key() {
    if is_c_reference() {
        return;
    }
    let auth = MockAuth::start(Reply::ok()).await;
    let server = Server::start(&["--auth-url", &auth.url(), "--auth-cache-ttl", "60", "bash"]);

    let client = http_client();
    client.get(server.http_url("/")).send().await.expect("get");
    client
        .head(server.http_url("/"))
        .send()
        .await
        .expect("head");

    assert_eq!(
        auth.requests().len(),
        2,
        "GET and HEAD shared a cached verdict despite being distinct to the endpoint"
    );
}

#[tokio::test]
async fn the_client_cannot_choose_its_own_forwarded_address() {
    // ttyd with --auth-url is normally the edge, so there is no trusted hop upstream: the
    // peer address it observes is the only trustworthy one.
    if is_c_reference() {
        return;
    }
    let auth = MockAuth::start(Reply::ok()).await;
    let server = Server::start(&["--auth-url", &auth.url(), "bash"]);

    http_client()
        .get(server.http_url("/"))
        .header("X-Forwarded-For", "10.0.0.1")
        .send()
        .await
        .expect("request");

    let requests = auth.requests();
    let seen = requests.first().expect("one request");
    assert_eq!(
        seen.headers["x-forwarded-for"], "127.0.0.1",
        "the client's own X-Forwarded-For must not reach the auth endpoint"
    );
}

#[tokio::test]
async fn listing_a_synthesized_header_cannot_reintroduce_the_clients_value() {
    // The guarantee above holds only while nothing puts the client's copy back. Headers go
    // out as appends, so an operator who names `x-forwarded-for` in --auth-request-header —
    // a natural thing to do, not knowing it is already synthesized — would otherwise have
    // the attacker-supplied value arrive first, ahead of the observed peer.
    if is_c_reference() {
        return;
    }
    let auth = MockAuth::start(Reply::ok()).await;
    let server = Server::start(&[
        "--auth-url",
        &auth.url(),
        "--auth-request-header",
        "X-Forwarded-For",
        "--auth-request-header",
        "X-Forwarded-Proto",
        "bash",
    ]);

    http_client()
        .get(server.http_url("/"))
        .header("X-Forwarded-For", "10.0.0.1")
        .header("X-Forwarded-Proto", "https")
        .send()
        .await
        .expect("request");

    let requests = auth.requests();
    let seen = requests.first().expect("one request");
    assert_eq!(
        seen.headers["x-forwarded-for"], "127.0.0.1",
        "the client's X-Forwarded-For must not reach the endpoint even when listed"
    );
    assert_eq!(
        seen.headers["x-forwarded-proto"], "http",
        "the observed scheme must win over the client's claim"
    );
}

#[tokio::test]
async fn rejections_are_never_cached() {
    if is_c_reference() {
        return;
    }
    let auth = MockAuth::start(Reply::status(StatusCode::FORBIDDEN)).await;
    let server = Server::start(&["--auth-url", &auth.url(), "--auth-cache-ttl", "60", "bash"]);

    let client = http_client();
    for _ in 0..2 {
        client
            .get(server.http_url("/"))
            .send()
            .await
            .expect("request");
    }

    assert_eq!(
        auth.requests().len(),
        2,
        "caching a denial would lock a user out after they log in"
    );
}

#[tokio::test]
async fn the_identity_becomes_ttyd_user() {
    if is_c_reference() {
        return;
    }
    let auth = MockAuth::start(Reply::ok().with_header("X-Auth-User", "carol")).await;
    let server = Server::start(&[
        "--auth-url",
        &auth.url(),
        "-W",
        "sh",
        "-c",
        "echo USER=$TTYD_USER; sleep 5",
    ]);

    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_terminal(&mut ws, 80, 24, None).await.expect("open");

    let seen = read_until(&mut ws, "USER=carol", LONG).await;
    assert!(seen.contains("USER=carol"), "got {seen:?}");
}

#[tokio::test]
async fn the_identity_header_name_is_configurable() {
    if is_c_reference() {
        return;
    }
    let auth = MockAuth::start(Reply::ok().with_header("X-Forwarded-User", "dave")).await;
    let server = Server::start(&[
        "--auth-url",
        &auth.url(),
        "--auth-user-header",
        "X-Forwarded-User",
        "-W",
        "sh",
        "-c",
        "echo USER=$TTYD_USER; sleep 5",
    ]);

    let mut ws = connect_ws(&server.ws_url("/ws"), &[])
        .await
        .expect("connect");
    open_terminal(&mut ws, 80, 24, None).await.expect("open");

    let seen = read_until(&mut ws, "USER=dave", LONG).await;
    assert!(seen.contains("USER=dave"), "got {seen:?}");
}

#[tokio::test]
async fn the_websocket_upgrade_is_authenticated_too() {
    if is_c_reference() {
        return;
    }
    // A terminal that could be opened by skipping the HTML page would make the whole
    // mechanism pointless, so the upgrade must be verified as well.
    let auth = MockAuth::start(Reply::status(StatusCode::FORBIDDEN)).await;
    let server = Server::start(&["--auth-url", &auth.url(), "bash"]);

    assert!(
        connect_ws(&server.ws_url("/ws"), &[]).await.is_err(),
        "the WebSocket upgrade bypassed forward authentication"
    );
    assert!(
        !auth.requests().is_empty(),
        "the upgrade never reached the auth endpoint"
    );
}

#[tokio::test]
async fn forward_auth_replaces_basic_auth() {
    if is_c_reference() {
        return;
    }
    // With forward auth configured, the static credential must no longer admit anyone, and
    // the token endpoint must not hand it out.
    let auth = MockAuth::start(Reply::ok()).await;
    let server = Server::start(&["-c", "user:pass", "--auth-url", &auth.url(), "bash"]);

    let body = http_client()
        .get(server.http_url("/token"))
        .send()
        .await
        .expect("request")
        .text()
        .await
        .expect("body");

    assert_eq!(body, r#"{"token": ""}"#);
}
