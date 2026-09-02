# Parity report: C implementation vs. Rust port

[繁體中文](PARITY.zh-TW.md) | **English**

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

Coverage is reported against three explicitly different denominators, because a single
number invites a comparison the method does not support. The first two rows are the
like-for-like pair: the same 108 shared tests, run against each build.

| Measured | Scope | Line coverage |
|---|---|---|
| C reference (`src/*.c`, 966 lines) | the 108 shared parity tests | **88.72 %** |
| Rust port (`rust/src/*.rs`) | the same 108 shared parity tests | **80.58 %** |
| Rust port, excluding `auth.rs` | the same 108 shared parity tests | **86.55 %** |
| Rust port (2778 lines) | the whole suite, including unit and forward-auth | **93.82 %** |

The comparable pair is the first two rows, and it does not favour this port: the shared
suite reaches **less** of the Rust code than of the C code. Most of that gap is
`auth.rs`, which is forward authentication — a feature the C build does not
have, so no shared test can reach it by construction; it is covered instead by the 19
`forward_auth` tests, which have nothing to run against. Excluding it closes the gap to
about two points. The rest is that this port carries close to three times the code of the
original by the same counting — 2778 instrumented lines against 966 — some of it error
handling with no C equivalent. By raw line count the ratio is smaller, 4631 against 1965.

The last row is the number to use when asking "how much of this port is tested at all",
and the one that should *not* be set beside the C figure.

These four figures were measured at `bfa0d05`, where the suite stood at 221 tests and the
port at 4409 lines; it has since gained one unit test and grown to 4631. The C figure is
unaffected, because `src/*.c` has not changed since the fork. The inventory below is the
current count, not the one behind the percentages.

Test inventory:

| Suite | Tests | Runs against C |
|---|---|---|
| Unit tests (`cargo test --lib`) | 95 | no — internal APIs |
| `cli_parity` | 18 | yes |
| `http_parity` | 21 | yes |
| `ws_parity` | 37 | yes |
| `tls_parity` | 4 | yes |
| `lifecycle_parity` | 28 | yes |
| `forward_auth` | 19 | no — new feature |

95 of the 108 shared tests assert identical behaviour on both binaries. The remaining thirteen
are the documented divergences below plus the tests covering behaviour the C build does not
have: forward authentication's startup validation (two tests) and its `--help` entry,
`--title`, client-certificate
verification, an explicit gzip refusal, the `-f` send-buffer ceiling, credential redaction in
the log, and base-path normalization. Eleven of the thirteen return early against C; two run
on both with a per-build assertion (identities longer than its 29-byte buffer, and the close
code a failing exit produces).

What the remaining ~12 % of uncovered C lines consists of, checked line by line:
allocation and `lws_write` failure branches, `inflate` failure handling, `fork`/`execvp`
failure paths, the `SIGABRT` handler, the partial-HTTP-write path that needs an
artificially slow client, and code made unreachable by the C defects described below.
None of it is behaviour a black-box test can drive without fault injection.

One caveat on the measurement: the `-u`/`-g` privilege-dropping test does exercise those
option branches, but the instrumented binary cannot write its profile data after dropping
to `nobody`, so that run contributes nothing to the coverage number.

## Divergences found

Four real differences are documented below. The suite surfaced three of them by running
against both binaries. §2 it missed, and the two faults behind that are worth naming
separately. `a_clean_exit_closes_with_code_1000`, named for the claim §2 used to make,
skipped the C build from the commit that introduced it, so that claim was never once checked
against C. `a_failing_exit_does_not_close_with_1000`, which covers the path where the two
builds really do differ, ran against both but asserted only `!= 1000`, which every ending a
failing exit produces satisfies. The two hid different things, and the weak assertion alone was enough to keep the
real difference out of the record until it was found by hand. Each difference is resolved for
the reasons given below. The first was this port's own defect and was corrected to match C;
the other three are C defects left as they are there and fixed here, so they remain places
where the two builds differ.

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

### 2. A failing exit puts a reserved close code on the wire

**Found by:** hand, on the wire, 2026-09-02. Two separate failures kept the suite from
finding it. `ws_parity::a_clean_exit_closes_with_code_1000` carried
`if is_c_reference() { return; }` from the commit that introduced it, so the claim this
section used to make had never once run against C. And the test that does cover the failing
path, `a_failing_exit_does_not_close_with_1000`, always ran against both builds but asserted
only `!= 1000`, which every ending below satisfies. A skipped test and a weak assertion are
different faults and they hid different things: the skip kept the old claim from ever being
checked, the weak assertion kept the real difference from being recorded. The second would
have been enough on its own.

**This section previously recorded the opposite of what it now records**, and the correction
is worth more than the defect. It claimed that a clean exit never reaches the browser as
1000, reasoning that `callback_tty` calls `lws_close_reason(wsi, 1000)` and then returns `1`
from the writable callback, which was read as dropping the connection before the handshake
could complete. That reading is wrong: returning non-zero from a callback is the documented
way to ask libwebsockets to close, and it sends the reason set beforehand. The two lines are
a pair, not a contradiction. Measured on the wire against the pinned `1.7.7-40e79c7` release
binary (libwebsockets 4.3.3) and against a homebrew 1.7.7 (libwebsockets 4.5.8), a clean exit
closes with **1000** followed by a FIN on both. With the guard removed, the test agrees.

What actually differs is the failing path:

| child ends by | C 1.7.7 | this port |
|---|---|---|
| `exit 0` | close frame, code 1000 | close frame, code 1000 |
| non-zero exit, or a signal | close frame, code **1006** | **no close frame**, socket dropped |

C takes `process->exit_code == 0 ? 1000 : 1006` straight into the frame. RFC 6455 §7.4.1
reserves 1006 for an endpoint to report an abnormal closure locally and states it MUST NOT be
set as a status code in a Close control frame. A strict client treats the frame as a protocol
violation: tungstenite turns it into `Close(1002)`, and a browser fires `error` before `close`.

**Resolution: the port sends 1000 for a clean exit and no close frame otherwise**, which is
what an abnormal closure is defined to look like.

The consequence is not symmetric, and this repository's own frontend is where it shows.
`html/src/components/terminal/xterm/index.ts` disables reconnection when the socket fires
`error`, and of these two teardowns only the invalid frame does. Against C the violation
therefore acts as a brake; against this port the browser sees a silent drop, keeps `doReconnect` true, and
reconnects immediately, with no delay and no bound. Nobody designed that brake, and
correcting the protocol removed it.

For most of this section's life nothing in the suite held it:
`a_failing_exit_does_not_close_with_1000` asserted only `!= 1000`, which both `Close(1002)`
and an abnormal drop satisfy. It now asserts the ending each build produces, so what this
section records is checked on every run instead of resting on the measurement above.

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

### 4. `--socket-owner` without a group is silently ignored

**Found by:** `lifecycle_parity::a_socket_owner_without_a_group_sets_only_the_user`, while
adding coverage for the short form of the option.

`-U daemon:daemon` works in the C build: the socket ends up `srw-rw---- 1 1`. `-U daemon`,
with the group half left off, produces `srwxr-xr-x 0 0` — libwebsockets fails to parse the
string and then abandons the whole permission step, so neither the `chown` nor the
unconditional `chmod 0660` happens. The socket is left at the process umask.

An operator who writes `-U ttyd` instead of `-U ttyd:ttyd` therefore gets a server that
starts normally, logs nothing unusual, and hands out none of the access that was asked for:
the named user does not own the socket, and the group cannot reach it. How exposed the
result is depends on the umask — see the note below — but in every case the option silently
did nothing.

**Resolution: the port applies the user half and still enforces the mode.** A missing group
means "do not change the group", not "do nothing". The test is skipped against the C
reference, with the reason recorded at the assertion.

## Other observations about the C build

Recorded here because they came out of the differential runs, not because the port
changes them:

- **Output from a very short-lived process can be lost.** With `ttyd -a -W echo`, the C
  build delivered the output on roughly three runs out of five; the abrupt teardown races
  with the last frame. The mechanism recorded here used to be "a TCP reset discards whatever
  the client had buffered", which came from the same misreading as §2 and is wrong: measured
  on the wire, C ends with a close frame followed by a FIN, not a reset. What the last frame
  actually races against has not been established. Two tests were rewritten to keep the child
  alive briefly so they measure argument passing rather than shutdown timing.

## Deliberate improvements

Beyond the four divergences above, the port makes these changes on purpose:

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
- **An explicit `gzip;q=0` is honoured.** Both builds decide whether to compress the index
  by asking whether `Accept-Encoding` *contains* `gzip` (`strstr` in `http.c`). That answers
  yes to `gzip;q=0`, which is how RFC 9110 spells "do not send me gzip" — the client then
  receives a compressed body it has just said it cannot decode, and a browser renders the raw
  deflate stream. This port parses the header into tokens and honours a zero weight. Measured
  across ten header forms against both builds; the divergence is confined to `gzip;q=0` (now
  uncompressed) and `GZIP` (now compressed, since RFC 9110 makes coding names
  case-insensitive). `*` is deliberately still *not* treated as accepting gzip, matching C —
  an uncompressed body is acceptable to every client, so there is nothing to gain by
  differing there.
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
- **`begin_shutdown` could silently drop the signal, leaving SIGTERM ignored.** It used
  `watch::Sender::send` and discarded the error — but that method returns an error *and
  leaves the value unchanged* when no receiver is subscribed at that instant. Since
  `wait_for_shutdown` subscribes per call, there is a window (the accept loop inside a branch
  body, no live sessions) in which a signal would vanish and the server would keep running.
  `send_replace` records the value unconditionally. Found while removing an `accepting` flag
  that nothing read: retargeting its test at the mechanism the accept loop *actually* selects
  on turned a test that passed while proving nothing into one that failed for a real reason.
- **A `force_exit` flag was carried over but never read.** The C build checks it in two
  places — a second signal escalates to an immediate exit, and a finished child ends the
  process. Both behaviours exist in this port, implemented by other means (a `select!` on a
  second signal, and an explicit wait), so setting the flag did nothing. Vestigial state that
  mirrors a real mechanism is worse than no state at all: it reads as though the behaviour
  hangs off it. Removed.
- **An oversized `--srv-buf-size` killed the server on the first connection.** The value is
  allocated once per session, and nothing bounded it: `-f 9999999999999` started cleanly and
  then died the moment a client connected. The C build survives the same argument (its RSS
  grew to 1.29 GB but it kept serving), so this was a denial of service by typo *and* a
  regression against the original. The value is now clamped to 16 MiB, with the clamp
  reported rather than applied silently.
- **`--auth-url` and `--auth-method` were accepted unvalidated.** Forward auth fails closed,
  so a typo in the URL started cleanly and then answered every request `500` — a typo turned
  into a total outage at first traffic instead of a startup error. Both are now checked while
  parsing.
- **A query string could panic the connection task.** `decode_query_value` read `%XX`
  escapes by slicing the `&str` at byte offsets, which are not character boundaries: `?arg=%aé`
  slices into the middle of `é` and panics. The query string comes straight off the wire, so
  any client could kill its own connection task at will with `--url-arg` enabled — confirmed
  against a running server, which logged `panicked at src/ws.rs:478`. Escapes are now decoded
  from the byte slice.
- **`-6` was ignored whenever `-i <name>` named an interface with an IPv4 address.** The v6
  branch fell through into the v4 branch, and a v4 address is usually enumerated first, so
  the common case silently bound IPv4. It now skips non-v6 entries and reports an error when
  the interface has no IPv6 address at all.
- **A failed `accept` spun the loop at full CPU.** Both accept loops discarded the error and
  continued immediately. `EMFILE`/`ENFILE` from descriptor exhaustion is not transient, so
  `accept` fails again at once — the loop busy-spins a core and starves the runtime that
  would otherwise be closing the connections which free those descriptors. Failures are now
  logged and backed off.
- **The UNIX socket carried the process umask between `bind` and the `chmod`.** Binding now
  happens under a umask that already denies everyone else, so the mode never depends on how
  quickly the `chmod` lands.
- **The PTY master was marked close-on-exec after the child had been spawned.** Between
  `openpty` and that point, any other session starting concurrently would fork with the
  descriptor open and inherit it — one terminal readable and writable from another session's
  child. Both ends are now marked before anything can fork; the slave still reaches the child
  because `dup2` clears the flag on the copy.
- **A forwarded header could be reintroduced by the operator.** Listing `x-forwarded-for` in
  `--auth-request-header` — a natural thing to do, not knowing it is synthesized — sent the
  client's value *and* the observed peer, client copy first, defeating the guarantee that only
  the observed address is forwarded. Client copies that collide with the synthesized set are
  now dropped.
- **The exit receiver could be polled after completion.** The branch was guarded on
  `exit_info.is_none()`, but a dropped sender yields `Err` and leaves `exit_info` empty, so
  the guard stayed true — and `oneshot::Receiver` is not fused, so the next poll panics. It is
  now guarded on whether the receiver has completed at all.
- **A fatal error was invisible at `-d 0`.** The startup failure path used `tracing::error!`,
  but `-d 0` installs no subscriber, so the process exited 1 having said nothing. It writes to
  stderr directly.
- **`--check-origin` accepted origins the C build refuses, and refused ones it accepts.**
  `check_host_origin` drops `:80` and `:443` from the origin whatever the scheme, and matches
  the scheme itself exactly and case-sensitively. This port dropped only the scheme's own
  default port — so `https://host:80` against `Host: host` was rejected where C admits it —
  and lower-cased the scheme before matching, so `HTTP://` and even `ftp://` were admitted
  where C turns them away. The second half is the one that matters: being *more* permissive
  than the reference on a security control is the wrong direction to differ in. Both halves
  are now pinned by tests that run against both binaries, across the 17 origin forms that
  were compared by hand.
- **The HTTPS redirect could emit a URL no client can follow.** A request with no usable
  authority — HTTP/1.0, which has no required `Host` — produced `Location: https:///token`.
  The authority now comes from `Host`, falling back to the URI's own authority for HTTP/2 and
  absolute-form requests, and a request with neither is answered `400 Bad Request` instead.
  (The C build drops the connection without answering at all.)
- **Terminal input was queued without limit.** A child that is slow to read, or not reading
  at all, fills the kernel PTY buffer and stops the write completing; everything the client
  kept sending piled up in the server's memory. Measured: 15 MiB of input aimed at a
  non-reading child grew RSS by 6.5 MB with nothing to stop it, while the C build absorbed
  179 MiB for under 1 MB of growth because libwebsockets applies read flow control. The
  session now stops reading its socket once 4 MiB is outstanding, so the backlog is bounded
  by TCP backpressure rather than by memory. Gating the read rather than waiting inside it is
  deliberate — the session drives both directions from one `select!`, so waiting there would
  also stop draining the child's output, and a child that both reads and writes would wedge
  against itself. That is not hypothetical: the first version of this fix blocked, and the
  test using `cat` deadlocked in exactly that way.
- **The UNIX domain socket was left at the process umask.** libwebsockets `chmod`s it to
  `0660` right after binding; this port did not, and also skipped the `chown` to `-u`/`-g`.
  Found by auditing `serve.rs` line by line against `server.c` and `strace`-ing the two
  binaries side by side, not by any test — the existing socket test asserted `uid == 0` while
  running as root, which is true whether or not anything happened. It now asserts the mode and
  chowns to a user that is not the test's own.
- **A test guarding the input backlog passed only on the host it was written on.** It wrote
  straight past the gate `ws::session` applies and asserted the ceiling was reached within
  8 MiB — which measures how much a host's line discipline swallows before it stops accepting,
  not the bound itself. It went red at `a827e3d`, where the PTY moved from a blocking writer
  thread to readiness-driven I/O, and stayed red across the eleven commits that followed,
  five weeks, this port's merge included, without being noticed. A red test nobody reads is worse than a missing one: it still appears
  in the inventory. The bound itself was never broken: modelling the gate shows the backlog
  peaking at exactly 4 MiB on Linux. The first rewrite replaced the 8 MiB premise with
  `peak <= MAX_QUEUED_INPUT + chunk`, and that rewrite was itself vacuous on macOS, which
  review caught: with `input_backlog_is_full()` hard-wired to `false`, so the bound was gone
  entirely, it still passed there, because a canonical-mode line discipline discards the
  excess rather than refusing the write and nothing ever queues. That is the same defect the
  rewrite was fixing. The child now puts its terminal into raw mode before it stops reading,
  so both kernels apply backpressure and the gate engages on both. The negative control fails
  on both as well: with the bound removed, the test reports writing 64 MiB without the backlog
  ever filling. It asserts two things now rather than one weaker invariant: that the gate is
  actually reached, and that a caller which stops there never queues more than one chunk past
  the ceiling.
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

**The Unix path is exercised on Linux and macOS**, and the suite is green on both: 222 tests
on an arm64 macOS 15.5 host and in a `rust:1.92-slim` container. Getting there took two
compilation fixes and four test fixes, none of which touched behaviour. `initgroups` takes its
base group as `gid_t` on Linux and as `int` on Apple platforms, and `setgroups` takes its count
as `size_t` and `int` respectively, so the port did not compile on macOS at all. Four tests
carried assumptions about their host rather than about the code: the loopback device is `lo` on
Linux and `lo0` elsewhere (two tests); the system opener `--browser` invokes is `xdg-open` or
`open`; the stale-socket test waited for its path to exist, which was already true of the
leftover file it had just written; and the `-6` fallthrough test named `lo` as an interface
without an IPv6 address, which is false wherever loopback carries `::1` — it had been failing
on Linux too, for that reason rather than for the fallthrough it guards.

Two caveats on that green, because a skip and a pass look alike. The `-6` fallthrough test now
looks for an interface with IPv4 and no IPv6 and skips when the host has none, which on a
machine that gives every interface a link-local `fe80::` is most of the time; it prints the
skip rather than passing quietly. And the suite is run by hand: the workflows under
`.github/` build the C project, and nothing in CI runs `cargo test`. The BSDs are untested.

Everything else in the C feature matrix is implemented and covered: all 30 command-line
options, the four HTTP endpoints, all eight WebSocket message types, all three
authentication modes, TLS with client-certificate verification, UNIX domain sockets,
privilege dropping and the `--once` / `--exit-no-conn` lifecycle rules. Each option has at
least one test asserting an observable effect, so "implemented" here means exercised rather
than merely parsed.

One option carries a caveat, stated because the sentence above would otherwise overstate
what was checked. `-6` has a unit test pinning the address it selects, and an integration
test that serves a real request over `[::1]` — but the container this was validated in has
no IPv6 stack (`bind` returns `Address family not supported by protocol`), so that
integration test skipped rather than ran here. The option is proved end to end only on a
host with IPv6 enabled.

## Browser verification

The rest of the suite talks to the protocol with a synthetic client, which proves the wire
format but not that the shipped frontend works against it. `browser-check.py` drives a real
Chromium through the real xterm.js bundle with Playwright and checks, on both builds:

- the frontend loads and xterm.js mounts
- typed keystrokes reach the shell (verified by files the shell creates — xterm.js renders
  to a WebGL canvas, so terminal text is not readable from the DOM)
- `TERM` reaches the child, and a viewport resize reaches its `winsize`
- colour, CJK and box-drawing render (screenshot)
- a full-screen program (`vi`) round-trips through the alternate screen
- no uncaught frontend errors, and the session never drops
- with `TTYD_BROWSER_TLS=1`: the same run over HTTPS against a generated CA, asserting
  `location.protocol === "https:"`

Two things came out of this that the protocol-level suite could not have found: `--title`
had to be checked against a real browser tab, and `networkidle` never fires because the
WebSocket keeps the page busy.

It also produced a false accusation worth recording. `vi` failed three times running
against the C build and passed against Rust, which looked like a C defect. Comparing the
raw protocol output showed both builds emitting byte-identical 3370-byte responses
containing `?1049h` — the real cause was a `.swp` file the *previous* run had left behind,
which made vim prompt instead of opening. The harness had cross-run state; the fix was a
fresh directory per run.

## Performance

Both builds are Release: the C build at `-O3 -DNDEBUG`, this one with LTO, one codegen unit
and symbols stripped. Runs are interleaved — C, Rust, C, Rust — rather than grouped, so
machine drift lands on both instead of on whichever went second. Each figure is the median of
five rounds on an otherwise idle 4-core machine, and the full per-round range is shown for
every row, because a difference smaller than the spread is not a difference.

The harness is `bench.py`, committed alongside this file so the table can be re-run rather
than taken on trust.

| Measurement | C (median) | C range | Rust (median) | Rust range | Verdict |
|---|---|---|---|---|---|
| startup to listening (ms) | 4.3 | 4.0–5.6 | **2.7** | 2.6–2.9 | Rust, ranges disjoint |
| baseline RSS (kB) | **5108** | 5068–5116 | 5256 | 5196–5280 | C, but by 3 % |
| **RSS per idle session (kB)** | **17.3** | 17.3–17.4 | 82.7 | 80.3–83.2 | **C, 4.8×** |
| HTTP `/token` (req/s) | 4582 | 3886–4786 | 4862 | 4402–5147 | ranges overlap — no call |
| terminal sessions (open+close/s) | 163 | 152–173 | **195** | 179–206 | Rust, ranges disjoint |
| terminal output (MB/s) | 76.7 | 47.9–83.6 | **92.3** | 85.8–92.8 | Rust, ranges disjoint |
| CPU per MB delivered (ms) | 8.6 | 8.4–9.2 | 8.5 | 8.1–9.0 | ranges overlap — equal |

Two rows deliberately draw no conclusion. The request-rate figure is produced by a Python
client that is itself the bottleneck, and its ranges overlap besides; CPU per byte is a real
measurement read from `/proc`, and it is genuinely a tie. Only the four rows with disjoint
ranges support a claim.

**Read the magnitudes as a floor, not as the machine's best.** A full round costs both builds
throughput, and it compounds: run `session_rate` on its own, three times in a row, and it holds
steady at 376–416/s for C and 762–870/s for this port; run it as step five of seven in a
five-round sweep and the medians fall to 163 and 195. Both builds are depressed together — the
ratio moves much less than the absolute figures — which is exactly what interleaving is for,
and it is why every row here carries its spread. What accumulates has not been identified: it
is not leaked shells (`ps` shows none after 500 sessions on either build) and not lingering
`TIME_WAIT` sockets (five, at rest). It is listed here as an open question rather than left for
a reader to trip over.

Absolute numbers are therefore not comparable with an earlier revision of this table, which in
addition was measured with a harness that was quietly loading the machine (below).

An earlier revision of this table read very differently: throughput was within the noise and
the C build used 23 % less CPU per byte delivered. That was measured before the PTY stopped
using three OS threads per session — a reader, a writer and a reaper. Removing them (see the
entry below) did not only reduce memory; it moved throughput and CPU efficiency from "C is
ahead" to "Rust is ahead or equal", because three threads per session multiplied by the
client count was scheduler pressure rather than useful work.

### Why the throughput rows go the way they do

"Rust is faster" is not a result until the mechanism is known, so the two disjoint rows were
traced rather than assumed. Both explanations turned out to be about *idle time*, not about
Rust generating better code.

**Terminal output (92.3 vs 76.7 MB/s).** Per-byte cost is a tie, and CPU utilisation is what
differs. On the same 4-core box, delivering the same generated stream:

| | throughput | server CPU | %CPU per MB/s |
|---|---|---|---|
| C | 78.3 MB/s | 79.5 %, all on one thread | 1.02 |
| Rust | 93.4 MB/s | 89.7 %, spread over four workers at ≤30 % each | 0.96 |

The revealing number is C's **79 %**. Its single event-loop thread is not saturated — it is
idle a fifth of the time — so nothing about the client or the generator is capping it. The
idle is structural. `read_cb` in `src/pty.c` calls `uv_read_stop` on its first line, parks the
chunk in `pss->pty_buf`, and only calls `pty_resume` after `lws_write` has returned inside the
writable callback. Exactly one chunk is ever in flight: while the socket write happens, the
PTY is not being read, and vice versa. `strace -c` confirms the shape — C spends **2.0
`epoll_pwait` per PTY read** (506 vs 253 per MB), i.e. two full event-loop turns per chunk.

This port reads the PTY in its own task feeding a depth-1 `mpsc`, so chunk *N+1* is being read
while chunk *N* is being framed and written. Same trace: **1.15 `epoll_wait` per read** (294 vs
256 per MB). The overlap is the entire win. It is not free — the cross-thread handoff shows up
as 228 `futex` per MB, which is precisely why per-byte CPU comes out a tie instead of a win.

An earlier guess, recorded because it was wrong: `uv_read_stop`/`uv_read_start` per chunk
looked like it should cost two `epoll_ctl` syscalls each time. The trace shows **zero**
`epoll_ctl` on either build — libuv coalesces the stop and the start in its watcher queue
before the next loop iteration. The cost of the ping-pong is the extra loop turn, not the
re-arming.

**Session churn (195 vs 163 open+close/s).** Measured directly: idle → ten concurrent
sessions, C goes from 2 threads to 12, this port stays at 5. C creates a real pthread per
session for a blocking `waitpid` (`uv_thread_create(&process->tid, wait_cb, process)` in
`pty_spawn`) and `uv_thread_join`s it in `process_free`. So every session pays a `clone`, a
stack mapping and a join handshake. This port spawns three tokio tasks instead. Note the
symmetry with the defect this port shipped and fixed: the first version used *three* OS
threads per session and was slower than C on both rows. Reducing C's one to zero is the same
lever, applied one notch further.

### Two ways this table was wrong before it was right

Both are recorded because both produced numbers that looked entirely plausible.

**The harness was the load.** `bench.py` terminated each server with SIGTERM and escalated to
SIGKILL after ten seconds. Both builds reap the terminal's process tree on SIGTERM — measured,
in about 0.01 s — but a SIGKILLed server never runs that path, and `-W sh -c 'while true; …'`
leaves a `dd | tr` loop spinning with no parent to kill it. Each orphan added load, load slowed
the next shutdown, a slower shutdown hit the escalation. A five-round run finished with
**twenty-six generators still running and a load average of 33 on a four-core box**, and its
output degraded across rounds in a way that reads as a result: C's terminal-output row spread
from 12.2 to 71.5 MB/s and its session rate halved.

Signalling the server's process group does not fix this, which is worth stating because it is
the obvious first attempt. ttyd gives the terminal its own session — `setsid` plus `TIOCSCTTY`,
exactly right for a terminal server — so the shell is not in the server's group and no signal
aimed at that group reaches it. The harness now sweeps for surviving generators after every
server, waits three seconds first so a still-exiting process is not miscounted as a leak,
refuses to start on a machine that already has some, and prints how many it had to reap.

One claim is deliberately *not* made: with the sweep in place, the SIGKILL escalation stops
firing entirely, and orphans still appear — around four per round, only after the machine has
been working, never in an isolated run of the same two measurements. So the escalation is one
way to produce them and not the only one, and the second path is unidentified. What the table
can say is that the run behind it swept its own strays and ended with none outstanding.

**A benchmark was run against a busy machine.** The first version of this table was taken
while an eight-minute browser soak ran alongside it. Those numbers happened to survive a clean
re-run, but they should not have been published.

The shape of both mistakes is the same: a measurement that silently includes the measurer.
That is why the harness now asserts rather than assumes, and why it is in the repository.

**C still wins on memory per session, but the gap is now 4.8×, not 12×.** The dominant cost
was found and removed.

Decomposing per-connection RSS before and after:

| stage | before | after |
|---|---|---|
| plain HTTP connection | 64 kB | 64 kB |
| WebSocket upgraded, no terminal opened | 187 kB | 62 kB |
| full terminal session | 219 kB | 104 kB |

The 125 kB the WebSocket upgrade added was a single 128 KiB allocation. A `gdb` catchpoint on
`mmap`, against a debug-symbol build, traced it to `tungstenite::FrameCodec::new` calling
`BytesMut::with_capacity` — the **read** buffer, whose `read_buffer_size` defaults to 128 KiB
and is allocated eagerly the moment a socket upgrades. (An earlier attempt sized the *write*
buffer down and saw no change; the write buffer is lazy, which is why. The two are separate
knobs and only the read one is eager.) The read buffer carries client-to-server traffic only —
keystrokes, resize messages, the opening frame — so it is sized to 16 KiB, and a larger paste
still works because `BytesMut` grows on demand, verified by echoing a 1 MB single frame back
through `cat` on both builds.

This decomposition and the table's 82.7 kB/session are measuring different things and do not
have to agree: the numbers above are one connection's own mappings, read from `smaps`, while
the table divides the resident growth across twenty-five simultaneous sessions. The marginal
session is cheaper than the first because the allocator reuses arenas it has already faulted
in, which is why the amortised figure lands below the single-connection one.

What remains is the ~64 kB every connection costs before it is even a WebSocket — hyper's own
per-connection buffers. That is a smaller target, it is load-bearing for HTTP throughput, and
it was left alone rather than chased on a hypothesis. The terminal session itself now adds
about 40 kB on top of that.

## Soak

A ten-minute run with eight concurrent clients connecting, streaming and disconnecting in
a loop, sampling the server every 30 seconds:

| | start (t=60s) | end (t=600s) |
|---|---|---|
| RSS | 10 180 kB | 10 692 kB |
| open descriptors | 42 | 42 |
| threads | 29 | 29 |

792 sessions, 11.2 GB of terminal output, no errors. The RSS figure oscillated between
9.9 MB and 11.1 MB throughout with no trend; descriptors and threads did not move at all.

A second run targets the input path rather than the output path: a client floods terminal
input at a child that never reads, is disconnected or blocked, reconnects, and repeats. Over
20 such rounds RSS oscillated between 12.2 MB and 13.1 MB and settled at 12.6 MB — the
backlog ceiling holds per session and is reclaimed when the session ends, rather than
ratcheting up across reconnects.

The first attempt at the ten-minute measurement reported the server freezing after three
minutes.
That was the harness: it read the server's stderr only until the port line appeared, so
the log filled the 64 kB pipe buffer and the server blocked in `write()`. Recorded because
the failure looked exactly like a server defect until the stack was inspected — the same
class of mistake as the vi false alarm above.

## Declined, with reasons

Not everything raised in review is a defect to fix. These were checked against the C build
and left alone, because changing them would be a divergence rather than a correction:

- **`--browser` waits for the launched process.** `open_uri` uses a blocking `status()`, so a
  handler that stays in the foreground delays the accept loop. The C build does exactly the
  same thing — `fork` followed by `waitpid` in `utils.c` — so this is parity, not a
  regression, and `xdg-open` detaches in practice.
- **The display probe only detects X11.** `xset -q` is missing on a Wayland session without
  Xwayland, so `--browser` quietly does nothing there. Again, the C build runs the identical
  probe (`system("xset -q > /dev/null 2>&1")`). Worth fixing upstream in both, not worth
  diverging here.

## A correction: what a UNIX socket's mode actually controls

Earlier revisions of this document, and of the comments in `serve.rs`, described a socket at
mode `0755` as "world-connectable" and said any local user could open a terminal through it.
**That is wrong**, and it was repeated in several places before review caught it.

Connecting to a UNIX domain socket requires **write** permission on the socket file.
Measured directly, connecting as an unprivileged user:

| mode | another user can connect |
|---|---|
| `0755` | no — `EACCES` |
| `0775` | no — `EACCES` |
| `0777` | **yes** |
| `0660` | no — `EACCES` |
| `0666` | **yes** |

So under the usual `0022` umask the un-`chmod`ed socket was `0755` and *already* closed to
other users; `0755` is in fact stricter for connecting than the `0660` the C build sets,
which deliberately opens it to the group. The reasons to match `0660` are parity, the group
access `--socket-owner` exists to grant, and the guarantee that the mode does not depend on
the umask the server happened to inherit — a process started with `umask 0` binds `0777`,
which genuinely is open to everyone.

The defect was real; the impact statement was overstated. Recorded rather than quietly
edited, because a security claim that turns out to be wrong is worth more as a correction
than as a deletion.

## Dependencies

`cargo audit` against the RustSec database (1169 advisories) reports **no vulnerabilities**
across the 222 crates in `Cargo.lock`. One informational warning: `rustls-pemfile` 2.2.0 is
marked unmaintained (RUSTSEC-2025-0134). It is used only to parse the PEM files named by
`--ssl-cert`, `--ssl-key` and `--ssl-ca` — operator-supplied local files, not network input.

Trivy was not run: this environment's proxy scopes GitHub access to the session's own
repositories, so the installer, the release API and the apt repository are all unreachable.
That is a gap in this report, not a clean result.
