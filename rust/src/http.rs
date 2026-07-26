//! HTTP routing and handlers.
//!
//! The C implementation authenticates before it looks at the path, so an unauthenticated
//! request to an unknown path is challenged rather than 404'd. That ordering is preserved
//! here by running authentication as a layer wrapping the whole router, fallback included.

use crate::auth::{Decision, RequestContext};
use crate::cli::VERSION;
use crate::html;
use crate::state::{AppState, ConnInfo};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::Router;
use std::sync::Arc;

pub fn router(state: Arc<AppState>) -> Router {
    let endpoints = state.cfg.endpoints.clone();

    let mut app = Router::new()
        .route(&endpoints.index, get(index))
        .route(&endpoints.token, get(token))
        .route(&endpoints.ws, any(crate::ws::handler));

    // `--base-path /x` also answers on `/x` itself, redirecting to `/x/`.
    if !endpoints.parent.is_empty() {
        app = app.route(&endpoints.parent, get(redirect_to_index));
    }

    app.fallback(not_found)
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .layer(middleware::from_fn(add_server_header))
        .with_state(state)
}

/// Rejects the request unless the configured authentication strategy admits it.
async fn authenticate(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let conn = request
        .extensions()
        .get::<ConnInfo>()
        .copied()
        .unwrap_or_default();

    let uri = request
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| request.uri().path().to_string());

    let decision = {
        let ctx = RequestContext {
            method: request.method(),
            uri: &uri,
            headers: request.headers(),
            peer: conn.peer.map(|p| p.ip()),
            tls: conn.tls,
        };
        state.auth.authenticate(&ctx).await
    };

    match decision {
        Decision::Allow { user } => {
            let mut request = request;
            // The WebSocket handler turns this into TTYD_USER for the child process.
            request.extensions_mut().insert(AuthenticatedUser(user));
            next.run(request).await
        }
        Decision::Deny { status, headers } => {
            tracing::debug!("auth rejected {} {} -> {}", request.method(), uri, status);
            let mut response = Response::builder()
                .status(status)
                .body(Body::empty())
                .expect("static response is valid");
            for (name, value) in headers.iter() {
                response.headers_mut().append(name.clone(), value.clone());
            }
            response
                .headers_mut()
                .insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
            response
        }
    }
}

/// The identity established during authentication, if any.
#[derive(Clone, Debug)]
pub struct AuthenticatedUser(pub Option<String>);

async fn add_server_header(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    if let Ok(value) = HeaderValue::from_str(&format!("ttyd/{VERSION} (rust)")) {
        response.headers_mut().insert(header::SERVER, value);
    }
    response
}

async fn index(State(state): State<Arc<AppState>>, request: Request) -> Response {
    log_access(&request);

    if let Some(path) = &state.cfg.index {
        return match tokio::fs::read(path).await {
            Ok(body) => html_response(body, false),
            Err(e) => {
                // The C build answers 404 here — `lws_serve_http_file` cannot distinguish a
                // file that was never there from one that has just been replaced by a deploy.
                // The reason is logged so an operator can tell a permission problem from a
                // missing file, which the status code alone does not say.
                tracing::error!("cannot read custom index {}: {e}", path.display());
                not_found(request).await
            }
        };
    }

    // The embedded bundle is stored compressed, so a client that accepts gzip gets it
    // without a decompression round trip.
    if accepts_gzip(&request) {
        html_response(html::INDEX_HTML_GZIP.to_vec(), true)
    } else {
        html_response(html::index_html_plain().to_vec(), false)
    }
}

async fn token(State(state): State<Arc<AppState>>, request: Request) -> Response {
    log_access(&request);

    let credential = state.cfg.credential().unwrap_or("");
    let body = format!("{{\"token\": \"{credential}\"}}");
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json;charset=utf-8"),
        )],
        body,
    )
        .into_response()
}

async fn redirect_to_index(State(state): State<Arc<AppState>>, request: Request) -> Response {
    log_access(&request);

    let location = HeaderValue::from_str(&state.cfg.endpoints.index)
        .unwrap_or_else(|_| HeaderValue::from_static("/"));
    (StatusCode::FOUND, [(header::LOCATION, location)]).into_response()
}

/// Byte-for-byte the body libwebsockets emits from `lws_return_http_status`, so error pages
/// look the same to anything that scrapes them.
const NOT_FOUND_BODY: &str = "<html><head><meta charset=utf-8 http-equiv=\"Content-Language\" content=\"en\"/><link rel=\"stylesheet\" type=\"text/css\" href=\"/error.css\"/></head><body><h1>404</h1></body></html>";

async fn not_found(request: Request) -> Response {
    log_access(&request);
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, HeaderValue::from_static("text/html"))],
        NOT_FOUND_BODY,
    )
        .into_response()
}

fn html_response(body: Vec<u8>, gzip: bool) -> Response {
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html")
        .body(Body::from(body))
        .expect("response builder inputs are valid");
    if gzip {
        response
            .headers_mut()
            .insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    }
    response
}

/// Whether the client will accept a gzip-encoded body.
///
/// The C build asks `strstr(buf, "gzip") != NULL`, which answers yes to
/// `Accept-Encoding: gzip;q=0` — a client saying, in the way RFC 9110 defines, that it does
/// *not* want gzip. It then receives a compressed body it has told the server it cannot
/// decode. This parses the header into tokens instead and honours a `q=0` refusal.
///
/// The divergence only ever goes one way, towards sending less compression: `*` is still not
/// treated as accepting gzip, because an uncompressed body is acceptable to every client and
/// there is nothing to gain from differing there. `x-gzip` stays accepted, as RFC 9110 lists
/// it as an alias and the C substring match happens to allow it.
fn accepts_gzip(request: &Request) -> bool {
    let Some(header) = request
        .headers()
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };

    header.split(',').any(|entry| {
        let mut parts = entry.split(';');
        let coding = parts.next().unwrap_or("").trim();
        if !coding.eq_ignore_ascii_case("gzip") && !coding.eq_ignore_ascii_case("x-gzip") {
            return false;
        }
        // Any weight of zero is a refusal; anything else, including a malformed or absent
        // parameter, leaves the coding acceptable.
        !parts.any(|param| {
            let mut kv = param.splitn(2, '=');
            let key = kv.next().unwrap_or("").trim();
            let value = kv.next().unwrap_or("").trim();
            key.eq_ignore_ascii_case("q") && value.parse::<f32>() == Ok(0.0)
        })
    })
}

fn log_access(request: &Request) {
    let conn = request
        .extensions()
        .get::<ConnInfo>()
        .copied()
        .unwrap_or_default();
    tracing::info!("HTTP {} - {}", request.uri().path(), conn.peer_display());
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    fn request(accept_encoding: Option<&str>) -> Request {
        let mut builder = axum::http::Request::builder().uri("/");
        if let Some(value) = accept_encoding {
            builder = builder.header(header::ACCEPT_ENCODING, value);
        }
        builder.body(Body::empty()).expect("valid request")
    }

    #[test]
    fn gzip_is_accepted_when_offered() {
        for value in [
            "gzip",
            "deflate, gzip",
            "gzip, br",
            " gzip ",
            "gzip;q=1",
            "gzip;q=0.5",
        ] {
            assert!(accepts_gzip(&request(Some(value))), "{value:?}");
        }
    }

    #[test]
    fn a_zero_weight_is_a_refusal() {
        // The C build answers yes to all of these because it only looks for the substring,
        // and then sends a body the client has said it cannot decode.
        for value in [
            "gzip;q=0",
            "gzip;q=0.0",
            "gzip;q=0.000",
            "gzip; q=0",
            "deflate, gzip;q=0",
        ] {
            assert!(!accepts_gzip(&request(Some(value))), "{value:?}");
        }
    }

    #[test]
    fn another_codings_refusal_does_not_refuse_gzip() {
        assert!(accepts_gzip(&request(Some("deflate;q=0, gzip"))));
        assert!(accepts_gzip(&request(Some("br;q=0, gzip;q=1.0"))));
    }

    #[test]
    fn coding_names_are_case_insensitive() {
        // RFC 9110: content codings are case-insensitive. The C build's `strstr` is not, so
        // it sends `GZIP` clients an uncompressed body — harmless, but not what they asked.
        assert!(accepts_gzip(&request(Some("GZIP"))));
        assert!(accepts_gzip(&request(Some("GZip;Q=1"))));
        assert!(!accepts_gzip(&request(Some("GZIP;Q=0"))));
    }

    #[test]
    fn anything_that_does_not_name_gzip_is_not_gzip() {
        for value in ["identity", "br, zstd", "deflate", "*", ""] {
            assert!(!accepts_gzip(&request(Some(value))), "{value:?}");
        }
        assert!(!accepts_gzip(&request(None)));
    }

    #[test]
    fn the_legacy_alias_is_still_accepted() {
        // Matching the C build, which accepts it by accident of substring matching.
        assert!(accepts_gzip(&request(Some("x-gzip"))));
        assert!(!accepts_gzip(&request(Some("x-gzip;q=0"))));
    }
}
