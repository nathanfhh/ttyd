# Parity report: C implementation vs. Rust port

This document records how the Rust port was validated against the original C
implementation, what the validation found, and where the two deliberately differ.

## Method

The port was verified with **characterization testing** (also called golden-master
testing): rather than asserting what the code *should* do, the suite asserts what the
existing implementation *actually* does, and the rewrite must reproduce it.

That is combined with **differential testing**: the same suite runs against both
binaries, selected by the `TTYD_BIN` environment variable. A test that passes for one
and fails for the other is a behavioural divergence that has to be explained — either
the port is wrong, or the original has a defect worth fixing on purpose.

```sh
./run-parity-tests.sh /path/to/c/ttyd
```

Neither technique proves equivalence. To bound what the suite misses, the C build was
also compiled with `--coverage` and the suite run against it: any C line the suite never
reaches is behaviour nothing is checking. That number is reported below, and the gaps it
exposed were used to write more tests until only unreachable error paths remained.

## Results

| | Tests | Line coverage |
|---|---|---|
| C reference (`src/*.c`, 966 lines) | 84 run against it, all passing | **87.89 %** |
| Rust port (`rust/src/*.rs`, 2454 lines) | 177 total, all passing | **91.32 %** |

Test inventory:

| Suite | Tests | Runs against C |
|---|---|---|
| Unit tests (`cargo test --lib`) | 75 | no — internal APIs |
| `cli_parity` | 12 | yes |
| `http_parity` | 18 | yes |
| `ws_parity` | 31 | yes |
| `tls_parity` | 3 | yes |
| `lifecycle_parity` | 20 | yes |
| `forward_auth` | 18 | no — new feature |

77 of the 84 shared tests assert identical behaviour on both binaries. The remaining six
are the documented divergences below plus the tests covering behaviour the C build does not
have (`--title`, base-path normalization, and identities longer than its 29-byte buffer).

What the remaining ~12 % of uncovered C lines consists of, checked line by line:
allocation and `lws_write` failure branches, `inflate` failure handling, `fork`/`execvp`
failure paths, the `SIGABRT` handler, the partial-HTTP-write path that needs an
artificially slow client, and code made unreachable by the two C defects described below.
None of it is behaviour a black-box test can drive without fault injection.

One caveat on the measurement: the `-u`/`-g` privilege-dropping test does exercise those
option branches, but the instrumented binary cannot write its profile data after dropping
to `nobody`, so that run contributes nothing to the coverage number.

## Divergences found

Running the suite against both binaries surfaced three real differences. All three were
resolved in favour of the Rust behaviour, for the reasons given.

### 1. The server used to announce itself before the client did

**Found by:** every `ws_parity` test failing against C at once.

The first draft of the port sent `SET_WINDOW_TITLE` and `SET_PREFERENCES` as soon as the
socket opened. The C implementation sends nothing until the browser has sent its opening
`{"columns":…,"rows":…,"AuthToken":…}` frame, because those messages are written from the
writable callback that `spawn_process` schedules.

**Resolution: the port was corrected to match C.** This is not cosmetic — the window
title contains the full command line, so the original ordering leaked it to any client
that merely opened a socket, before the `AuthToken` had been checked.

Matching C closes the pre-authentication hole but not the underlying exposure: even in the
C build, every client that legitimately opens a session receives the full command line. The
port therefore adds `--title`, which replaces the announced title outright so a command
carrying a script path, a host or a key never reaches the browser. It is distinct from the
existing `-t titleFixed=…` client option, which only changes what the browser displays
after the real title has already been sent over the wire.

### 2. A clean exit never reaches the browser as close code 1000

**Found by:** `ws_parity::a_clean_exit_closes_with_code_1000`.

The frontend decides whether to reconnect with `if (event.code !== 1000)`. The C code
means to cooperate — it calls `lws_close_reason(wsi, 1000)` — but it returns `1` from the
writable callback in the same breath, which makes libwebsockets drop the connection
instead of completing the close handshake. Observed on the wire, a C session always ends
in `ResetWithoutClosingHandshake`, so the browser sees 1006 and offers to reconnect even
after the user typed `exit`.

**Resolution: the port completes the close handshake**, sending 1000 for a clean exit and
dropping the connection without a close frame otherwise — which is what a browser reports
as 1006. The test is skipped when running against the C reference, with the reason
recorded at the assertion.

### 3. `PAUSE` does nothing

**Found by:** `ws_parity::pause_stops_output_and_resume_restarts_it`.

```c
void pty_pause(pty_process *process) {
  if (process->paused) return;      /* paused is always true */
  uv_read_stop(...);                /* never reached */
}
```

`process->paused` is set to `true` once in `pty_spawn` and never assigned again, so
`pty_pause` always returns early and `uv_read_stop` is dead code. Client-driven flow
control has therefore never worked in the C build; a client that cannot keep up has no
way to ask the server to slow down.

**Resolution: the port implements the flow control the protocol describes.** Terminal
output travels through a bounded channel, so a paused or slow client stalls the reader
thread and the kernel PTY buffer applies backpressure to the child process. The test is
skipped against the C reference, with the reason recorded at the assertion.

## Other observations about the C build

Recorded here because they came out of the differential runs, not because the port
changes them:

- **Output from a very short-lived process can be lost.** With `ttyd -a -W echo`, the C
  build delivered the output on roughly three runs out of five; the abrupt teardown races
  with the last frame, and a TCP reset discards whatever the client had buffered. The Rust
  port was stable across the same runs because it closes gracefully. Two tests were
  rewritten to keep the child alive briefly so they measure argument passing rather than
  shutdown timing.

## Deliberate improvements

Beyond the three divergences, the port makes these changes on purpose:

- **Basic auth comparison is constant time** (`subtle::ConstantTimeEq`). The C version uses
  `strcmp`, which returns as soon as it finds a differing byte and so leaks how much of a
  guessed credential was correct.
- **The `Basic` scheme name is matched case-insensitively**, as RFC 7617 requires. The C
  version does `strstr(buf, "Basic ")`, which rejects a spec-compliant `basic`.
- **`-t key=value` keeps everything after the first `=`.** The C version splits again on a
  second `=` and silently drops the remainder, so `-t token=a=b=c` stored `a`.
- **404 responses are byte-identical** to what libwebsockets emits, so anything scraping
  error pages sees no change.
- **Credentials never reach the log.** The C build prints the base64 credential in its
  startup banner (`server.c`) and echoes the presented token when a WebSocket handshake
  fails (`protocol.c`). Base64 is encoding, not encryption — both lines put a reversible
  `user:password` into everything that collects stdout. This port reports that basic auth is
  enabled without the value, and describes a token mismatch by length instead of printing
  it. `lifecycle_parity::the_credential_never_reaches_the_log` pins this down.

- **Integer options reject trailing garbage and octal literals.** The C version parses them
  with `strtol(…, 0)`, which accepts `-p 80abc` as port 80 and reads `-p 010` as 8. This port
  requires the whole value to be a decimal (or `0x`-prefixed) number and exits with the
  standard `invalid value for …` message otherwise.
- **`--base-path` is normalized and validated.** A value with no leading slash is accepted
  and normalized (`mounted` → `/mounted`) rather than reaching the router verbatim, and a
  value containing `{`, `}`, `?` or `#` is rejected — those are route-matching syntax and
  would silently turn the endpoints into wildcard captures.
- **Log timestamps are UTC**, where the C build prints local time. Deliberate: a container
  with no `TZ` set is UTC anyway, and UTC is easier to correlate across hosts.

Two options are added, and nothing that existed was removed: `--title`, above, and
`--auth-url` with its companions, documented in [README.md](README.md).

One option maps onto a different mechanism. `--srv-buf-size` configures libwebsockets'
per-thread service buffer in the C build; there is no equivalent knob in hyper, so this port
applies it to the closest observable thing — the largest amount of terminal output read from
the PTY, and therefore carried in a single WebSocket frame. The default stays 4096, matching
the C default, and `lifecycle_parity::the_send_buffer_size_bounds_one_output_frame` pins the
behaviour down.

## Gaps the coverage run exposed

Measuring coverage of the C build did more than produce a number — it pointed at parts of
the original the suite was not touching, and two of those turned out to be real problems
in the port rather than merely missing tests:

- **`--ping-interval` was parsed and then ignored.** The option had no effect at all, so
  an idle terminal would have been dropped by any reverse proxy with an idle timeout. The
  port now sends WebSocket pings on that interval and hangs up on a peer that has not
  responded for `interval + 7` seconds, matching the C retry policy.
- **SIGTERM left child processes running.** Terminating the server did not tear down live
  sessions, and because each child leads its own process group the kernel did not signal
  them either — every terminal would have survived as an orphan. Shutdown now propagates
  to live sessions, which signal their children before the process exits.

Both are covered by tests that pass against the C build too.

## Defects this port shipped and then fixed

Recorded because the reader deserves to know which parts had to be corrected after the
first implementation landed, and because each one says something about where the testing
approach above was blind.

- **Forward-auth cache could replay a verdict.** The cache key covered the path and the
  operator-listed request headers, but the subrequest also carried the method and the
  `X-Forwarded-*` set — so a grant issued for one request was reused for another the
  endpoint would have refused, without the endpoint being consulted. Differential testing
  could not have caught this: forward auth has no counterpart in the C build to differ
  against. Fixed by deriving the key from the same structure that builds the outgoing
  headers, so the two cannot drift apart.
- **Privilege dropping left supplementary groups in place.** `setgid` does not touch that
  list; libwebsockets calls `initgroups`, and this port did not. A test for `--uid` existed
  and ran against both builds, but asserted only `id -u` — shallow enough to miss it. Fixed
  with `initgroups`/`setgroups`, and the test now asserts the whole group list and installs
  a marker group itself rather than depending on how the machine happens to be configured.
- **`--url-arg` did not percent-decode.** Written against the assumption that
  libwebsockets hands over raw fragments; measuring showed it decodes. The test used
  `first` and `second`, values that look identical either way. Fixed, and the test now uses
  values containing a space and a non-ASCII character.
- **`--srv-buf-size` was parsed and never read**, and the WebSocket access log reported
  every client as `unix` because the handler built a default `ConnInfo` instead of reading
  the one the accept loop recorded. Both fixed, both now covered.
- **Shutdown left terminals running under load.** Terminating the server relied on each
  session task waking up to signal its own child; under parallel load a task could miss the
  window, and since every child leads its own process group nothing else would reap it. The
  server now signals the registered children itself before exiting.
- **The forwarded identity was silently truncated to 29 bytes**, mirroring the C buffer. That
  is worse than it looks: two accounts sharing a 29-byte prefix would collapse onto the same
  `TTYD_USER`. The limit is gone — the name is passed through whole. (The C build refuses the
  WebSocket upgrade outright for such a name, which the suite now asserts on that side.)

## Known gap

**Windows is not ported.** The C implementation supports Windows through ConPTY
(`src/pty.c`, `#ifdef _WIN32`). The Rust port implements the Unix PTY path only, natively,
so that `setsid`, controlling-terminal acquisition, process-group signalling and the
`128 + signal` exit convention match the original exactly. A Windows backend is a
self-contained addition behind the same `pty` module interface; it was left out rather
than shipped untested.

Everything else in the C feature matrix is implemented and covered: all 30 command-line
options, the four HTTP endpoints, all eight WebSocket message types, all three
authentication modes, TLS with client-certificate verification, UNIX domain sockets,
privilege dropping and the `--once` / `--exit-no-conn` lifecycle rules. Each option has at
least one test asserting an observable effect, so "implemented" here means exercised rather
than merely parsed.
