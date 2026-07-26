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

The only external requirement is a Rust toolchain — no libwebsockets, libuv or json-c.

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
- A non-2xx answer is relayed to the browser along with `WWW-Authenticate`,
  `Proxy-Authenticate`, `Location`, `Set-Cookie` and `Cache-Control`. A `302` to your login
  page therefore works end to end.
- If the endpoint cannot be reached the request is refused with `500`. An auth outage never
  admits anyone.
- Only successful decisions are cached. Caching a denial would keep a user locked out for
  the rest of the TTL after they had logged in.
- The cache key includes every forwarded header value, so one user's verdict is never
  reused for another.
- `--auth-url` takes precedence over `--credential` and `--auth-header`, and the `/token`
  endpoint stops handing out a credential.

## Testing

The port is validated against the C build with a differential characterization suite. See
[PARITY.md](PARITY.md) for the method, the results, and the behavioural differences it
found.

```sh
cargo test                                  # everything, against this build
./run-parity-tests.sh /path/to/c/ttyd       # and again against the C build, compared
```

## Compatibility

Every command-line option, HTTP endpoint, WebSocket message type and authentication mode of
the C implementation is supported. Three behaviours differ on purpose — a clean exit now
reaches the browser as WebSocket close code 1000, `PAUSE` actually pauses, and basic-auth
comparison is constant time — each explained in [PARITY.md](PARITY.md).

**Windows is not ported.** The C build supports it through ConPTY; this port implements the
Unix PTY path only. A Windows backend fits behind the same `pty` module interface and was
left out rather than shipped untested.
