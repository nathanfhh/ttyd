"""Twenty-minute end-to-end soak: a real browser session held open under concurrent load.

The existing checks each miss something this one covers. `browser-check.py` drives a real
Chromium but only for about a minute. The protocol soak runs for ten minutes but with
synthetic socket clients, so it never exercises xterm.js, the keepalive path as the frontend
sees it, or anything the browser does over time.

Here one browser page stays open for the whole run and is asked to prove it still works —
by running a command and reading the file the shell created — every 30 seconds, while
synthetic clients churn sessions alongside it. What is being tested is longevity: that a
terminal someone left open in a tab is still usable twenty minutes later, and that holding
it open costs the server nothing that accumulates.
"""
import base64
import collections
import os
import pathlib
import re
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time

from playwright.sync_api import sync_playwright

REPO = pathlib.Path(__file__).resolve().parent
BIN = sys.argv[1] if len(sys.argv) > 1 else str(REPO / "target" / "release" / "ttyd")
DURATION = int(sys.argv[2]) if len(sys.argv) > 2 else 1200
CHURN_CLIENTS = 4
# A unique directory per run: two soaks on one host — parallel CI jobs, say — would
# otherwise share marker files and read each other's results as their own.
WORK = pathlib.Path(tempfile.mkdtemp(prefix="ttyd-e2e-soak-"))

proc = subprocess.Popen(
    [BIN, "-p", "0", "-W", "--max-clients", "16", "bash"],
    stdout=subprocess.DEVNULL,
    stderr=subprocess.PIPE,
    text=True,
)

try:
    port_found = threading.Event()
    holder = {}
    log_tail = collections.deque(maxlen=400)


    def drain_stderr():
        for line in proc.stderr:
            log_tail.append(line.rstrip())
            if "port" not in holder:
                m = re.search(r"Listening on port:\s*(\d+)", line)
                if m:
                    holder["port"] = int(m.group(1))
                    port_found.set()


    threading.Thread(target=drain_stderr, daemon=True).start()
    assert port_found.wait(20), "server never reported a port"
    port = holder["port"]
    print(f"  server pid {proc.pid} on port {port}, {DURATION}s, {CHURN_CLIENTS} churn clients")
    print(f"  work dir {WORK}")

    # ---------------------------------------------------------------- synthetic churn
    stop = threading.Event()
    churn = {"sessions": 0, "errors": 0}
    lock = threading.Lock()


    def mask(payload):
        m = os.urandom(4)
        n = len(payload)
        if n < 126:
            head = b"\x82" + bytes([0x80 | n]) + m
        else:
            head = b"\x82" + bytes([0x80 | 126]) + struct.pack(">H", n) + m
        return head + bytes(c ^ m[i % 4] for i, c in enumerate(payload))


    def churn_client():
        """Connect, read, disconnect — so the browser session is never the only thing running."""
        while not stop.is_set():
            try:
                s = socket.create_connection(("127.0.0.1", port), 5)
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
                s.sendall(mask(b'{"columns":100,"rows":30}'))
                s.settimeout(5)
                end = time.time() + 5
                while time.time() < end and not stop.is_set():
                    if not s.recv(65536):
                        break
                s.close()
                with lock:
                    churn["sessions"] += 1
            except Exception:
                with lock:
                    churn["errors"] += 1
                time.sleep(0.5)


    for _ in range(CHURN_CLIENTS):
        threading.Thread(target=churn_client, daemon=True).start()


    # ---------------------------------------------------------------- server sampling
    def sample():
        with open(f"/proc/{proc.pid}/status") as f:
            status = f.read()
        rss = int(re.search(r"VmRSS:\s+(\d+)", status).group(1))
        threads = int(re.search(r"Threads:\s+(\d+)", status).group(1))
        fds = len(os.listdir(f"/proc/{proc.pid}/fd"))
        return rss, fds, threads


    def launch_chromium(pw):
        override = os.environ.get("TTYD_CHROMIUM")
        if override:
            return pw.chromium.launch(args=["--no-sandbox"], executable_path=override)
        try:
            return pw.chromium.launch(args=["--no-sandbox"])
        except Exception:
            root = pathlib.Path(os.environ.get("PLAYWRIGHT_BROWSERS_PATH", ""))
            found = sorted(root.glob("chromium*/chrome-linux/chrome")) if root.name else []
            if not found:
                raise
            return pw.chromium.launch(args=["--no-sandbox"], executable_path=str(found[-1]))


    samples = []
    failures = []
    disconnects = []
    page_errors = []

    with sync_playwright() as pw:
        browser = launch_chromium(pw)
        page = browser.new_page(viewport={"width": 1200, "height": 760})
        page.on("pageerror", lambda e: page_errors.append(str(e)))
        page.on("websocket", lambda ws: ws.on("close", lambda _: disconnects.append(time.time())))

        page.goto(f"http://127.0.0.1:{port}/", wait_until="domcontentloaded")
        page.wait_for_selector(".xterm-screen", timeout=20000)
        page.click(".xterm-screen")
        print("  browser session open")

        def prove_alive(tag):
            """Runs a command and reads back the file it created — the session works, or it does not."""
            marker = WORK / f"alive-{tag}"
            page.keyboard.type(f"echo ALIVE-{tag}-$$ > {marker}")
            page.keyboard.press("Enter")
            deadline = time.time() + 20
            while time.time() < deadline:
                if marker.exists() and marker.stat().st_size:
                    time.sleep(0.2)
                    text = marker.read_text(errors="replace").strip()
                    return text.startswith(f"ALIVE-{tag}-")
                time.sleep(0.25)
            return False

        assert prove_alive("t0"), "the session did not work even at the start"
        print("  t=   0s  session verified working")

        start = time.time()
        next_probe = start + 30
        next_sample = start + 60
        probes_ok = probes_run = 1

        while time.time() - start < DURATION:
            time.sleep(1)
            now = time.time()

            if now >= next_probe:
                next_probe += 30
                tag = f"t{int(now - start)}"
                probes_run += 1
                ok = prove_alive(tag)
                probes_ok += 1 if ok else 0
                if not ok:
                    failures.append(f"session unusable at t={int(now - start)}s")
                    print(f"  t={int(now - start):>4}s  PROBE FAILED", flush=True)

            if now >= next_sample:
                next_sample += 60
                try:
                    rss, fds, threads = sample()
                except Exception:
                    failures.append("server process disappeared")
                    break
                with lock:
                    sessions, errors = churn["sessions"], churn["errors"]
                samples.append((int(now - start), rss, fds, threads))
                print(
                    f"  t={int(now - start):>4}s  rss={rss:>7} kB  fds={fds:>3}  threads={threads:>3}"
                    f"  probes={probes_ok}/{probes_run}  churn={sessions:>4}  churn_err={errors}"
                    f"  ws_closes={len(disconnects)}",
                    flush=True,
                )

        # The real question: is the tab someone left open twenty minutes ago still a terminal?
        final = prove_alive("final")
        probes_run += 1
        probes_ok += 1 if final else 0
        if not final:
            failures.append("the browser session was not usable at the end of the run")

        page.screenshot(path=str(WORK / "screenshot.png"))
        browser.close()

    stop.set()
    time.sleep(1)
    alive = proc.poll() is None
finally:
    # Reached even when an assertion above fires. `Popen` does not reap or kill on garbage
    # collection, so without this an early failure leaves a ttyd and its shell running for
    # the rest of the machine's life — the same guarantee `tests/common/mod.rs` makes.
    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5)

print()
print(f"  browser session usable at the end: {final}")
print(f"  probes passed: {probes_ok}/{probes_run}")
print(f"  websocket closes seen by the browser: {len(disconnects)}")
print(f"  uncaught frontend errors: {len(page_errors)}" + (f" -> {page_errors[:2]}" if page_errors else ""))
print(f"  server still alive: {alive}")
with lock:
    print(f"  churn sessions: {churn['sessions']}, churn errors: {churn['errors']}")

if len(samples) >= 3:
    _, rss0, fd0, th0 = samples[1]
    _, rss1, fd1, th1 = samples[-1]
    print(f"  rss     {rss0} -> {rss1} kB ({rss1 - rss0:+})")
    print(f"  fds     {fd0} -> {fd1} ({fd1 - fd0:+})")
    print(f"  threads {th0} -> {th1} ({th1 - th0:+})")

if failures:
    print("\n  FAILED: " + "; ".join(failures))
    print("\n  --- last server log lines ---")
    for line in list(log_tail)[-15:]:
        print("   " + line[:170])
else:
    print("\n  PASSED: the session stayed usable for the whole run")
sys.exit(1 if failures else 0)
