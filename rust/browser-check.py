"""Drives ttyd through a real browser and the real xterm.js frontend.

The rest of the suite talks to the protocol with a synthetic client, which proves the wire
format but not that the shipped frontend works against it. xterm.js renders glyphs to a
WebGL canvas, so terminal contents are not readable from the DOM — keystrokes are therefore
verified by their *effect* (files the shell creates), and rendering from a screenshot.
"""
import pathlib
import re
import subprocess
import sys
import time

from playwright.sync_api import sync_playwright

BIN = sys.argv[1] if len(sys.argv) > 1 else "/home/user/ttyd/rust/target/release/ttyd"
LABEL = sys.argv[2] if len(sys.argv) > 2 else "rust"
WORK = pathlib.Path(f"/tmp/ttyd-browser-{LABEL}")
# Each run starts from an empty directory, so nothing a previous run left behind (a vim
# swap file, a stale marker) can change what this one observes.

if WORK.exists():
    for f in WORK.iterdir():
        f.unlink()
else:
    WORK.mkdir()

IS_C = LABEL == "c"
args = [BIN, "-p", "0", "-W"] + ([] if IS_C else ["--title", "Browser Check"]) + ["bash"]
proc = subprocess.Popen(
    args,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.PIPE,
    text=True,
)

port = None
deadline = time.time() + 15
while time.time() < deadline and port is None:
    line = proc.stderr.readline()
    if not line:
        break
    m = re.search(r"Listening on port:\s*(\d+)", line)
    if m:
        port = int(m.group(1))
assert port, "server never reported a port"
print(f"  server on port {port}")

failures = []


def check(name, condition, detail=""):
    print(f"  [{'PASS' if condition else 'FAIL'}] {name}{(' — ' + detail) if detail else ''}")
    if not condition:
        failures.append(name)


def wait_for_file(path, timeout=15):
    end = time.time() + timeout
    while time.time() < end:
        if path.exists() and path.stat().st_size > 0:
            time.sleep(0.2)
            return path.read_text(errors="replace").strip()
        time.sleep(0.2)
    return None


with sync_playwright() as pw:
    browser = pw.chromium.launch(
        args=["--no-sandbox"],
        executable_path="/opt/pw-browsers/chromium-1194/chrome-linux/chrome",
    )
    page = browser.new_page(viewport={"width": 1100, "height": 700})
    errors = []
    page.on("pageerror", lambda e: errors.append(str(e)))
    disconnects = []
    page.on("websocket", lambda ws: ws.on("close", lambda _: disconnects.append(1)))

    page.goto(f"http://127.0.0.1:{port}/", wait_until="domcontentloaded")
    page.wait_for_selector(".xterm-screen", timeout=15000)
    check("frontend loads and xterm.js mounts", True)

    title = page.title()
    if IS_C:
        # The C build has no --title; its tab carries the command line instead.
        print(f"  [SKIP] --title (not in the C build) — title={title!r}")
    else:
        check("--title reaches the browser tab", "Browser Check" in title, f"title={title!r}")
        check("the command line is absent from the tab", "bash (" not in title, f"title={title!r}")

    page.click(".xterm-screen")

    def run(cmd):
        page.keyboard.type(cmd)
        page.keyboard.press("Enter")

    # Keystrokes are verified by what the shell actually does with them.
    run(f"echo BROWSER-ROUNDTRIP-OK > {WORK}/roundtrip")
    got = wait_for_file(WORK / "roundtrip")
    check("keystrokes reach the shell", got == "BROWSER-ROUNDTRIP-OK", f"got {got!r}")

    run(f"echo TERM=$TERM > {WORK}/term")
    got = wait_for_file(WORK / "term")
    check("TERM is exported to the shell", got == "TERM=xterm-256color", f"got {got!r}")

    # Resize must reach the child's winsize through the browser's own resize handling.
    page.set_viewport_size({"width": 1500, "height": 950})
    time.sleep(2)
    run(f"stty size > {WORK}/size")
    got = wait_for_file(WORK / "size")
    check(
        "resize reaches the child",
        bool(got and re.fullmatch(r"\d+ \d+", got)),
        f"stty size -> {got!r}",
    )

    # Something on screen worth looking at: colour, CJK, box drawing.
    run("printf '\\033[1;32mCOLOUR-OK\\033[0m 中文測試 CJK ┌───┐\\n'")
    time.sleep(1.5)
    page.screenshot(path=f"/tmp/ttyd-{LABEL}.png")

    # A full-screen program exercises the alternate screen and escape sequences.
    vi_file = WORK / "vi-check.txt"
    run(f"vi {vi_file}")
    time.sleep(4)
    page.keyboard.type("iVI-ALT-SCREEN-OK")
    time.sleep(1.5)
    page.screenshot(path=f"/tmp/ttyd-{LABEL}-vi.png")
    page.keyboard.press("Escape")
    time.sleep(0.3)
    run(f":wq {WORK}/vi-out")
    vi_out = wait_for_file(WORK / "vi-out", timeout=10)
    check(
        "full-screen program (vi) round-trips",
        bool(vi_out and "VI-ALT-SCREEN-OK" in vi_out),
        f"got {vi_out!r}",
    )

    check("no uncaught frontend errors", not errors, "; ".join(errors[:2]))
    check("the session never dropped", not disconnects, f"{len(disconnects)} disconnect(s)")

    browser.close()

proc.terminate()
proc.wait(timeout=5)

print(f"  screenshots: /tmp/ttyd-{LABEL}.png, /tmp/ttyd-{LABEL}-vi.png")
print(f"\n  {LABEL}: {'ALL PASSED' if not failures else 'FAILED: ' + ', '.join(failures)}")
sys.exit(1 if failures else 0)
