//! Request authentication.
//!
//! The C implementation hard-codes two strategies: HTTP basic auth against a fixed
//! credential, or blind trust in a header set by a reverse proxy. Both live here behind a
//! single entry point, together with a third strategy this port adds — forward
//! authentication, where every request is validated against an external endpoint the way
//! nginx's `auth_request` and Traefik's ForwardAuth middleware do.

use crate::cli::{AuthMode, ForwardAuthConfig, MAX_USER};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use subtle::ConstantTimeEq;

/// How long to wait for the forward-auth endpoint before giving up.
const FORWARD_AUTH_TIMEOUT: Duration = Duration::from_secs(10);

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
                        .max_capacity(1024)
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
            user: Some(truncate_user(user)),
        },
        None => Decision::deny(
            StatusCode::PROXY_AUTHENTICATION_REQUIRED,
            header::PROXY_AUTHENTICATE,
            "Basic realm=\"ttyd\"",
        ),
    }
}

async fn check_forward(state: &ForwardState, ctx: &RequestContext<'_>) -> Decision {
    let forwarded = collect_forwarded_headers(state, ctx);

    if let Some(cache) = &state.cache {
        if let Some(user) = cache.get(&cache_key(ctx.uri, &forwarded)) {
            return Decision::Allow { user };
        }
    }

    let method =
        reqwest::Method::from_bytes(state.config.method.as_bytes()).unwrap_or(reqwest::Method::GET);
    let mut request = state.client.request(method, &state.config.url);

    for (name, value) in &forwarded {
        request = request.header(name, value);
    }
    for (name, value) in original_request_metadata(ctx) {
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
            .map(truncate_user);

        if let Some(cache) = &state.cache {
            cache.insert(cache_key(ctx.uri, &forwarded), user.clone());
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

/// The `X-Original-*` / `X-Forwarded-*` set that nginx and Traefik both send, so existing
/// auth services work unchanged.
fn original_request_metadata(ctx: &RequestContext<'_>) -> Vec<(&'static str, String)> {
    let mut out = vec![
        ("x-original-method", ctx.method.to_string()),
        ("x-original-uri", ctx.uri.to_string()),
        ("x-forwarded-method", ctx.method.to_string()),
        ("x-forwarded-uri", ctx.uri.to_string()),
        (
            "x-forwarded-proto",
            if ctx.tls {
                "https".into()
            } else {
                "http".to_string()
            },
        ),
    ];
    if let Some(host) = ctx.headers.get(header::HOST).and_then(|v| v.to_str().ok()) {
        out.push(("x-forwarded-host", host.to_string()));
    }
    if let Some(peer) = ctx.peer {
        let chain = match ctx
            .headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
        {
            Some(existing) => format!("{existing}, {peer}"),
            None => peer.to_string(),
        };
        out.push(("x-forwarded-for", chain));
    }
    out
}

fn cache_key(uri: &str, forwarded: &[(String, String)]) -> String {
    let mut key = String::with_capacity(uri.len() + 64);
    key.push_str(uri);
    for (name, value) in forwarded {
        key.push('\u{1}');
        key.push_str(name);
        key.push('\u{2}');
        key.push_str(value);
    }
    key
}

/// The child process environment reserves 29 bytes for the user name, matching the C
/// `pss_tty.user` buffer.
fn truncate_user(user: &str) -> String {
    if user.len() <= MAX_USER {
        return user.to_string();
    }
    let mut end = MAX_USER;
    while end > 0 && !user.is_char_boundary(end) {
        end -= 1;
    }
    user[..end].to_string()
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
    fn user_names_are_truncated_to_the_env_buffer() {
        let long = "u".repeat(100);
        assert_eq!(truncate_user(&long).len(), MAX_USER);
    }

    #[test]
    fn cache_keys_separate_distinct_credentials() {
        let a = cache_key("/", &[("cookie".into(), "session=a".into())]);
        let b = cache_key("/", &[("cookie".into(), "session=b".into())]);
        assert_ne!(a, b);
    }
}
