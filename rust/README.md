# ttyd — Rust port

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

```
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

## Compatibility

Every command-line option, HTTP endpoint, WebSocket message type and authentication mode of
the C implementation is supported. Three behaviours differ on purpose — a clean exit now
reaches the browser as WebSocket close code 1000, `PAUSE` actually pauses, and basic-auth
comparison is constant time — each explained in [PARITY.md](PARITY.md). `--title`,
`--auth-url` and its companions are additions; nothing that existed was removed.

**Windows is not ported.** The C build supports it through ConPTY; this port implements the
Unix PTY path only. A Windows backend fits behind the same `pty` module interface and was
left out rather than shipped untested.
