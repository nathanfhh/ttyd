//! Socket binding, TLS termination and the connection accept loop.

use crate::cli::Config;
use crate::state::{AppState, ConnInfo};
use anyhow::{bail, Context, Result};
use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use axum::Router;
use hyper_util::rt::{TokioExecutor, TokioIo};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream, UnixListener};
use tower::ServiceExt;

/// How long to wait for the first byte when deciding whether a connection on the TLS port
/// is a handshake or plain HTTP.
const TLS_SNIFF_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait after a failed `accept` before trying again.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(50);

/// The listening socket, which is either a TCP port or a UNIX domain socket.
pub enum Listener {
    Tcp(TcpListener),
    Unix {
        listener: UnixListener,
        path: PathBuf,
    },
}

impl Listener {
    /// The port actually bound, which matters when `--port 0` asked for a random one.
    pub fn port(&self) -> u16 {
        match self {
            Listener::Tcp(l) => l.local_addr().map(|a| a.port()).unwrap_or(0),
            Listener::Unix { .. } => 0,
        }
    }
}

pub async fn bind(cfg: &Config) -> Result<Listener> {
    if cfg.is_unix_socket() {
        let path = PathBuf::from(cfg.iface.as_deref().expect("checked by is_unix_socket"));
        // A leftover socket file from an unclean shutdown would make bind fail.
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("cannot remove stale socket {}", path.display()))?;
        }
        // Created under a umask that already denies everyone else, because `chmod`ing after
        // the fact leaves a window — however short — in which a local user can connect to a
        // world-accessible socket and be handed a terminal.
        let listener = {
            let previous = unsafe { libc::umask(0o117) };
            let result = UnixListener::bind(&path);
            unsafe { libc::umask(previous) };
            result.with_context(|| format!("cannot bind unix socket {}", path.display()))?
        };
        apply_socket_permissions(&path, cfg)?;
        return Ok(Listener::Unix { listener, path });
    }

    let address = resolve_bind_address(cfg)?;
    let listener = TcpListener::bind(SocketAddr::new(address, cfg.port))
        .await
        .with_context(|| format!("cannot bind {address}:{}", cfg.port))?;
    Ok(Listener::Tcp(listener))
}

fn resolve_bind_address(cfg: &Config) -> Result<IpAddr> {
    let Some(iface) = cfg.iface.as_deref() else {
        return Ok(if cfg.ipv6 {
            IpAddr::V6(Ipv6Addr::UNSPECIFIED)
        } else {
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        });
    };

    if let Ok(ip) = iface.parse::<IpAddr>() {
        return Ok(ip);
    }

    // Otherwise treat it as a network interface name and take its first matching address.
    let addresses = nix::ifaddrs::getifaddrs().context("getifaddrs failed")?;
    for entry in addresses {
        if entry.interface_name != iface {
            continue;
        }
        let Some(storage) = entry.address else {
            continue;
        };
        // `-6` asks for IPv6; falling through to the v4 branch when this particular entry
        // happens to be v4 would silently ignore it, and an interface's v4 address is
        // usually enumerated first.
        if cfg.ipv6 {
            if let Some(v6) = storage.as_sockaddr_in6() {
                return Ok(IpAddr::V6(v6.ip()));
            }
            continue;
        }
        if let Some(v4) = storage.as_sockaddr_in() {
            return Ok(IpAddr::V4(v4.ip()));
        }
    }
    if cfg.ipv6 {
        bail!("no IPv6 address found for interface {iface}")
    }
    bail!("no address found for interface {iface}")
}

/// Reproduces what libwebsockets does to a UNIX domain socket right after binding it:
/// `chown` to the context's uid/gid — i.e. whatever `-u`/`-g` asked for — then to the
/// `--socket-owner` pair if one was given, then an unconditional `chmod 0660`.
///
/// The mode is the part that matters, and it is the part this port originally missed.
/// Without it the socket keeps the process umask, which on a default umask means `0755`:
/// world-connectable, so any local user on the box gets a terminal. Verified against the C
/// build by `strace`, which shows `chown` followed by `chmod(path, 0660)` on every start.
/// The caller also binds under a restrictive umask, so the mode here confirms the result
/// rather than being the only thing standing between the socket and the rest of the box.
fn apply_socket_permissions(path: &std::path::Path, cfg: &Config) -> Result<()> {
    let uid = cfg.uid.map(nix::unistd::Uid::from_raw);
    let gid = cfg.gid.map(nix::unistd::Gid::from_raw);
    if uid.is_some() || gid.is_some() {
        nix::unistd::chown(path, uid, gid)
            .with_context(|| format!("cannot chown {}", path.display()))?;
    }

    if let Some(owner) = &cfg.socket_owner {
        apply_socket_owner(path, owner)?;
    }

    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))
        .with_context(|| format!("cannot chmod {}", path.display()))?;
    Ok(())
}

fn apply_socket_owner(path: &std::path::Path, owner: &str) -> Result<()> {
    let (user, group) = match owner.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (owner, None),
    };

    let uid = nix::unistd::User::from_name(user)
        .ok()
        .flatten()
        .map(|u| u.uid)
        .with_context(|| format!("unknown socket owner user: {user}"))?;

    let gid = match group.filter(|g| !g.is_empty()) {
        Some(g) => Some(
            nix::unistd::Group::from_name(g)
                .ok()
                .flatten()
                .map(|g| g.gid)
                .with_context(|| format!("unknown socket owner group: {g}"))?,
        ),
        None => None,
    };

    nix::unistd::chown(path, Some(uid), gid)
        .with_context(|| format!("cannot chown {}", path.display()))?;
    Ok(())
}

/// Builds the rustls configuration, optionally requiring a client certificate signed by the
/// CA in `--ssl-ca`.
pub fn tls_config(cfg: &Config) -> Result<Arc<rustls::ServerConfig>> {
    let cert_path = cfg.ssl_cert.as_ref().context("--ssl requires --ssl-cert")?;
    let key_path = cfg.ssl_key.as_ref().context("--ssl requires --ssl-key")?;

    let certs = rustls_pemfile::certs(&mut std::io::BufReader::new(
        std::fs::File::open(cert_path)
            .with_context(|| format!("cannot open {}", cert_path.display()))?,
    ))
    .collect::<Result<Vec<_>, _>>()
    .with_context(|| format!("cannot parse certificates from {}", cert_path.display()))?;

    let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(
        std::fs::File::open(key_path)
            .with_context(|| format!("cannot open {}", key_path.display()))?,
    ))
    .with_context(|| format!("cannot parse private key from {}", key_path.display()))?
    .context("no private key found")?;

    let builder = rustls::ServerConfig::builder();
    let config = match &cfg.ssl_ca {
        Some(ca_path) => {
            let mut roots = rustls::RootCertStore::empty();
            for cert in rustls_pemfile::certs(&mut std::io::BufReader::new(
                std::fs::File::open(ca_path)
                    .with_context(|| format!("cannot open {}", ca_path.display()))?,
            )) {
                roots.add(cert?).context("invalid CA certificate")?;
            }
            let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
                .build()
                .context("cannot build client certificate verifier")?;
            builder
                .with_client_cert_verifier(verifier)
                .with_single_cert(certs, key)?
        }
        None => builder.with_no_client_auth().with_single_cert(certs, key)?,
    };

    Ok(Arc::new(config))
}

pub async fn run(
    state: Arc<AppState>,
    router: Router,
    listener: Listener,
    tls: Option<Arc<rustls::ServerConfig>>,
) {
    let acceptor = tls.map(tokio_rustls::TlsAcceptor::from);

    match listener {
        Listener::Tcp(listener) => loop {
            tokio::select! {
                _ = state.wait_for_shutdown() => break,
                accepted = listener.accept() => {
                    let (stream, peer) = match accepted {
                        Ok(accepted) => accepted,
                        Err(e) => {
                            accept_backoff(e).await;
                            continue;
                        }
                    };
                    let router = router.clone();
                    let acceptor = acceptor.clone();
                    tokio::spawn(async move {
                        serve_tcp(stream, peer, router, acceptor).await;
                    });
                }
            }
        },
        Listener::Unix { listener, path } => {
            loop {
                tokio::select! {
                    _ = state.wait_for_shutdown() => break,
                    accepted = listener.accept() => {
                        let stream = match accepted {
                            Ok((stream, _)) => stream,
                            Err(e) => {
                                accept_backoff(e).await;
                                continue;
                            }
                        };
                        let router = router.clone();
                        tokio::spawn(async move {
                            let conn = ConnInfo { peer: None, tls: false };
                            serve_connection(TokioIo::new(stream), router, conn).await;
                        });
                    }
                }
            }
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Reports a failed `accept` and pauses briefly.
///
/// Descriptor exhaustion (`EMFILE`/`ENFILE`) is not transient: `accept` fails immediately
/// and keeps failing, so retrying without a pause spins a core at 100 % and starves the
/// runtime that would otherwise be closing the connections which free those descriptors.
async fn accept_backoff(error: std::io::Error) {
    tracing::warn!("accept failed: {error}");
    tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
}

async fn serve_tcp(
    stream: TcpStream,
    peer: SocketAddr,
    router: Router,
    acceptor: Option<tokio_rustls::TlsAcceptor>,
) {
    let Some(acceptor) = acceptor else {
        let conn = ConnInfo {
            peer: Some(peer),
            tls: false,
        };
        serve_connection(TokioIo::new(stream), router, conn).await;
        return;
    };

    // The C build enables both `ALLOW_NON_SSL_ON_SSL_PORT` and `REDIRECT_HTTP_TO_HTTPS`, so a
    // plain HTTP request on the TLS port is answered with a redirect instead of a handshake
    // failure. A TLS ClientHello always starts with the handshake record type, 0x16.
    // Bounded, because this runs before hyper takes over and therefore before any of its
    // timeouts apply — a client that connects and then says nothing would otherwise hold
    // the task and its descriptor open indefinitely.
    let mut first = [0u8; 1];
    let peeked = tokio::time::timeout(TLS_SNIFF_TIMEOUT, stream.peek(&mut first)).await;
    let is_tls = match peeked {
        Ok(Ok(1)) => first[0] == 0x16,
        _ => return,
    };

    if !is_tls {
        let conn = ConnInfo {
            peer: Some(peer),
            tls: false,
        };
        serve_connection(TokioIo::new(stream), redirect_router(), conn).await;
        return;
    }

    let Ok(tls_stream) = acceptor.accept(stream).await else {
        return;
    };
    let conn = ConnInfo {
        peer: Some(peer),
        tls: true,
    };
    serve_connection(TokioIo::new(tls_stream), router, conn).await;
}

async fn serve_connection<I>(io: TokioIo<I>, router: Router, conn: ConnInfo)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let service = hyper::service::service_fn(move |request: Request<hyper::body::Incoming>| {
        let mut request = request.map(Body::new);
        request.extensions_mut().insert(conn);
        let router = router.clone();
        async move { router.oneshot(request).await }
    });

    let builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
    // `with_upgrades` is what lets the WebSocket handshake take the connection over.
    if let Err(e) = builder.serve_connection_with_upgrades(io, service).await {
        tracing::debug!("connection closed: {e}");
    }
}

/// Serves nothing but permanent redirects to the HTTPS equivalent of the requested URL.
fn redirect_router() -> Router {
    Router::new().fallback(|request: Request<Body>| async move {
        // Without an authority there is no such thing as "the HTTPS equivalent" of the
        // request, and guessing one would point the client somewhere it never asked for.
        // Emitting `https:///path` — which is what building the URL unconditionally did —
        // is worse still: a syntactically invalid Location that no client can follow.
        let host = request
            .headers()
            .get(axum::http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .filter(|h| !h.is_empty())
            // HTTP/2 carries the authority in the URI rather than in a Host header, as does
            // an HTTP/1.1 request made in absolute form.
            .or_else(|| request.uri().authority().map(|a| a.as_str()));

        let Some(host) = host else {
            // The C build drops the connection here without answering at all. A 400 says the
            // same thing in a way a client can report.
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::empty())
                .expect("static response is valid");
        };

        let path = request
            .uri()
            .path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or("/");

        Response::builder()
            .status(StatusCode::MOVED_PERMANENTLY)
            .header(
                axum::http::header::LOCATION,
                format!("https://{host}{path}"),
            )
            .body(Body::empty())
            .expect("static redirect response is valid")
    })
}

/// Drops privileges after the socket is bound, matching the order the C version relies on.
///
/// Supplementary groups are dealt with first and separately: `setgid` does not touch that
/// list, so without this the terminal would keep every group the server was started with —
/// `docker`, `adm`, `disk` and friends when launched from a root session or via systemd's
/// `SupplementaryGroups=`. libwebsockets calls `initgroups` for the same reason.
pub fn drop_privileges(cfg: &Config) -> Result<()> {
    if cfg.uid.is_none() && cfg.gid.is_none() {
        return Ok(());
    }

    let target_gid = cfg.gid.unwrap_or_else(|| unsafe { libc::getgid() });
    let target_user = cfg
        .uid
        .and_then(|uid| nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid)).ok())
        .flatten();

    match target_user {
        // Rebuild the list from the target user's real group membership, as the C build does.
        Some(user) => {
            let name = std::ffi::CString::new(user.name.as_bytes())
                .context("user name contains an interior NUL")?;
            if unsafe { libc::initgroups(name.as_ptr(), target_gid) } != 0 {
                return Err(std::io::Error::last_os_error()).context("initgroups failed");
            }
        }
        // No user to look up, so there is no membership to rebuild — drop the list entirely
        // rather than leaving the caller's groups in place.
        None => {
            if unsafe { libc::setgroups(0, std::ptr::null()) } != 0 {
                return Err(std::io::Error::last_os_error()).context("setgroups failed");
            }
        }
    }

    if let Some(gid) = cfg.gid {
        if unsafe { libc::setgid(gid) } != 0 {
            return Err(std::io::Error::last_os_error()).context("setgid failed");
        }
    }
    if let Some(uid) = cfg.uid {
        if unsafe { libc::setuid(uid) } != 0 {
            return Err(std::io::Error::last_os_error()).context("setuid failed");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(mutate: impl FnOnce(&mut Config)) -> Config {
        let mut c = Config {
            argv: vec!["bash".into()],
            command: "bash".into(),
            ..Default::default()
        };
        mutate(&mut c);
        c
    }

    #[test]
    fn no_interface_binds_all_ipv4_addresses() {
        let address = resolve_bind_address(&cfg(|_| {})).expect("resolves");
        assert_eq!(address, IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    }

    #[test]
    fn the_ipv6_flag_binds_the_unspecified_v6_address() {
        // Matches the C build, which clears LWS_SERVER_OPTION_DISABLE_IPV6 for `-6`.
        let address = resolve_bind_address(&cfg(|c| c.ipv6 = true)).expect("resolves");
        assert_eq!(address, IpAddr::V6(Ipv6Addr::UNSPECIFIED));
    }

    #[test]
    fn a_literal_address_is_used_as_given() {
        let address =
            resolve_bind_address(&cfg(|c| c.iface = Some("127.0.0.1".into()))).expect("resolves");
        assert_eq!(address, IpAddr::V4(Ipv4Addr::LOCALHOST));
    }

    #[test]
    fn an_interface_name_resolves_to_its_address() {
        // `-i eth0` in the C help means a device name, not an address.
        let address =
            resolve_bind_address(&cfg(|c| c.iface = Some("lo".into()))).expect("resolves");
        assert_eq!(address, IpAddr::V4(Ipv4Addr::LOCALHOST));
    }

    #[test]
    fn an_unknown_interface_is_an_error_rather_than_a_silent_wildcard() {
        let result = resolve_bind_address(&cfg(|c| c.iface = Some("nosuchdev0".into())));
        assert!(
            result.is_err(),
            "an unknown interface must not bind everything"
        );
    }
}
