"""Measures the two ttyd builds under identical conditions.

Runs are interleaved (C, Rust, C, Rust, …) rather than grouped, so machine drift — a noisy
neighbour, thermal behaviour, page cache state — lands on both builds instead of on whichever
went second. Every figure is the median of several rounds; the spread is reported alongside so
a difference smaller than the noise is visible as such.

The client is Python, which is slow enough to be the bottleneck for request-rate work. Those
numbers are therefore reported as a floor: "at least this fast", not "this fast".
"""
import base64
import json
import os
import pathlib
import re
import signal
import socket
import statistics
import struct
import subprocess
import sys
import threading
import time

REPO = pathlib.Path(__file__).resolve().parent
C_BIN = os.environ.get("TTYD_C_BIN", str(REPO.parent / "build-c" / "ttyd"))
RUST_BIN = os.environ.get("TTYD_RUST_BIN", str(REPO / "target" / "release" / "ttyd"))
ROUNDS = int(sys.argv[1]) if len(sys.argv) > 1 else 5

# The stream both throughput measurements read. Named once so the leak check below can
# look for exactly the processes this harness starts and nothing else.
GENERATOR = "while true; do dd if=/dev/zero bs=65536 count=256 2>/dev/null | tr '\\0' 'x'; done"
GENERATOR_MARKER = "dd if=/dev/zero bs=65536 count=256"

# Orphans found and killed during the run. Reported at the end: a run that had to clean up
# after itself is still a valid run, but it is not one to describe as having gone smoothly.
ORPHANS_REAPED = 0
# Servers that had to be SIGKILLed because SIGTERM did not finish in time. This is the
# mechanism that produces orphans -- a killed server never reaps the terminal it started --
# so the two counters are reported together and should rise together.
HARD_KILLS = 0


def running_generators():
    """PIDs running this harness's generator, excluding whoever is doing the asking.

    `ps` rather than `pgrep -f`: pgrep matched nothing here for command lines containing the
    pattern, and a cleanup step that silently matches nothing is worse than none at all.
    """
    out = subprocess.run(["ps", "-eo", "pid,ppid,args", "--no-headers"],
                         capture_output=True, text=True)
    pids = []
    for line in out.stdout.splitlines():
        parts = line.split(None, 2)
        if len(parts) < 3 or GENERATOR_MARKER not in parts[2]:
            continue
        if parts[2].startswith("ps ") or "bench.py" in parts[2]:
            continue
        pids.append((int(parts[0]), int(parts[1])))
    return pids


def reap_orphaned_generators(grace=3.0):
    """Kills generators whose server is gone, after giving them a chance to exit on their own.

    The grace period is what separates "leaked" from "still exiting". A generator whose server
    shut down cleanly is reparented to init the instant the server dies, so ppid alone does not
    identify a leak — checked immediately, a perfectly healthy teardown looks exactly like one.
    Measured, a clean teardown clears in about 0.01 s; anything still running after three
    seconds is not on its way out. Counting without waiting reported twenty orphans in a
    five-round run and would have libelled the two builds for a race in the harness's clock.
    """
    global ORPHANS_REAPED
    deadline = time.time() + grace
    while time.time() < deadline:
        if not [p for p, ppid in running_generators() if ppid == 1]:
            return
        time.sleep(0.05)
    for pid, ppid in running_generators():
        if ppid != 1:
            continue
        try:
            os.kill(pid, signal.SIGKILL)
            ORPHANS_REAPED += 1
        except ProcessLookupError:
            pass


class Server:
    def __init__(self, binary, args):
        self.proc = subprocess.Popen(
            [binary, "-p", "0", *args],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
            # Its own process group, so `close` can signal the server *and* everything it
            # spawned as one unit. See the comment there for why that matters.
            start_new_session=True,
        )
        self.port = None
        found = threading.Event()

        def drain():
            for line in self.proc.stderr:
                if self.port is None:
                    m = re.search(r"Listening on port:\s*(\d+)", line)
                    if m:
                        self.port = int(m.group(1))
                        found.set()

        threading.Thread(target=drain, daemon=True).start()
        if not found.wait(20):
            raise RuntimeError(f"{binary} never reported a port")

    def rss_kb(self):
        with open(f"/proc/{self.proc.pid}/status") as f:
            return int(re.search(r"VmRSS:\s+(\d+)", f.read()).group(1))

    def threads(self):
        with open(f"/proc/{self.proc.pid}/status") as f:
            return int(re.search(r"Threads:\s+(\d+)", f.read()).group(1))

    def close(self):
        """Ends the server and everything it started, leaving nothing behind.

        The original version sent SIGTERM and escalated to SIGKILL after ten seconds. Both
        builds handle SIGTERM correctly — measured: the terminal's process tree dies with them,
        in about 0.01 s — but SIGKILL cannot be caught, so the escalation left the load
        generator running instead of letting the server reap it. That is self-reinforcing in
        exactly the wrong direction: an orphan adds load, load makes the next shutdown slower,
        a slower shutdown hits the escalation. A five-round run left twenty-six
        `while true; do dd | tr` loops spinning, drove the load average to 33 on a four-core
        box, and printed numbers that drifted downward as it went — a machine measuring mostly
        itself.

        Signalling the process group does not fix it, which is the part that took a second
        attempt: ttyd gives the terminal its own session (`setsid` plus `TIOCSCTTY`), exactly
        as a terminal server should, so the shell is *not* in the server's group and no signal
        aimed at that group can reach it. Only ttyd can kill it, and a SIGKILLed ttyd never
        gets the chance. Hence the sweep below, which does not depend on how the server died.

        One thing is deliberately *not* claimed here. With the sweep in place the escalation
        stops firing — `HARD_KILLS` stays at zero — and orphans still appear, intermittently
        and only on a machine that has already been working. So the escalation is one way to
        produce them and not the only one; the second path is unidentified. The sweep contains
        the effect either way, and the run reports how often it had to.
        """
        try:
            pgid = os.getpgid(self.proc.pid)
        except ProcessLookupError:
            pgid = None

        global HARD_KILLS
        for sig in (signal.SIGTERM, signal.SIGKILL):
            try:
                if pgid is not None:
                    os.killpg(pgid, sig)
                else:
                    self.proc.send_signal(sig)
            except ProcessLookupError:
                break
            if sig == signal.SIGKILL:
                HARD_KILLS += 1
            try:
                self.proc.wait(timeout=10)
                break
            except subprocess.TimeoutExpired:
                continue
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass

        reap_orphaned_generators()


def mask(payload):
    m = os.urandom(4)
    n = len(payload)
    if n < 126:
        head = b"\x82" + bytes([0x80 | n]) + m
    elif n < 65536:
        head = b"\x82" + bytes([0x80 | 126]) + struct.pack(">H", n) + m
    else:
        head = b"\x82" + bytes([0x80 | 127]) + struct.pack(">Q", n) + m
    return head + bytes(c ^ m[i % 4] for i, c in enumerate(payload))


def ws_open(port, timeout=10):
    s = socket.create_connection(("127.0.0.1", port), timeout)
    key = base64.b64encode(os.urandom(16)).decode()
    s.sendall(
        f"GET /ws HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\n"
        f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n"
        f"Sec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: tty\r\n\r\n".encode()
    )
    buf = b""
    while b"\r\n\r\n" not in buf:
        chunk = s.recv(4096)
        if not chunk:
            raise ConnectionError("closed during handshake")
        buf += chunk
    s.sendall(mask(b'{"columns":120,"rows":40}'))
    return s


# ------------------------------------------------------------------ measurements

def startup_ms(binary):
    """Time from exec to the port being reported."""
    start = time.perf_counter()
    srv = Server(binary, ["bash"])
    elapsed = (time.perf_counter() - start) * 1000
    srv.close()
    return elapsed


def baseline_rss(binary):
    srv = Server(binary, ["bash"])
    time.sleep(1.0)
    rss = srv.rss_kb()
    srv.close()
    return rss


def rss_per_session(binary, sessions=25):
    """Resident growth per idle terminal, which is what --max-clients multiplies."""
    srv = Server(binary, ["-W", "bash"])
    time.sleep(0.8)
    before = srv.rss_kb()
    held = []
    try:
        for _ in range(sessions):
            held.append(ws_open(srv.port))
            time.sleep(0.02)
        time.sleep(1.5)
        after = srv.rss_kb()
    finally:
        for s in held:
            try:
                s.close()
            except Exception:
                pass
        srv.close()
    return (after - before) / sessions


def http_rps(binary, seconds=3.0, workers=4):
    """Requests per second for /token, closed connection each time."""
    srv = Server(binary, ["bash"])
    time.sleep(0.5)
    port = srv.port
    stop = time.time() + seconds
    counts = [0] * workers
    request = f"GET /token HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n".encode()

    def worker(i):
        n = 0
        while time.time() < stop:
            try:
                s = socket.create_connection(("127.0.0.1", port), 5)
                s.sendall(request)
                while s.recv(65536):
                    pass
                s.close()
                n += 1
            except Exception:
                break
        counts[i] = n

    threads = [threading.Thread(target=worker, args=(i,)) for i in range(workers)]
    t0 = time.time()
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    elapsed = time.time() - t0
    srv.close()
    return sum(counts) / elapsed


def session_rate(binary, seconds=3.0, workers=4):
    """Full terminal sessions opened and torn down per second."""
    srv = Server(binary, ["-W", "bash"])
    time.sleep(0.5)
    port = srv.port
    stop = time.time() + seconds
    counts = [0] * workers

    def worker(i):
        n = 0
        while time.time() < stop:
            try:
                s = ws_open(port, 5)
                s.close()
                n += 1
            except Exception:
                break
        counts[i] = n

    threads = [threading.Thread(target=worker, args=(i,)) for i in range(workers)]
    t0 = time.time()
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    elapsed = time.time() - t0
    srv.close()
    return sum(counts) / elapsed


def output_mbs(binary, seconds=4.0):
    """Terminal output delivered to one client, MB/s. The server does the PTY read,
    framing and write; the client only counts bytes."""
    srv = Server(
        binary,
        ["-W", "sh", "-c", GENERATOR],
    )
    time.sleep(0.5)
    s = ws_open(srv.port)
    s.settimeout(5)
    # Let it reach steady state before timing.
    end_warm = time.time() + 1.0
    while time.time() < end_warm:
        s.recv(262144)
    total = 0
    t0 = time.time()
    end = t0 + seconds
    try:
        while time.time() < end:
            data = s.recv(262144)
            if not data:
                break
            total += len(data)
    except Exception:
        pass
    elapsed = time.time() - t0
    s.close()
    srv.close()
    return total / elapsed / 1048576


def cpu_for_output(binary, seconds=4.0):
    """CPU seconds the server burns delivering that stream — efficiency, not just speed."""
    srv = Server(
        binary,
        ["-W", "sh", "-c", GENERATOR],
    )
    time.sleep(0.5)

    def cpu_ticks():
        with open(f"/proc/{srv.proc.pid}/stat") as f:
            parts = f.read().rsplit(") ", 1)[1].split()
        return int(parts[11]) + int(parts[12])  # utime + stime

    s = ws_open(srv.port)
    s.settimeout(5)
    end_warm = time.time() + 1.0
    while time.time() < end_warm:
        s.recv(262144)
    c0, t0 = cpu_ticks(), time.time()
    total = 0
    end = t0 + seconds
    try:
        while time.time() < end:
            data = s.recv(262144)
            if not data:
                break
            total += len(data)
    except Exception:
        pass
    c1, t1 = cpu_ticks(), time.time()
    s.close()
    srv.close()
    hz = os.sysconf("SC_CLK_TCK")
    cpu_s = (c1 - c0) / hz
    mb = total / 1048576
    return (cpu_s / mb * 1000) if mb else float("nan")  # ms of CPU per MB


MEASUREMENTS = [
    ("startup to listening (ms)", startup_ms, "lower"),
    ("baseline RSS (kB)", baseline_rss, "lower"),
    ("RSS per idle session (kB)", rss_per_session, "lower"),
    ("HTTP /token (req/s, client-bound)", http_rps, "higher"),
    ("terminal sessions (open+close/s)", session_rate, "higher"),
    ("terminal output (MB/s)", output_mbs, "higher"),
    ("CPU per MB delivered (ms)", cpu_for_output, "lower"),
]

results = {name: {"C": [], "Rust": []} for name, _, _ in MEASUREMENTS}

# The sweep in `close` reaps orphans this run created; anything present before the first
# server starts belongs to something else, and no amount of cleanup makes the machine idle.
if running_generators():
    raise SystemExit(
        f"  {len(running_generators())} load generator(s) already running — the machine is\n"
        f"  not idle, and these numbers would measure them too. Kill them and start again."
    )

for round_no in range(1, ROUNDS + 1):
    print(f"  round {round_no}/{ROUNDS}", flush=True)
    for name, fn, _ in MEASUREMENTS:
        for label, binary in (("C", C_BIN), ("Rust", RUST_BIN)):
            try:
                results[name][label].append(fn(binary))
            except Exception as e:
                print(f"    {name} [{label}] failed: {type(e).__name__}: {e}")
                # The failing measurement may not have reached its own `close`.
                reap_orphaned_generators()
            time.sleep(0.3)
    # Belt and braces: `close` sweeps after every server, so this should always be empty.
    # If it is not, something is still running that no round accounted for, and every later
    # round would have been measured against it.
    leftover = running_generators()
    if leftover:
        raise SystemExit(
            f"  round {round_no} left {len(leftover)} load generator(s) running that the\n"
            f"  per-server sweep did not catch. Results discarded rather than reported."
        )

print()
print(f"  {'measurement':<38} {'C':>12} {'Rust':>12}   {'delta':>10}")
print("  " + "-" * 76)
summary = {}
for name, _, better in MEASUREMENTS:
    c = results[name]["C"]
    r = results[name]["Rust"]
    if not c or not r:
        continue
    cm, rm = statistics.median(c), statistics.median(r)
    if better == "lower":
        delta = (cm - rm) / cm * 100  # positive = Rust better
    else:
        delta = (rm - cm) / cm * 100
    winner = "Rust" if delta > 0 else "C"
    summary[name] = (cm, rm, delta, winner)
    print(f"  {name:<38} {cm:>12.1f} {rm:>12.1f}   {delta:>+9.1f}%")

print()
print("  spread (min–max across rounds), to show what is noise:")
for name, _, _ in MEASUREMENTS:
    c, r = results[name]["C"], results[name]["Rust"]
    if not c or not r:
        continue
    print(f"    {name:<38} C {min(c):.1f}–{max(c):.1f}   Rust {min(r):.1f}–{max(r):.1f}")

print()
print(f"  servers that needed SIGKILL: {HARD_KILLS}")
if ORPHANS_REAPED:
    # Said out loud. The orphan appears after the measurement that produced it, so the run is
    # still valid — but "the harness had to clean up after itself N times" is a fact about
    # these numbers that a reader deserves alongside them.
    print(f"  note: {ORPHANS_REAPED} orphaned generator(s) were swept between servers")
else:
    print("  no orphaned generators: nothing this run started outlived the server that did")

with open(os.environ.get("TTYD_BENCH_JSON", "/tmp/bench-results.json"), "w") as f:
    json.dump({k: v for k, v in results.items()}, f, indent=2)
