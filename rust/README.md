# ttyd — Rust port

[繁體中文](README.zh-TW.md) | **English**

A Rust implementation of [ttyd](https://github.com/tsl0922/ttyd), keeping the command line,
the HTTP surface and the `tty` WebSocket protocol compatible with the C original, and adding
**forward authentication** so ttyd can be protected by an existing SSO or auth service
instead of only a static basic-auth credential.

The frontend is untouched: the build reads the same `src/html.h` the C build embeds, so both
serve a byte-identical bundle.

## Build

```sh
cd rust
cargo build --release
# target/release/ttyd
```

The only external requirement is a [Rust toolchain](https://rustup.rs) — no libwebsockets,
libuv or json-c. Every command-line option of the C build is accepted, so an existing
invocation works unchanged:

```sh
./target/release/ttyd -W bash
```

## Hiding the command line: `--title`

The window title the server announces defaults to the full command line plus the host name,
and every client that opens a session receives it. When the command contains anything
sensitive — a script path, a host, a key — `--title` replaces it outright:

```sh
ttyd --title "Support Console" -W /opt/ops/reset-account.sh --token abc123
```

The browser tab then reads `Support Console`, and the command line is never put on the wire.
This is not the same as the existing `-t titleFixed=…` client option, which only changes what
the browser displays after the real title has already been sent.

## Forward authentication

The C version offers two ways to protect a terminal: a static `user:password` credential
(`--credential`), or blind trust in a header a reverse proxy sets (`--auth-header`). Neither
integrates with an identity provider. Forward auth fills that gap the same way nginx's
`auth_request` and Traefik's ForwardAuth middleware do: every request — including the
WebSocket upgrade — is first sent to an endpoint you control, and only a 2xx answer lets it
through.

```text
-R, --auth-url            Delegate authentication to this URL
-F, --auth-request-header Request header to copy into the auth subrequest, repeat to add
                          more (default: Cookie, Authorization)
-N, --auth-user-header    Auth response header carrying the user name, exposed to the
                          child process as TTYD_USER (default: X-Auth-User)
    --auth-method         HTTP method for the auth subrequest (default: GET)
    --auth-cache-ttl      Seconds to cache an auth decision (default: 0, no caching)
```

```sh
ttyd --auth-url https://sso.internal/verify \
     --auth-request-header Cookie \
     --auth-user-header X-Auth-User \
     --auth-cache-ttl 30 \
     -W bash
```

Behaviour worth knowing:

- The subrequest carries `X-Original-Method`, `X-Original-URI`, `X-Forwarded-Method`,
  `X-Forwarded-Uri`, `X-Forwarded-Proto`, `X-Forwarded-Host` and `X-Forwarded-For`, so
  services written for nginx or Traefik work unchanged.
- `X-Forwarded-For` carries **only the address ttyd itself observed**. A client-supplied
  `X-Forwarded-For` is discarded rather than appended to, because ttyd running forward auth
  is normally the edge: there is no trusted hop upstream, so anything the client sent is
  attacker-chosen. `X-Forwarded-Host`, in contrast, is by definition the client's own `Host`
  header — treat it as untrusted input in your auth service.
- A non-2xx answer is relayed to the browser along with `WWW-Authenticate`,
  `Proxy-Authenticate`, `Location`, `Set-Cookie` and `Cache-Control`. A `302` to your login
  page therefore works end to end.
- If the endpoint cannot be reached the request is refused with `500`. An auth outage never
  admits anyone.
- Only successful decisions are cached. Caching a denial would keep a user locked out for
  the rest of the TTL after they had logged in.
- The cache key is derived from the **complete set of inputs the subrequest carries** — the
  method, the URI, the forwarded request headers and every `X-Forwarded-*` / `X-Original-*`
  value. Whatever your endpoint is entitled to decide on is part of the key, so a verdict is
  never replayed for a request the endpoint would have refused.
- `--auth-url` takes precedence over `--credential` and `--auth-header`, and the `/token`
  endpoint stops handing out a credential.

Operating limits worth planning for:

- Every request that is not served from the cache produces one subrequest, and rejections
  are deliberately never cached — so unauthenticated traffic, the kind an attacker fully
  controls, always reaches your identity provider. Put a rate-limiting reverse proxy in
  front of ttyd if it is exposed to the open internet.
- Because an unreachable endpoint fails closed with `500`, an auth-service outage takes the
  terminal down with it. That is the intended trade; size the endpoint accordingly.

## Testing

The port is validated against the C build with a differential characterization suite. See
[PARITY.md](PARITY.md) for the method, the results, and the behavioural differences it
found.

```sh
cargo test                                  # everything, against this build
./run-parity-tests.sh /path/to/c/ttyd       # and again against the C build, compared
```

Those suites drive the protocol with a synthetic client, which proves the wire format but
not that the shipped frontend works against it. `browser-check.py` closes that gap: it opens
the page in a real Chromium through Playwright and checks that xterm.js mounts, keystrokes
reach the shell, `TERM` and window resizes arrive, a full-screen program (`vi`) round-trips,
and the session never drops. It also leaves screenshots behind for a look at colour, CJK and
box-drawing output.

```sh
pip install playwright && playwright install chromium
python3 browser-check.py                          # this build
python3 browser-check.py /path/to/c/ttyd c        # and the C build, for comparison
TTYD_BROWSER_TLS=1 python3 browser-check.py       # the same run over HTTPS
```

It uses Playwright's own Chromium. On an image that ships a pinned browser build the
managed launch can fail even though a usable Chromium is installed, in which case the
script falls back to one found under `PLAYWRIGHT_BROWSERS_PATH` and says so; set
`TTYD_CHROMIUM=/path/to/chrome` to choose the binary yourself.

`browser-check.py` proves the frontend works; it does not prove a terminal survives being
left open. `e2e-soak.py` holds one real browser session for twenty minutes under concurrent
load, asking it every thirty seconds to run a command and prove the shell executed it, and
samples the server throughout:

```sh
python3 e2e-soak.py                          # 20 minutes, the default
python3 e2e-soak.py ./target/release/ttyd 300  # or a shorter run
```

It fails if the session ever stops working, if the browser sees a WebSocket close, or if the
server's descriptors or threads grow. Because every probe records the shell's own PID, a
silent reconnect — which looks identical on screen, since the frontend reconnects by itself —
shows up as a changed PID rather than passing unnoticed.

`bench.py` produces the performance table in `PARITY.md`, running both builds interleaved so
machine drift lands on both. It refuses to start on a machine that already has load generators
running, sweeps for any it orphans, and reports the count — a benchmark that becomes its own
load is the failure this harness is built to make visible:

```sh
python3 bench.py        # 5 rounds, the default
python3 bench.py 1      # a quick check
```

It expects the C reference at `../build-c/ttyd`; override with `TTYD_C_BIN` and
`TTYD_RUST_BIN`.

## Supply chain

```sh
cargo audit                                    # RustSec advisories against Cargo.lock
cargo cyclonedx --format json --spec-version 1.5   # writes ttyd.cdx.json
```

The SBOM is generated rather than checked in. It is derived entirely from `Cargo.lock`,
which *is* committed, so any revision can reproduce its own SBOM exactly — whereas a stored
copy silently goes stale on the next dependency bump, and a stale SBOM is worse than none
because it is trusted. Generate it in CI, at release time, or on demand.

Run on 2026-07-26 UTC against 1169 RustSec advisories: 222 crates locked from 24 direct
dependencies, **no known vulnerabilities then**, and one informational warning — `rustls-pemfile` is unmaintained
(RUSTSEC-2025-0134). Re-run the command above rather than trusting this paragraph — a
vulnerability-free result is a statement about a date, not a property of the code. It parses
the PEM files named by `--ssl-cert`, `--ssl-key` and
`--ssl-ca`, which are operator-supplied local files rather than network input.

## Compatibility

Every command-line option, HTTP endpoint, WebSocket message type and authentication mode of
the C implementation is supported. Three behaviours differ on purpose — a failing exit
closes without putting the reserved code 1006 on the wire, `PAUSE` actually pauses, and
basic-auth comparison is constant time — each explained in [PARITY.md](PARITY.md). `--title`,
`--auth-url` and its companions are additions; nothing that existed was removed.

**Windows is not ported.** The C build supports it through ConPTY; this port implements the
Unix PTY path only. A Windows backend fits behind the same `pty` module interface and was
left out rather than shipped untested.

**The Unix path is exercised on Linux and macOS**, with the suite green on both. The BSDs are
untested.

## Versioning

This port continues the C project's version line instead of restarting at `0.1.0`. It is the
same program to anyone using it — same options, same wire protocol, same frontend bundle — so
a fresh numbering would tell an operator nothing useful and would lose the fact that this
follows 1.7.7.

**It starts at 2.0.0.** Not 1.8.0: a reimplementation in a different language is the largest
change a user can be handed even when every observable behaviour is preserved, and the
platform support genuinely narrowed, since the C build runs on Windows and this one does not.
Under semver, dropping a supported platform is breaking on its own.

`--version` therefore reports `2.0.1-<short git hash>` where the C build reports
`1.7.7-<short git hash>`. The format is deliberately identical and the number deliberately
is not: a binary found in the wild should never be ambiguous about which implementation it is.
Version numbers do not travel between the two builds, and nothing in the test suite compares
them — the differential tests compare *behaviour*, which is the thing that is supposed to
match.

Releases are tagged with the bare version, matching how the C project tags its own.
