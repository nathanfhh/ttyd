//! Command line parsing, reproducing the `getopt_long` behaviour of the C implementation.
//!
//! The C version first scans for the first non-option argument to locate where the child
//! command begins, then parses options only from the arguments preceding it. Everything
//! from that point on — including anything that looks like an option — becomes the command
//! line of the spawned program. This module reproduces that two-pass structure exactly.

use crate::jsonc;
use crate::utils::{get_sig, get_sig_name};
use std::path::PathBuf;

pub const VERSION: &str = env!("TTYD_VERSION");

/// Buffer sizes the C implementation imposes through fixed-size `char` arrays. They are
/// observable (values are silently truncated), so the port keeps them.
const MAX_IFACE: usize = 127;
const MAX_SOCKET_OWNER: usize = 127;
const MAX_TERMINAL_TYPE: usize = 29;
const MAX_BASE_PATH: usize = 127;
const MAX_SSL_PATH: usize = 1023;

/// URL paths the server answers on, shifted by `--base-path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    pub ws: String,
    pub index: String,
    pub token: String,
    pub parent: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            ws: "/ws".into(),
            index: "/".into(),
            token: "/token".into(),
            parent: String::new(),
        }
    }
}

impl Endpoints {
    /// Builds the endpoint set for a `--base-path` value.
    ///
    /// The value reaches the router verbatim, so it is normalized and checked here: a
    /// missing leading slash is a common typo worth accepting, but braces are route-capture
    /// syntax and would silently turn the endpoints into wildcard matches.
    fn with_base_path(base: &str) -> Result<Self, String> {
        let trimmed = base.trim_end_matches('/');
        if trimmed.is_empty() {
            return Ok(Self::default());
        }
        let normalized = if trimmed.starts_with('/') {
            trimmed.to_string()
        } else {
            format!("/{trimmed}")
        };
        if normalized.contains(['{', '}', '?', '#']) {
            return Err(format!("ttyd: invalid base path: {base}"));
        }
        Ok(Self {
            ws: format!("{normalized}/ws"),
            index: format!("{normalized}/"),
            token: format!("{normalized}/token"),
            parent: normalized,
        })
    }
}

/// How incoming requests are authenticated. Exactly one strategy is active at a time,
/// matching the precedence the C version applies (auth header wins over basic credentials).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthMode {
    None,
    /// `--credential`: HTTP basic auth against a fixed base64 `user:pass` blob.
    Basic {
        credential: String,
    },
    /// `--auth-header`: trust a reverse proxy to have authenticated, read identity from a header.
    TrustedHeader {
        header: String,
    },
    /// `--auth-url`: delegate each request to an external endpoint, like nginx `auth_request`.
    Forward(ForwardAuthConfig),
}

/// Settings for the forward-auth (nginx `auth_request` / Traefik ForwardAuth) strategy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardAuthConfig {
    pub url: String,
    pub method: String,
    /// Request headers copied from the original request into the auth subrequest.
    pub request_headers: Vec<String>,
    /// Response header of the auth endpoint carrying the authenticated user name.
    pub user_header: String,
    /// Seconds to cache an auth decision; 0 disables caching.
    pub cache_ttl: u64,
}

impl ForwardAuthConfig {
    fn new(url: String) -> Self {
        Self {
            url,
            method: "GET".into(),
            request_headers: Vec::new(),
            user_header: "X-Auth-User".into(),
            cache_ttl: 0,
        }
    }

    /// Headers forwarded when the user did not narrow the list down explicitly.
    pub const DEFAULT_REQUEST_HEADERS: &'static [&'static str] = &["cookie", "authorization"];

    pub fn effective_request_headers(&self) -> Vec<String> {
        if self.request_headers.is_empty() {
            Self::DEFAULT_REQUEST_HEADERS
                .iter()
                .map(|h| (*h).to_string())
                .collect()
        } else {
            self.request_headers
                .iter()
                .map(|h| h.to_ascii_lowercase())
                .collect()
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub iface: Option<String>,
    pub socket_owner: Option<String>,
    pub auth: AuthMode,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub sig_code: i32,
    pub sig_name: String,
    pub cwd: Option<PathBuf>,
    pub index: Option<PathBuf>,
    pub endpoints: Endpoints,
    pub ping_interval: u64,
    pub srv_buf_size: usize,
    pub ipv6: bool,
    pub ssl: bool,
    pub ssl_cert: Option<PathBuf>,
    pub ssl_key: Option<PathBuf>,
    pub ssl_ca: Option<PathBuf>,
    pub url_arg: bool,
    pub writable: bool,
    pub terminal_type: String,
    /// Replaces the window title sent to the browser. Without it the title is the command
    /// line and host name, which exposes the command to anyone who can open a session.
    pub title: Option<String>,
    pub prefs_json: String,
    pub check_origin: bool,
    pub max_clients: usize,
    pub once: bool,
    pub exit_no_conn: bool,
    pub browser: bool,
    pub debug_level: i32,
    /// The child command and its arguments.
    pub argv: Vec<String>,
    /// The command rendered back as a single space-joined string, used in the window title.
    pub command: String,
}

impl Config {
    /// The base64 `user:pass` blob when basic auth is active. This doubles as the token the
    /// browser echoes back over the WebSocket, matching the C implementation.
    pub fn credential(&self) -> Option<&str> {
        match &self.auth {
            AuthMode::Basic { credential } => Some(credential.as_str()),
            _ => None,
        }
    }

    /// Whether the WebSocket layer must see a matching `AuthToken` before spawning a process.
    pub fn requires_ws_token(&self) -> bool {
        matches!(self.auth, AuthMode::Basic { .. })
    }

    /// The window title announced to the browser once a terminal opens. It defaults to the
    /// command line and host name, which `--title` replaces for deployments where the
    /// command itself should not be visible to whoever opens the terminal.
    pub fn window_title(&self) -> String {
        match &self.title {
            Some(title) => title.clone(),
            None => format!("{} ({})", self.command, crate::utils::hostname()),
        }
    }

    pub fn is_unix_socket(&self) -> bool {
        self.iface
            .as_deref()
            .is_some_and(|i| i.ends_with(".sock") || i.ends_with(".socket"))
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 7681,
            iface: None,
            socket_owner: None,
            auth: AuthMode::None,
            uid: None,
            gid: None,
            sig_code: 1,
            sig_name: "SIGHUP".into(),
            cwd: None,
            index: None,
            endpoints: Endpoints::default(),
            ping_interval: 5,
            srv_buf_size: 4096,
            ipv6: false,
            ssl: false,
            ssl_cert: None,
            ssl_key: None,
            ssl_ca: None,
            url_arg: false,
            writable: false,
            terminal_type: "xterm-256color".into(),
            title: None,
            prefs_json: "{ }".into(),
            check_origin: false,
            max_clients: 0,
            once: false,
            exit_no_conn: false,
            browser: false,
            debug_level: 7,
            argv: Vec::new(),
            command: String::new(),
        }
    }
}

/// What `parse` decided the process should do next.
pub enum Outcome {
    Run(Box<Config>),
    /// Print nothing more and terminate with this status.
    Exit(i32),
}

struct OptSpec {
    short: Option<char>,
    long: &'static str,
    takes_arg: bool,
}

const fn opt(short: Option<char>, long: &'static str, takes_arg: bool) -> OptSpec {
    OptSpec {
        short,
        long,
        takes_arg,
    }
}

const OPTIONS: &[OptSpec] = &[
    opt(Some('p'), "port", true),
    opt(Some('i'), "interface", true),
    opt(Some('U'), "socket-owner", true),
    opt(Some('c'), "credential", true),
    opt(Some('H'), "auth-header", true),
    opt(Some('u'), "uid", true),
    opt(Some('g'), "gid", true),
    opt(Some('s'), "signal", true),
    opt(Some('w'), "cwd", true),
    opt(Some('I'), "index", true),
    opt(Some('b'), "base-path", true),
    opt(Some('P'), "ping-interval", true),
    opt(Some('f'), "srv-buf-size", true),
    opt(Some('6'), "ipv6", false),
    opt(Some('S'), "ssl", false),
    opt(Some('C'), "ssl-cert", true),
    opt(Some('K'), "ssl-key", true),
    opt(Some('A'), "ssl-ca", true),
    opt(Some('a'), "url-arg", false),
    opt(Some('W'), "writable", false),
    opt(Some('T'), "terminal-type", true),
    opt(None, "title", true),
    opt(Some('t'), "client-option", true),
    opt(Some('O'), "check-origin", false),
    opt(Some('m'), "max-clients", true),
    opt(Some('o'), "once", false),
    opt(Some('q'), "exit-no-conn", false),
    opt(Some('B'), "browser", false),
    opt(Some('d'), "debug", true),
    opt(Some('v'), "version", false),
    opt(Some('h'), "help", false),
    // Forward authentication, new in this port.
    opt(Some('R'), "auth-url", true),
    opt(Some('F'), "auth-request-header", true),
    opt(Some('N'), "auth-user-header", true),
    opt(None, "auth-method", true),
    opt(None, "auth-cache-ttl", true),
];

/// Largest `--srv-buf-size` accepted. The buffer is allocated once per session, so an
/// unbounded value is an allocation failure waiting for the first client. 16 MiB is four
/// thousand times the default and far beyond any useful terminal read.
const MAX_SRV_BUF_SIZE: u64 = 16 * 1024 * 1024;

fn find_short(c: char) -> Option<&'static OptSpec> {
    OPTIONS.iter().find(|o| o.short == Some(c))
}

fn find_long(name: &str) -> Option<&'static OptSpec> {
    OPTIONS.iter().find(|o| o.long == name)
}

/// A parsed option occurrence, identified by its long name.
struct Parsed {
    name: &'static str,
    value: Option<String>,
}

pub fn print_help() {
    eprint!(
        "\
ttyd is a tool for sharing terminal over the web

USAGE:
    ttyd [options] <command> [<arguments...>]

VERSION:
    {VERSION}

OPTIONS:
    -p, --port              Port to listen (default: 7681, use `0` for random port)
    -i, --interface         Network interface to bind (eg: eth0), or UNIX domain socket path (eg: /var/run/ttyd.sock)
    -U, --socket-owner      User owner of the UNIX domain socket file, when enabled (eg: user:group)
    -c, --credential        Credential for basic authentication (format: username:password)
    -H, --auth-header       HTTP Header name for auth proxy, this will configure ttyd to let a HTTP reverse proxy handle authentication
    -u, --uid               User id to run with
    -g, --gid               Group id to run with
    -s, --signal            Signal to send to the command when exit it (default: 1, SIGHUP)
    -w, --cwd               Working directory to be set for the child program
    -a, --url-arg           Allow client to send command line arguments in URL (eg: http://localhost:7681?arg=foo&arg=bar)
    -W, --writable          Allow clients to write to the TTY (readonly by default)
    -t, --client-option     Send option to client (format: key=value), repeat to add more options
    -T, --terminal-type     Terminal type to report, default: xterm-256color
        --title             Window title to send to the browser. Without it the title is
                            the full command line and host name, which every client that
                            opens a session can read
    -O, --check-origin      Do not allow websocket connection from different origin
    -m, --max-clients       Maximum clients to support (default: 0, no limit)
    -o, --once              Accept only one client and exit on disconnection
    -q, --exit-no-conn      Exit on all clients disconnection
    -B, --browser           Open terminal with the default system browser
    -I, --index             Custom index.html path
    -b, --base-path         Expected base path for requests coming from a reverse proxy (eg: /mounted/here, max length: 128)
    -P, --ping-interval     Websocket ping interval(sec) (default: 5)
    -f, --srv-buf-size      Maximum chunk of file (in bytes) that can be sent at once, a larger value may improve throughput (default: 4096)
    -6, --ipv6              Enable IPv6 support
    -S, --ssl               Enable SSL
    -C, --ssl-cert          SSL certificate file path
    -K, --ssl-key           SSL key file path
    -A, --ssl-ca            SSL CA file path for client certificate verification
    -d, --debug             Set log level (default: 7)
    -v, --version           Print the version and exit
    -h, --help              Print this text and exit

FORWARD AUTHENTICATION:
    -R, --auth-url          Delegate authentication to this URL, like nginx `auth_request`.
                            A 2xx response admits the request, anything else rejects it.
    -F, --auth-request-header
                            Request header to copy into the auth subrequest, repeat to add
                            more (default: Cookie, Authorization)
    -N, --auth-user-header  Auth response header carrying the user name, exposed to the
                            child process as TTYD_USER (default: X-Auth-User)
        --auth-method       HTTP method for the auth subrequest (default: GET)
        --auth-cache-ttl    Seconds to cache an auth decision (default: 0, no caching)

Visit https://github.com/tsl0922/ttyd to get more information and report bugs.
"
    );
}

/// Locates the first non-option argument, which is where the child command begins.
/// Mirrors how the C version derives `start` from `getopt_long`'s `optind`.
///
/// This walks arguments by the same rules as [`collect_options`] — `--`, inline `=` values,
/// short-option clusters, unknown options consuming nothing. **The two must stay in
/// agreement**: if they disagree, the command would start in a different place than the
/// options were parsed from, and nothing would fail to compile.
/// `command_start_agrees_with_option_parsing` guards the pair.
fn find_command_start(args: &[String]) -> usize {
    let mut i = 1;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            return i + 1;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let name = long.split('=').next().unwrap_or(long);
            let has_inline_value = long.contains('=');
            if let Some(spec) = find_long(name) {
                if spec.takes_arg && !has_inline_value {
                    i += 1;
                }
            }
            i += 1;
        } else if arg.starts_with('-') && arg.len() > 1 {
            let chars: Vec<char> = arg[1..].chars().collect();
            for (idx, c) in chars.iter().enumerate() {
                let takes_arg = find_short(*c).is_some_and(|s| s.takes_arg);
                if takes_arg {
                    // The value is either the rest of this cluster or the next argument.
                    if idx + 1 == chars.len() {
                        i += 1;
                    }
                    break;
                }
            }
            i += 1;
        } else {
            return i;
        }
    }
    args.len()
}

/// Splits `args[1..end]` into option occurrences. Unknown options are ignored, matching
/// the C version's `case '?': break;`.
///
/// Walks by the same rules as [`find_command_start`]; see the note there.
fn collect_options(args: &[String], end: usize) -> Result<Vec<Parsed>, String> {
    let mut out = Vec::new();
    let mut i = 1;
    while i < end {
        let arg = &args[i];
        if arg == "--" {
            break;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            match find_long(name) {
                Some(spec) => {
                    let value = if spec.takes_arg {
                        match inline {
                            Some(v) => Some(v),
                            None => {
                                i += 1;
                                if i >= end {
                                    return Err(format!(
                                        "ttyd: option '--{name}' requires an argument"
                                    ));
                                }
                                Some(args[i].clone())
                            }
                        }
                    } else {
                        None
                    };
                    out.push(Parsed {
                        name: spec.long,
                        value,
                    });
                }
                None => eprintln!("ttyd: unrecognized option '--{name}'"),
            }
            i += 1;
        } else if arg.starts_with('-') && arg.len() > 1 {
            let chars: Vec<char> = arg[1..].chars().collect();
            let mut idx = 0;
            while idx < chars.len() {
                let c = chars[idx];
                match find_short(c) {
                    Some(spec) if spec.takes_arg => {
                        let value = if idx + 1 < chars.len() {
                            chars[idx + 1..].iter().collect::<String>()
                        } else {
                            i += 1;
                            if i >= end {
                                return Err(format!("ttyd: option requires an argument -- '{c}'"));
                            }
                            args[i].clone()
                        };
                        out.push(Parsed {
                            name: spec.long,
                            value: Some(value),
                        });
                        break;
                    }
                    Some(spec) => out.push(Parsed {
                        name: spec.long,
                        value: None,
                    }),
                    None => eprintln!("ttyd: invalid option -- '{c}'"),
                }
                idx += 1;
            }
            i += 1;
        } else {
            break;
        }
    }
    Ok(out)
}

fn truncate(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    let mut end = max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

/// Parses an integer the way the C `parse_int` helper does, exiting with status 1 on failure.
fn parse_int(name: &str, raw: &str) -> Result<i64, i32> {
    let trimmed = raw.trim();
    let parsed = if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        i64::from_str_radix(hex, 16)
    } else {
        trimmed.parse::<i64>()
    };
    match parsed {
        Ok(v) => Ok(v),
        Err(_) => {
            eprintln!("ttyd: invalid value for {name}: {raw}");
            Err(1)
        }
    }
}

pub fn parse(args: &[String]) -> Outcome {
    match parse_inner(args) {
        Ok(outcome) => outcome,
        Err(code) => Outcome::Exit(code),
    }
}

fn parse_inner(args: &[String]) -> Result<Outcome, i32> {
    if args.len() <= 1 {
        print_help();
        return Ok(Outcome::Exit(0));
    }

    let start = find_command_start(args);
    let parsed = match collect_options(args, start) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("{msg}");
            return Err(255);
        }
    };

    let mut cfg = Config::default();
    let mut prefs = serde_json::Map::new();
    let mut credential: Option<String> = None;
    let mut auth_header: Option<String> = None;
    let mut forward: Option<ForwardAuthConfig> = None;
    let mut forward_method: Option<String> = None;
    let mut forward_user_header: Option<String> = None;
    let mut forward_cache_ttl: Option<u64> = None;
    let mut forward_request_headers: Vec<String> = Vec::new();
    let mut base_path: Option<String> = None;

    for Parsed { name, value } in parsed {
        // Every option in the table with `takes_arg` is guaranteed a value by `collect_options`.
        let arg = value.unwrap_or_default();
        match name {
            "help" => {
                print_help();
                return Ok(Outcome::Exit(0));
            }
            "version" => {
                println!("ttyd version {VERSION}");
                return Ok(Outcome::Exit(0));
            }
            "debug" => cfg.debug_level = parse_int("debug", &arg)? as i32,
            "url-arg" => cfg.url_arg = true,
            "writable" => cfg.writable = true,
            "check-origin" => cfg.check_origin = true,
            "max-clients" => cfg.max_clients = parse_int("max-clients", &arg)?.max(0) as usize,
            "once" => cfg.once = true,
            "exit-no-conn" => cfg.exit_no_conn = true,
            "browser" => cfg.browser = true,
            "port" => {
                let port = parse_int("port", &arg)?;
                if !(0..=65535).contains(&port) {
                    eprintln!("ttyd: invalid port: {arg}");
                    return Err(255);
                }
                cfg.port = port as u16;
            }
            "interface" => cfg.iface = Some(truncate(&arg, MAX_IFACE)),
            "socket-owner" => cfg.socket_owner = Some(truncate(&arg, MAX_SOCKET_OWNER)),
            "credential" => {
                if !arg.contains(':') {
                    eprintln!("ttyd: invalid credential, format: username:password");
                    return Err(255);
                }
                use base64::Engine;
                credential = Some(base64::engine::general_purpose::STANDARD.encode(arg.as_bytes()));
            }
            "auth-header" => auth_header = Some(arg),
            "uid" => cfg.uid = Some(parse_int("uid", &arg)?.max(0) as u32),
            "gid" => cfg.gid = Some(parse_int("gid", &arg)?.max(0) as u32),
            "signal" => {
                let sig = get_sig(&arg);
                if sig > 0 {
                    cfg.sig_code = sig;
                    cfg.sig_name = get_sig_name(sig);
                } else {
                    eprintln!("ttyd: invalid signal: {arg}");
                    return Err(255);
                }
            }
            "cwd" => cfg.cwd = Some(PathBuf::from(arg)),
            "index" => {
                let expanded = match arg.strip_prefix("~/") {
                    Some(rest) => match std::env::var("HOME") {
                        Ok(home) => format!("{home}/{rest}"),
                        Err(_) => arg.clone(),
                    },
                    None => arg.clone(),
                };
                let path = PathBuf::from(&expanded);
                match std::fs::metadata(&path) {
                    Err(e) => {
                        eprintln!("Can not stat index.html: {expanded}, error: {e}");
                        return Err(255);
                    }
                    Ok(meta) if meta.is_dir() => {
                        eprintln!("Invalid index.html path: {expanded}, is it a dir?");
                        return Err(255);
                    }
                    Ok(_) => cfg.index = Some(path),
                }
            }
            "base-path" => base_path = Some(truncate(&arg, MAX_BASE_PATH)),
            "ping-interval" => {
                let interval = parse_int("ping-interval", &arg)?;
                if interval < 0 {
                    eprintln!("ttyd: invalid ping interval: {arg}");
                    return Err(255);
                }
                cfg.ping_interval = interval as u64;
            }
            "srv-buf-size" => {
                let size = parse_int("srv-buf-size", &arg)?;
                if size < 0 {
                    eprintln!("ttyd: invalid srv-buf-size: {arg}");
                    return Err(255);
                }
                if size > 0 {
                    // Clamped, because this value is allocated per session: `-f 9999999999999`
                    // starts fine and then kills the server on the first connection. The C
                    // build survives the same argument, so crashing there would be a
                    // regression as well as a denial of service by typo. Reported rather than
                    // applied silently.
                    let size = size as u64;
                    if size > MAX_SRV_BUF_SIZE {
                        eprintln!(
                            "ttyd: srv-buf-size {size} is above the {MAX_SRV_BUF_SIZE} byte \
                             maximum, using {MAX_SRV_BUF_SIZE}"
                        );
                    }
                    cfg.srv_buf_size = size.min(MAX_SRV_BUF_SIZE) as usize;
                }
            }
            "ipv6" => cfg.ipv6 = true,
            "ssl" => cfg.ssl = true,
            "ssl-cert" => cfg.ssl_cert = Some(PathBuf::from(truncate(&arg, MAX_SSL_PATH))),
            "ssl-key" => cfg.ssl_key = Some(PathBuf::from(truncate(&arg, MAX_SSL_PATH))),
            "ssl-ca" => cfg.ssl_ca = Some(PathBuf::from(truncate(&arg, MAX_SSL_PATH))),
            "terminal-type" => cfg.terminal_type = truncate(&arg, MAX_TERMINAL_TYPE),
            "title" => cfg.title = Some(arg),
            "client-option" => {
                // Unlike the C version, the value keeps every character after the first `=`;
                // the original truncated at a second `=`, silently dropping part of the value.
                let Some((key, raw)) = arg.split_once('=') else {
                    eprintln!("ttyd: invalid client option: {arg}, format: key=value");
                    return Err(255);
                };
                let value = serde_json::from_str::<serde_json::Value>(raw)
                    .unwrap_or_else(|_| serde_json::Value::String(raw.to_string()));
                prefs.insert(key.to_string(), value);
            }
            "auth-url" => {
                // Validated here rather than per request: a typo would otherwise start
                // cleanly and then fail every subrequest, and forward auth fails closed —
                // so the first real traffic turns a typo into a total outage.
                match arg.parse::<axum::http::Uri>() {
                    Ok(uri)
                        if matches!(
                            uri.scheme_str(),
                            Some(scheme) if scheme.eq_ignore_ascii_case("http")
                                || scheme.eq_ignore_ascii_case("https")
                        ) && uri.authority().is_some() => {}
                    _ => {
                        eprintln!("ttyd: invalid auth-url: {arg}");
                        return Err(255);
                    }
                }
                forward = Some(ForwardAuthConfig::new(arg));
            }
            "auth-request-header" => forward_request_headers.push(arg),
            "auth-user-header" => forward_user_header = Some(arg),
            "auth-method" => {
                let method = arg.to_ascii_uppercase();
                if axum::http::Method::from_bytes(method.as_bytes()).is_err() {
                    eprintln!("ttyd: invalid auth-method: {arg}");
                    return Err(255);
                }
                forward_method = Some(method);
            }
            "auth-cache-ttl" => {
                forward_cache_ttl = Some(parse_int("auth-cache-ttl", &arg)?.max(0) as u64)
            }
            _ => unreachable!("option {name} is in the table but not handled"),
        }
    }

    cfg.prefs_json = jsonc::to_string(&serde_json::Value::Object(prefs));

    if let Some(base) = base_path {
        match Endpoints::with_base_path(&base) {
            Ok(endpoints) => cfg.endpoints = endpoints,
            Err(message) => {
                eprintln!("{message}");
                return Err(255);
            }
        }
    }

    // Authentication precedence matches the C version: an explicit forward-auth endpoint wins,
    // then the trusted proxy header, then basic credentials.
    cfg.auth = if let Some(mut fwd) = forward {
        if let Some(m) = forward_method {
            fwd.method = m;
        }
        if let Some(h) = forward_user_header {
            fwd.user_header = h;
        }
        if let Some(ttl) = forward_cache_ttl {
            fwd.cache_ttl = ttl;
        }
        fwd.request_headers = forward_request_headers;
        AuthMode::Forward(fwd)
    } else if let Some(header) = auth_header {
        AuthMode::TrustedHeader {
            header: header.to_ascii_lowercase(),
        }
    } else if let Some(credential) = credential {
        AuthMode::Basic { credential }
    } else {
        AuthMode::None
    };

    cfg.argv = args[start.min(args.len())..].to_vec();
    cfg.command = cfg.argv.join(" ");

    if cfg.command.is_empty() {
        eprintln!("ttyd: missing start command");
        return Err(255);
    }

    Ok(Outcome::Run(Box::new(cfg)))
}

/// Renders the startup banner the C version writes through `lwsl_notice`.
pub fn config_summary(cfg: &Config) -> Vec<String> {
    let mut lines = vec!["tty configuration:".to_string()];
    if cfg.credential().is_some() {
        // Deliberately not the value. It is base64 of `user:password`, i.e. reversible, and
        // the C build prints it — which puts the password into every log collector that
        // scrapes stdout.
        lines.push("  credential: (basic auth enabled)".to_string());
    }
    lines.push(format!("  start command: {}", cfg.command));
    lines.push(format!(
        "  close signal: {} ({})",
        cfg.sig_name, cfg.sig_code
    ));
    lines.push(format!("  terminal type: {}", cfg.terminal_type));
    if let Some(title) = &cfg.title {
        lines.push(format!("  window title: {title}"));
    }
    if !cfg.endpoints.parent.is_empty() {
        lines.push("endpoints:".to_string());
        lines.push(format!("  base-path: {}", cfg.endpoints.parent));
        lines.push(format!("  index    : {}", cfg.endpoints.index));
        lines.push(format!("  token    : {}", cfg.endpoints.token));
        lines.push(format!("  websocket: {}", cfg.endpoints.ws));
    }
    match &cfg.auth {
        AuthMode::TrustedHeader { header } => lines.push(format!("  auth header: {header}")),
        AuthMode::Forward(f) => {
            lines.push(format!("  forward auth: {} {}", f.method, f.url));
            lines.push(format!(
                "  forward auth headers: {}",
                f.effective_request_headers().join(", ")
            ));
            lines.push(format!("  forward auth user header: {}", f.user_header));
            if f.cache_ttl > 0 {
                lines.push(format!("  forward auth cache: {}s", f.cache_ttl));
            }
        }
        _ => {}
    }
    if cfg.check_origin {
        lines.push("  check origin: true".into());
    }
    if cfg.url_arg {
        lines.push("  allow url arg: true".into());
    }
    if cfg.max_clients > 0 {
        lines.push(format!("  max clients: {}", cfg.max_clients));
    }
    if cfg.once {
        lines.push("  once: true".into());
    }
    if cfg.exit_no_conn {
        lines.push("  exit_no_conn: true".into());
    }
    if let Some(index) = &cfg.index {
        lines.push(format!("  custom index.html: {}", index.display()));
    }
    if let Some(cwd) = &cfg.cwd {
        lines.push(format!("  working directory: {}", cwd.display()));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn an_oversized_send_buffer_is_clamped() {
        // Allocated once per session, so an unbounded value starts cleanly and then kills the
        // server on the first client — measured, before this cap, as the process dying while
        // the C build survived the same argument.
        let cfg = run(&["ttyd", "-f", "9999999999999", "bash"]);
        assert_eq!(cfg.srv_buf_size, MAX_SRV_BUF_SIZE as usize);
        // Anything sane is still taken verbatim.
        assert_eq!(run(&["ttyd", "-f", "65536", "bash"]).srv_buf_size, 65536);
    }

    fn run(items: &[&str]) -> Config {
        match parse(&argv(items)) {
            Outcome::Run(cfg) => *cfg,
            Outcome::Exit(code) => panic!("expected a config, got exit {code}"),
        }
    }

    #[test]
    fn command_starts_at_first_non_option() {
        let cfg = run(&["ttyd", "-W", "-p", "8080", "bash", "-c", "echo hi"]);
        assert_eq!(cfg.argv, argv(&["bash", "-c", "echo hi"]));
        assert_eq!(cfg.command, "bash -c echo hi");
        assert_eq!(cfg.port, 8080);
        assert!(cfg.writable);
    }

    #[test]
    fn options_after_the_command_belong_to_the_command() {
        let cfg = run(&["ttyd", "bash", "-W", "-p", "9999"]);
        assert_eq!(cfg.argv, argv(&["bash", "-W", "-p", "9999"]));
        assert!(
            !cfg.writable,
            "-W after the command must not configure ttyd"
        );
        assert_eq!(cfg.port, 7681);
    }

    #[test]
    fn clustered_short_options_are_split() {
        let cfg = run(&["ttyd", "-Wa6", "bash"]);
        assert!(cfg.writable && cfg.url_arg && cfg.ipv6);
    }

    #[test]
    fn attached_short_option_value_is_read() {
        let cfg = run(&["ttyd", "-p7000", "bash"]);
        assert_eq!(cfg.port, 7000);
    }

    #[test]
    fn long_option_with_equals_is_read() {
        let cfg = run(&["ttyd", "--port=7100", "--terminal-type=vt100", "bash"]);
        assert_eq!(cfg.port, 7100);
        assert_eq!(cfg.terminal_type, "vt100");
    }

    #[test]
    fn the_startup_summary_never_contains_the_credential() {
        let cfg = run(&["ttyd", "-c", "user:pass", "bash"]);
        let summary = config_summary(&cfg).join("\n");
        assert!(
            summary.contains("basic auth enabled"),
            "the summary should still say auth is on: {summary}"
        );
        for secret in ["dXNlcjpwYXNz", "user:pass"] {
            assert!(
                !summary.contains(secret),
                "the credential leaked into the startup summary: {summary}"
            );
        }
    }

    #[test]
    fn credential_is_base64_encoded() {
        let cfg = run(&["ttyd", "-c", "user:pass", "bash"]);
        assert_eq!(cfg.credential(), Some("dXNlcjpwYXNz"));
        assert!(cfg.requires_ws_token());
    }

    #[test]
    fn base_path_shifts_every_endpoint() {
        let cfg = run(&["ttyd", "-b", "/mounted/here/", "bash"]);
        assert_eq!(cfg.endpoints.parent, "/mounted/here");
        assert_eq!(cfg.endpoints.index, "/mounted/here/");
        assert_eq!(cfg.endpoints.token, "/mounted/here/token");
        assert_eq!(cfg.endpoints.ws, "/mounted/here/ws");
    }

    #[test]
    fn a_base_path_without_a_leading_slash_is_normalized() {
        // The router rejects a path with no leading slash by panicking, so this must be
        // handled before it ever gets there.
        let cfg = run(&["ttyd", "-b", "mounted", "bash"]);
        assert_eq!(cfg.endpoints.parent, "/mounted");
        assert_eq!(cfg.endpoints.index, "/mounted/");
    }

    #[test]
    fn a_base_path_with_route_syntax_is_rejected() {
        for bad in ["/a{x}", "/a?b", "/a#b"] {
            match parse(&argv(&["ttyd", "-b", bad, "bash"])) {
                Outcome::Exit(code) => assert_eq!(code, 255, "for {bad}"),
                Outcome::Run(_) => panic!("{bad} should have been rejected"),
            }
        }
    }

    #[test]
    fn command_start_agrees_with_option_parsing() {
        // find_command_start and collect_options implement the same walking rules twice;
        // this pins them together so a future option with unusual arity cannot split them.
        let cases: Vec<Vec<&str>> = vec![
            vec!["ttyd", "-W", "-p", "8080", "bash"],
            vec!["ttyd", "-Wa6", "bash", "-c", "x"],
            vec!["ttyd", "-p7000", "--terminal-type=vt100", "sh"],
            vec!["ttyd", "-Z", "-W", "bash"],
            vec!["ttyd", "--", "-weird"],
            vec!["ttyd", "-t", "a=b", "-c", "u:p", "bash"],
            vec!["ttyd", "--auth-url", "http://x/y", "bash"],
        ];
        for case in cases {
            let args = argv(&case);
            let start = find_command_start(&args);
            // Every argument before the command must be consumed as an option or its value;
            // nothing in that range may look like the start of the command.
            assert!(
                collect_options(&args, start).is_ok(),
                "option parsing failed for {case:?}"
            );
            assert!(start <= args.len(), "start out of range for {case:?}");
            if start < args.len() {
                let first = &args[start];
                assert!(
                    !first.starts_with('-') || case.contains(&"--"),
                    "command start {first:?} looks like an option in {case:?}"
                );
            }
        }
    }

    #[test]
    fn empty_base_path_keeps_defaults() {
        let cfg = run(&["ttyd", "-b", "///", "bash"]);
        assert_eq!(cfg.endpoints, Endpoints::default());
    }

    #[test]
    fn client_options_build_json_c_style_prefs() {
        let cfg = run(&[
            "ttyd",
            "-t",
            "fontSize=20",
            "-t",
            r#"theme={"background":"red"}"#,
            "-t",
            "titleFixed=hello",
            "bash",
        ]);
        assert_eq!(
            cfg.prefs_json,
            r#"{ "fontSize": 20, "theme": { "background": "red" }, "titleFixed": "hello" }"#
        );
    }

    #[test]
    fn no_client_options_yields_empty_json_object() {
        assert_eq!(run(&["ttyd", "bash"]).prefs_json, "{ }");
    }

    #[test]
    fn client_option_keeps_everything_after_the_first_equals() {
        let cfg = run(&["ttyd", "-t", "token=a=b=c", "bash"]);
        assert_eq!(cfg.prefs_json, r#"{ "token": "a=b=c" }"#);
    }

    #[test]
    fn signal_name_and_number_both_work() {
        assert_eq!(run(&["ttyd", "-s", "SIGTERM", "bash"]).sig_code, 15);
        assert_eq!(run(&["ttyd", "-s", "9", "bash"]).sig_code, 9);
        assert_eq!(run(&["ttyd", "-s", "INT", "bash"]).sig_name, "SIGINT");
    }

    #[test]
    fn the_window_title_defaults_to_the_command_and_host() {
        let title = run(&["ttyd", "sh", "-c", "secret-script.sh"]).window_title();
        assert!(
            title.starts_with("sh -c secret-script.sh ("),
            "title was {title:?}"
        );
        assert!(title.ends_with(')'), "title was {title:?}");
    }

    #[test]
    fn the_title_option_replaces_the_command_line_entirely() {
        let cfg = run(&[
            "ttyd",
            "--title",
            "Support Console",
            "sh",
            "-c",
            "secret.sh",
        ]);
        assert_eq!(cfg.window_title(), "Support Console");
        assert!(
            !cfg.window_title().contains("secret.sh"),
            "the command line must not leak into an overridden title"
        );
    }

    #[test]
    fn an_empty_title_is_honoured() {
        // Deliberately blank is a valid way to say "tell the browser nothing".
        let cfg = run(&["ttyd", "--title", "", "sh", "-c", "secret.sh"]);
        assert_eq!(cfg.window_title(), "");
    }

    #[test]
    fn terminal_type_is_truncated_like_the_c_buffer() {
        let long = "x".repeat(50);
        let cfg = run(&["ttyd", "-T", &long, "bash"]);
        assert_eq!(cfg.terminal_type.len(), MAX_TERMINAL_TYPE);
    }

    #[test]
    fn missing_command_exits_255() {
        match parse(&argv(&["ttyd", "-p", "8080"])) {
            Outcome::Exit(code) => assert_eq!(code, 255),
            Outcome::Run(_) => panic!("expected failure"),
        }
    }

    #[test]
    fn credential_without_colon_exits_255() {
        match parse(&argv(&["ttyd", "-c", "nocolon", "bash"])) {
            Outcome::Exit(code) => assert_eq!(code, 255),
            Outcome::Run(_) => panic!("expected failure"),
        }
    }

    #[test]
    fn invalid_signal_exits_255() {
        match parse(&argv(&["ttyd", "-s", "NOPE", "bash"])) {
            Outcome::Exit(code) => assert_eq!(code, 255),
            Outcome::Run(_) => panic!("expected failure"),
        }
    }

    #[test]
    fn invalid_integer_exits_1() {
        match parse(&argv(&["ttyd", "-p", "abc", "bash"])) {
            Outcome::Exit(code) => assert_eq!(code, 1),
            Outcome::Run(_) => panic!("expected failure"),
        }
    }

    #[test]
    fn no_arguments_prints_help_and_exits_0() {
        match parse(&argv(&["ttyd"])) {
            Outcome::Exit(code) => assert_eq!(code, 0),
            Outcome::Run(_) => panic!("expected help"),
        }
    }

    #[test]
    fn unknown_option_is_ignored_like_getopt() {
        let cfg = run(&["ttyd", "-Z", "-W", "bash"]);
        assert!(cfg.writable);
        assert_eq!(cfg.argv, argv(&["bash"]));
    }

    #[test]
    fn double_dash_starts_the_command() {
        let cfg = run(&["ttyd", "-W", "--", "-weird-command"]);
        assert_eq!(cfg.argv, argv(&["-weird-command"]));
        assert!(cfg.writable);
    }

    #[test]
    fn auth_header_is_lowercased() {
        let cfg = run(&["ttyd", "-H", "X-Remote-User", "bash"]);
        assert_eq!(
            cfg.auth,
            AuthMode::TrustedHeader {
                header: "x-remote-user".into()
            }
        );
    }

    #[test]
    fn forward_auth_defaults() {
        let cfg = run(&["ttyd", "--auth-url", "http://auth.local/verify", "bash"]);
        let AuthMode::Forward(f) = &cfg.auth else {
            panic!("expected forward auth, got {:?}", cfg.auth);
        };
        assert_eq!(f.url, "http://auth.local/verify");
        assert_eq!(f.method, "GET");
        assert_eq!(f.user_header, "X-Auth-User");
        assert_eq!(f.cache_ttl, 0);
        assert_eq!(
            f.effective_request_headers(),
            vec!["cookie", "authorization"]
        );
    }

    #[test]
    fn forward_auth_is_fully_configurable() {
        let cfg = run(&[
            "ttyd",
            "-R",
            "http://auth/v",
            "-F",
            "Cookie",
            "-F",
            "X-Token",
            "-N",
            "X-User",
            "--auth-method",
            "post",
            "--auth-cache-ttl",
            "30",
            "bash",
        ]);
        let AuthMode::Forward(f) = &cfg.auth else {
            panic!("expected forward auth");
        };
        assert_eq!(f.method, "POST");
        assert_eq!(f.user_header, "X-User");
        assert_eq!(f.cache_ttl, 30);
        assert_eq!(f.effective_request_headers(), vec!["cookie", "x-token"]);
    }

    #[test]
    fn forward_auth_takes_precedence_over_basic_and_header() {
        let cfg = run(&[
            "ttyd",
            "-c",
            "a:b",
            "-H",
            "X-User",
            "-R",
            "http://auth/v",
            "bash",
        ]);
        assert!(matches!(cfg.auth, AuthMode::Forward(_)));
        assert_eq!(cfg.credential(), None);
    }

    #[test]
    fn unix_socket_is_detected_from_the_interface_suffix() {
        assert!(run(&["ttyd", "-i", "/run/ttyd.sock", "bash"]).is_unix_socket());
        assert!(run(&["ttyd", "-i", "/run/ttyd.socket", "bash"]).is_unix_socket());
        assert!(!run(&["ttyd", "-i", "eth0", "bash"]).is_unix_socket());
    }
}
