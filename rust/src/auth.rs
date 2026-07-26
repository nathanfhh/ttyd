//! Request authentication.
//!
//! The C implementation hard-codes two strategies: HTTP basic auth against a fixed
//! credential, or blind trust in a header set by a reverse proxy. Both live here behind a
//! single entry point, together with a third strategy this port adds — forward
//! authentication, where every request is validated against an external endpoint the way
//! nginx's `auth_request` and Traefik's ForwardAuth middleware do.

use crate::cli::{AuthMode, ForwardAuthConfig};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use subtle::ConstantTimeEq;

/// How long to wait for the forward-auth endpoint before giving up.
const FORWARD_AUTH_TIMEOUT: Duration = Duration::from_secs(10);

/// Upper bound on cached auth decisions; the key space is bounded by the number of
/// simultaneously active callers, not by traffic volume.
const AUTH_CACHE_CAPACITY: u64 = 1024;

/// Headers copied from the auth endpoint's rejection back to the browser. These carry the
/// information a user needs to actually authenticate: a challenge, a login redirect, or a
/// session cookie issued during the attempt.
const PASSTHROUGH_HEADERS: &[HeaderName] = &[
    header::WWW_AUTHENTICATE,
    header::PROXY_AUTHENTICATE,
    header::LOCATION,
    header::SET_COOKIE,
    header::CACHE_CONTROL,
];

/// The outcome of authenticating one request.
#[derive(Debug, Clone)]
pub enum Decision {
    /// The request may proceed. `user` becomes `TTYD_USER` in the child process.
    Allow { user: Option<String> },
    /// The request is refused; the response carries these status and headers.
    Deny {
        status: StatusCode,
        headers: HeaderMap,
    },
}

impl Decision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Decision::Allow { .. })
    }

    pub fn user(&self) -> Option<&str> {
        match self {
            Decision::Allow { user } => user.as_deref(),
            Decision::Deny { .. } => None,
        }
    }

    fn deny(status: StatusCode, header: HeaderName, value: &'static str) -> Self {
        let mut headers = HeaderMap::new();
        headers.insert(header, HeaderValue::from_static(value));
        Decision::Deny { status, headers }
    }
}

/// What the caller knows about the incoming request. Kept deliberately small so both the
/// plain HTTP handler and the WebSocket upgrade path can build one cheaply.
pub struct RequestContext<'a> {
    pub method: &'a Method,
    /// Path plus query string, as it appeared in the request line.
    pub uri: &'a str,
    pub headers: &'a HeaderMap,
    pub peer: Option<IpAddr>,
    pub tls: bool,
}

pub struct Authenticator {
    mode: AuthMode,
    forward: Option<ForwardState>,
}

struct ForwardState {
    config: ForwardAuthConfig,
    client: reqwest::Client,
    cache: Option<moka::sync::Cache<String, Option<String>>>,
}

impl Authenticator {
    pub fn new(mode: AuthMode) -> anyhow::Result<Arc<Self>> {
        let forward = match &mode {
            AuthMode::Forward(config) => {
                let client = reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(FORWARD_AUTH_TIMEOUT)
                    .build()?;
                let cache = (config.cache_ttl > 0).then(|| {
                    moka::sync::Cache::builder()
                        .max_capacity(AUTH_CACHE_CAPACITY)
                        .time_to_live(Duration::from_secs(config.cache_ttl))
                        .build()
                });
                Some(ForwardState {
                    config: config.clone(),
                    client,
                    cache,
                })
            }
            _ => None,
        };
        Ok(Arc::new(Self { mode, forward }))
    }

    /// Whether any authentication is configured at all.
    pub fn is_enabled(&self) -> bool {
        !matches!(self.mode, AuthMode::None)
    }

    pub async fn authenticate(&self, ctx: &RequestContext<'_>) -> Decision {
        match &self.mode {
            AuthMode::None => Decision::Allow { user: None },
            AuthMode::Basic { credential } => check_basic(ctx.headers, credential),
            AuthMode::TrustedHeader { header } => check_trusted_header(ctx.headers, header),
            AuthMode::Forward(_) => {
                let state = self
                    .forward
                    .as_ref()
                    .expect("forward auth state is built alongside the mode");
                check_forward(state, ctx).await
            }
        }
    }
}

/// Validates `Authorization: Basic <base64>` against the configured credential.
///
/// Unlike the C version this compares in constant time and treats the scheme name
/// case-insensitively, as RFC 7617 requires.
fn check_basic(headers: &HeaderMap, credential: &str) -> Decision {
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let (scheme, rest) = v.split_once(' ')?;
            scheme.eq_ignore_ascii_case("basic").then_some(rest)
        });

    if let Some(presented) = presented {
        if presented.as_bytes().ct_eq(credential.as_bytes()).into() {
            return Decision::Allow { user: None };
        }
    }
    Decision::deny(
        StatusCode::UNAUTHORIZED,
        header::WWW_AUTHENTICATE,
        "Basic realm=\"ttyd\"",
    )
}

/// Accepts the request when the trusted proxy header is present and non-empty, using its
/// value as the user identity.
fn check_trusted_header(headers: &HeaderMap, name: &str) -> Decision {
    let value = headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty());

    match value {
        Some(user) => Decision::Allow {
            user: Some(keep_user(user)),
        },
        None => Decision::deny(
            StatusCode::PROXY_AUTHENTICATION_REQUIRED,
            header::PROXY_AUTHENTICATE,
            "Basic realm=\"ttyd\"",
        ),
    }
}

async fn check_forward(state: &ForwardState, ctx: &RequestContext<'_>) -> Decision {
    // The endpoint is entitled to decide on anything the subrequest carries, so the cache
    // key is derived from that exact set. Keying on less would replay one caller's verdict
    // for a request the endpoint would have refused.
    let subrequest_headers = build_subrequest_headers(state, ctx);
    let key = cache_key(ctx.method.as_str(), ctx.uri, &subrequest_headers);

    if let Some(cache) = &state.cache {
        if let Some(user) = cache.get(&key) {
            return Decision::Allow { user };
        }
    }

    let method =
        reqwest::Method::from_bytes(state.config.method.as_bytes()).unwrap_or(reqwest::Method::GET);
    let mut request = state.client.request(method, &state.config.url);

    for (name, value) in &subrequest_headers {
        request = request.header(name, value);
    }

    let response = match request.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("forward auth request to {} failed: {e}", state.config.url);
            return Decision::Deny {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                headers: HeaderMap::new(),
            };
        }
    };

    let status = StatusCode::from_u16(response.status().as_u16())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    if status.is_success() {
        let user = response
            .headers()
            .get(&state.config.user_header)
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.is_empty())
            .map(keep_user);

        if let Some(cache) = &state.cache {
            cache.insert(key, user.clone());
        }
        return Decision::Allow { user };
    }

    // Anything other than 2xx is relayed to the browser together with the headers that let
    // the user act on it — a challenge, a login redirect, or a cookie set during the attempt.
    let mut headers = HeaderMap::new();
    for name in PASSTHROUGH_HEADERS {
        for value in response.headers().get_all(name) {
            if let Ok(value) = HeaderValue::from_bytes(value.as_bytes()) {
                headers.append(name.clone(), value);
            }
        }
    }
    Decision::Deny { status, headers }
}

/// Copies the configured subset of request headers that the auth endpoint needs to identify
/// the caller.
fn collect_forwarded_headers(
    state: &ForwardState,
    ctx: &RequestContext<'_>,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for name in state.config.effective_request_headers() {
        for value in ctx.headers.get_all(&name) {
            if let Ok(value) = value.to_str() {
                out.push((name.clone(), value.to_string()));
            }
        }
    }
    out
}

/// Everything the auth subrequest carries: the operator's chosen request headers plus the
/// description of the original request. Built once and used for both the outgoing headers
/// and the cache key, so the two can never drift apart.
fn build_subrequest_headers(
    state: &ForwardState,
    ctx: &RequestContext<'_>,
) -> Vec<(String, String)> {
    let mut out = collect_forwarded_headers(state, ctx);
    out.extend(original_request_metadata(ctx));
    out
}

/// The `X-Original-*` / `X-Forwarded-*` set that nginx and Traefik both send, so existing
/// auth services work unchanged.
fn original_request_metadata(ctx: &RequestContext<'_>) -> Vec<(String, String)> {
    let mut out = vec![
        ("x-original-method".to_string(), ctx.method.to_string()),
        ("x-original-uri".to_string(), ctx.uri.to_string()),
        ("x-forwarded-method".to_string(), ctx.method.to_string()),
        ("x-forwarded-uri".to_string(), ctx.uri.to_string()),
        (
            "x-forwarded-proto".to_string(),
            if ctx.tls { "https" } else { "http" }.to_string(),
        ),
    ];
    if let Some(host) = ctx.headers.get(header::HOST).and_then(|v| v.to_str().ok()) {
        out.push(("x-forwarded-host".to_string(), host.to_string()));
    }
    if let Some(peer) = ctx.peer {
        // Only the address we observed ourselves. ttyd running forward auth is normally the
        // edge, so there is no trusted hop upstream and a client-supplied X-Forwarded-For is
        // just an attacker-chosen string — appending to it would let the caller prepend any
        // address it likes for an endpoint that reads the chain from the left.
        out.push(("x-forwarded-for".to_string(), peer.to_string()));
    }
    out
}

fn cache_key(method: &str, uri: &str, subrequest_headers: &[(String, String)]) -> String {
    let mut key = String::with_capacity(uri.len() + 128);
    key.push_str(method);
    key.push('\u{3}');
    key.push_str(uri);
    for (name, value) in subrequest_headers {
        key.push('\u{1}');
        key.push_str(name);
        key.push('\u{2}');
        key.push_str(value);
    }
    key
}

/// The identity is passed through whole.
///
/// The C build copies it into a `char[30]`, and `lws_hdr_custom_copy` refuses outright when
/// the value does not fit — so an account name of 30 bytes or more cannot open a terminal
/// there at all. Truncating instead would be worse than refusing: two distinct accounts
/// sharing a 29-byte prefix would collapse onto the same `TTYD_USER`. With no fixed buffer
/// here, neither compromise is necessary.
fn keep_user(user: &str) -> String {
    user.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[tokio::test]
    async fn no_auth_allows_everything() {
        let auth = Authenticator::new(AuthMode::None).unwrap();
        let h = headers(&[]);
        let ctx = RequestContext {
            method: &Method::GET,
            uri: "/",
            headers: &h,
            peer: None,
            tls: false,
        };
        assert!(auth.authenticate(&ctx).await.is_allowed());
        assert!(!auth.is_enabled());
    }

    #[test]
    fn basic_auth_accepts_the_matching_credential() {
        // base64("user:pass")
        let credential = "dXNlcjpwYXNz";
        let h = headers(&[("authorization", "Basic dXNlcjpwYXNz")]);
        assert!(check_basic(&h, credential).is_allowed());
    }

    #[test]
    fn basic_auth_scheme_is_case_insensitive() {
        let h = headers(&[("authorization", "basic dXNlcjpwYXNz")]);
        assert!(check_basic(&h, "dXNlcjpwYXNz").is_allowed());
    }

    #[test]
    fn basic_auth_rejects_a_wrong_credential_with_a_challenge() {
        let h = headers(&[("authorization", "Basic d3Jvbmc=")]);
        match check_basic(&h, "dXNlcjpwYXNz") {
            Decision::Deny { status, headers } => {
                assert_eq!(status, StatusCode::UNAUTHORIZED);
                assert_eq!(
                    headers.get(header::WWW_AUTHENTICATE).unwrap(),
                    "Basic realm=\"ttyd\""
                );
            }
            Decision::Allow { .. } => panic!("must not allow"),
        }
    }

    #[test]
    fn basic_auth_rejects_a_missing_header() {
        assert!(!check_basic(&headers(&[]), "dXNlcjpwYXNz").is_allowed());
    }

    #[test]
    fn trusted_header_supplies_the_user() {
        let h = headers(&[("x-remote-user", "alice")]);
        let decision = check_trusted_header(&h, "x-remote-user");
        assert_eq!(decision.user(), Some("alice"));
    }

    #[test]
    fn trusted_header_rejects_when_absent_or_empty() {
        match check_trusted_header(&headers(&[]), "x-remote-user") {
            Decision::Deny { status, .. } => {
                assert_eq!(status, StatusCode::PROXY_AUTHENTICATION_REQUIRED)
            }
            Decision::Allow { .. } => panic!("must not allow"),
        }
        let empty = headers(&[("x-remote-user", "")]);
        assert!(!check_trusted_header(&empty, "x-remote-user").is_allowed());
    }

    #[test]
    fn long_user_names_survive_intact() {
        // Truncating would alias two accounts sharing a prefix onto one TTYD_USER.
        let long = "u".repeat(100);
        let h = headers(&[("x-remote-user", &long)]);
        assert_eq!(
            check_trusted_header(&h, "x-remote-user").user(),
            Some(long.as_str())
        );
    }

    #[test]
    fn cache_keys_separate_distinct_credentials() {
        let a = cache_key("GET", "/", &[("cookie".into(), "session=a".into())]);
        let b = cache_key("GET", "/", &[("cookie".into(), "session=b".into())]);
        assert_ne!(a, b);
    }

    #[test]
    fn cache_keys_separate_distinct_methods_and_forwarded_metadata() {
        // Everything the subrequest carries must move the key, or a verdict granted for one
        // request gets replayed for another the endpoint would have refused.
        let headers = [("x-forwarded-for".to_string(), "10.0.0.1".to_string())];
        assert_ne!(
            cache_key("GET", "/", &headers),
            cache_key("HEAD", "/", &headers)
        );
        assert_ne!(
            cache_key("GET", "/", &headers),
            cache_key(
                "GET",
                "/",
                &[("x-forwarded-for".to_string(), "10.0.0.2".to_string())]
            )
        );
        assert_ne!(cache_key("GET", "/", &headers), cache_key("GET", "/", &[]));
    }

    #[test]
    fn the_forwarded_address_is_the_observed_peer_only() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", HeaderValue::from_static("10.0.0.1"));
        let ctx = RequestContext {
            method: &Method::GET,
            uri: "/",
            headers: &h,
            peer: Some("203.0.113.9".parse().unwrap()),
            tls: false,
        };
        let sent = original_request_metadata(&ctx);
        let xff = sent
            .iter()
            .find(|(n, _)| n == "x-forwarded-for")
            .map(|(_, v)| v.as_str());
        assert_eq!(xff, Some("203.0.113.9"));
    }
}
