//! PTY process management.
//!
//! This mirrors the Unix half of the C `src/pty.c`: a pseudo terminal is opened, the child
//! becomes a session leader with the slave as its controlling terminal, and the master end is
//! pumped by two tasks driven by readiness on a non-blocking descriptor.
//!
//! Nothing here spawns an OS thread. An earlier version used three per session — a reader, a
//! writer and a reaper — which cost about 234 kB of resident memory each session against the
//! C build's 17 kB, measured; `--max-clients` multiplies that. Readiness-driven tasks are
//! heap-sized rather than stack-sized, and `tokio::process` reaps through the runtime's own
//! SIGCHLD handling rather than a thread per child.
//!
//! Output flows through a bounded channel, so a slow WebSocket client stalls the reader task
//! and the kernel PTY buffer applies backpressure to the child — the same effect the C
//! version achieves with libuv's explicit read pause/resume.

use anyhow::{Context, Result};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::unix::AsyncFd;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

/// Size of a single read from the PTY master when `--srv-buf-size` is not set, matching
/// libuv's suggested buffer size.
pub const DEFAULT_READ_CHUNK: usize = 65536;

/// How much unwritten terminal input may be queued for one child before the session stops
/// reading from its client.
///
/// A child that is slow to read — or not reading at all — lets the kernel PTY buffer fill,
/// after which the writer thread blocks in `write`. Everything the client keeps sending
/// piles up in front of it, so without a ceiling one authenticated writable client can grow
/// the *server's* memory without bound, and `--max-clients` multiplies that. Reaching the
/// ceiling stops the session reading its socket until the child catches up, which pushes
/// back through TCP instead of dropping input or dropping the client.
pub const MAX_QUEUED_INPUT: usize = 4 * 1024 * 1024;

/// How the child process finished, with the same numbers the C version derives from `waitpid`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitInfo {
    /// `WEXITSTATUS`, or `128 + signal` when the child was terminated by a signal.
    pub code: i32,
    pub signal: Option<i32>,
}

impl ExitInfo {
    pub fn success(&self) -> bool {
        self.signal.is_none() && self.code == 0
    }
}

/// Everything a session needs to drive one child process.
pub struct Pty {
    pub pid: i32,
    /// Set once the child has been reaped. After that the pid may be handed to an unrelated
    /// process, so signalling it would hit a stranger's process group.
    reaped: Arc<AtomicBool>,
    master: Arc<OwnedFd>,
    writer: mpsc::UnboundedSender<Vec<u8>>,
    /// Bytes accepted for the child but not yet written to the PTY.
    queued_input: Arc<AtomicUsize>,
    pub columns: u16,
    pub rows: u16,
}

pub struct Spawned {
    pub pty: Pty,
    /// Output chunks read from the PTY. Closing indicates end of file.
    pub output: mpsc::Receiver<Vec<u8>>,
    /// Resolves once the child has been reaped.
    pub exit: oneshot::Receiver<ExitInfo>,
}

/// Describes the child process to launch.
pub struct SpawnRequest<'a> {
    pub argv: &'a [String],
    pub env: &'a [(String, String)],
    pub cwd: Option<&'a Path>,
    pub columns: u16,
    pub rows: u16,
    /// Largest amount of terminal output read — and therefore sent — at once.
    pub read_chunk: usize,
}

pub fn spawn(req: SpawnRequest<'_>) -> Result<Spawned> {
    let program = req
        .argv
        .first()
        .context("cannot spawn a process without a command")?;

    let winsize = nix::pty::Winsize {
        ws_row: req.rows,
        ws_col: req.columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = nix::pty::openpty(Some(&winsize), None).context("openpty failed")?;
    let master = Arc::new(pty.master);
    let slave = pty.slave;

    // Marked close-on-exec before anything can fork, not after this child is spawned:
    // another session starting concurrently would otherwise inherit this terminal's master
    // descriptor and be able to read and write someone else's session. `Stdio` dup2s the
    // slave onto 0/1/2 in the child, and dup2 clears the flag on the copy, so the child
    // still gets its controlling terminal.
    set_cloexec(master.as_raw_fd())?;
    set_cloexec(slave.as_raw_fd())?;
    // Readiness-driven I/O requires this; the slave stays blocking so the child sees an
    // ordinary terminal.
    set_nonblocking(master.as_raw_fd())?;

    let mut cmd = Command::new(program);
    cmd.args(&req.argv[1..]);
    for (key, value) in req.env {
        cmd.env(key, value);
    }
    if let Some(cwd) = req.cwd {
        cmd.current_dir(cwd);
    }

    // std duplicates these onto fds 0/1/2 in the child before running the `pre_exec` hook.
    cmd.stdin(Stdio::from(slave.try_clone().context("dup pty slave")?));
    cmd.stdout(Stdio::from(slave.try_clone().context("dup pty slave")?));
    cmd.stderr(Stdio::from(slave.try_clone().context("dup pty slave")?));

    unsafe {
        cmd.pre_exec(|| {
            // Detach from the parent's session so the child leads its own process group; the
            // server later signals that whole group, exactly as the C version does.
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Claim the slave (already dup'd onto fd 0) as the controlling terminal.
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd
        .spawn()
        .with_context(|| format!("failed to execute {program}"))?;
    drop(slave);

    let pid = child
        .id()
        .context("child exited before its pid could be read")? as i32;

    let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>(1);
    let (exit_tx, exit_rx) = oneshot::channel::<ExitInfo>();
    let (write_tx, write_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    let queued_input = Arc::new(AtomicUsize::new(0));

    // One registration shared by both directions: `AsyncFd` tracks read and write interest
    // separately, so the two tasks do not contend.
    let io = Arc::new(AsyncFd::new(dup_master(&master)?).context("register pty master")?);
    tokio::spawn(pump_output(io.clone(), out_tx, req.read_chunk.max(1)));
    tokio::spawn(pump_input(io, write_rx, queued_input.clone()));

    let reaped = Arc::new(AtomicBool::new(false));
    tokio::spawn(reap(child, exit_tx, reaped.clone()));

    Ok(Spawned {
        pty: Pty {
            pid,
            reaped,
            master,
            writer: write_tx,
            queued_input,
            columns: req.columns,
            rows: req.rows,
        },
        output: out_rx,
        exit: exit_rx,
    })
}

impl Pty {
    /// Queues data for the child's terminal input. Returns false once the writer is gone.
    ///
    /// This never blocks and never refuses: the caller drives both directions of the session
    /// from one `select!`, so waiting here would also stop draining the child's output, and a
    /// child that is simultaneously writing and being written to would wedge against itself.
    /// The ceiling is enforced by the caller declining to read more from the socket while
    /// [`Pty::input_backlog_is_full`] holds — see `ws::session`.
    pub fn write(&self, data: Vec<u8>) -> bool {
        let len = data.len();
        self.queued_input.fetch_add(len, Ordering::AcqRel);
        if self.writer.send(data).is_err() {
            // Nothing will ever write these bytes, so they must not stay counted. Saturating
            // because the writer thread zeroes the counter on its way out, and this can lose
            // the race with it — going negative would wrap and pin the backlog full forever.
            let _ = self
                .queued_input
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                    Some(queued.saturating_sub(len))
                });
            return false;
        }
        true
    }

    /// Whether the child is far enough behind on input that its client should be made to
    /// wait. One in-flight chunk may still exceed the ceiling; what matters is that the next
    /// one is not read until the backlog has drained.
    pub fn input_backlog_is_full(&self) -> bool {
        self.queued_input.load(Ordering::Acquire) >= MAX_QUEUED_INPUT
    }

    /// Applies a new terminal size. Zero dimensions are ignored, like the C version.
    pub fn resize(&mut self, columns: u16, rows: u16) -> bool {
        if columns == 0 || rows == 0 {
            return false;
        }
        self.columns = columns;
        self.rows = rows;
        let size = libc::winsize {
            ws_row: rows,
            ws_col: columns,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // Safety: `master` is a live PTY master descriptor for the lifetime of `self`.
        unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &size) == 0 }
    }

    /// Signals the child's entire process group, matching the C `uv_kill(-pid, sig)`.
    pub fn kill(&self, signal: i32) -> bool {
        if self.pid <= 0 || self.reaped.load(Ordering::SeqCst) {
            return false;
        }
        unsafe { libc::kill(-self.pid, signal) == 0 }
    }

    pub fn is_running(&self) -> bool {
        self.pid > 0
            && !self.reaped.load(Ordering::SeqCst)
            && unsafe { libc::kill(self.pid, 0) } == 0
    }
}

fn set_cloexec(fd: std::os::fd::RawFd) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error()).context("fcntl(F_GETFD)");
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error()).context("fcntl(F_SETFD)");
    }
    Ok(())
}

fn dup_master(master: &Arc<OwnedFd>) -> Result<OwnedFd> {
    let dup = master.try_clone().context("dup pty master")?;
    set_cloexec(dup.as_raw_fd())?;
    Ok(dup)
}

fn set_nonblocking(fd: std::os::fd::RawFd) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error()).context("fcntl(F_GETFL)");
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error()).context("fcntl(F_SETFL)");
    }
    Ok(())
}

/// Moves terminal output to the session, one readiness notification at a time.
///
/// `send` on the bounded channel awaits rather than parking a thread, which is what keeps
/// the backpressure story intact: a client that stops reading stalls this task, the kernel
/// PTY buffer fills, and the child blocks on its next write.
async fn pump_output(io: Arc<AsyncFd<OwnedFd>>, tx: mpsc::Sender<Vec<u8>>, chunk: usize) {
    let mut buf = vec![0u8; chunk];
    loop {
        let mut ready = match io.readable().await {
            Ok(ready) => ready,
            Err(_) => break,
        };
        match ready.try_io(|inner| read_fd(inner.get_ref().as_raw_fd(), &mut buf)) {
            // Linux reports EIO on the master once the last slave descriptor closes, which is
            // the normal end of a session rather than a failure.
            Ok(Ok(0)) | Ok(Err(_)) => break,
            Ok(Ok(n)) => {
                if tx.send(buf[..n].to_vec()).await.is_err() {
                    break;
                }
            }
            // Not actually ready after all; wait for the next notification.
            Err(_would_block) => continue,
        }
    }
}

/// Moves client input to the terminal, releasing the backlog accounting as bytes land.
async fn pump_input(
    io: Arc<AsyncFd<OwnedFd>>,
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
    queued: Arc<AtomicUsize>,
) {
    while let Some(chunk) = rx.recv().await {
        let mut written = 0;
        let outcome = loop {
            if written == chunk.len() {
                break Ok(());
            }
            let mut ready = match io.writable().await {
                Ok(ready) => ready,
                Err(e) => break Err(e),
            };
            match ready.try_io(|inner| write_fd(inner.get_ref().as_raw_fd(), &chunk[written..])) {
                Ok(Ok(0)) => break Err(std::io::Error::from(std::io::ErrorKind::WriteZero)),
                Ok(Ok(n)) => written += n,
                Ok(Err(e)) => break Err(e),
                Err(_would_block) => continue,
            }
        };
        // Released only once the bytes have reached the PTY, so the ceiling measures what is
        // actually outstanding rather than what has been handed over.
        queued.fetch_sub(chunk.len(), Ordering::AcqRel);
        if outcome.is_err() {
            break;
        }
    }
    // Whatever is still queued will never be written now. Leaving it counted would pin the
    // backlog above its ceiling forever, and the session gates its socket reads on that — it
    // would stop reading from its client and never start again.
    rx.close();
    while let Ok(chunk) = rx.try_recv() {
        queued.fetch_sub(chunk.len(), Ordering::AcqRel);
    }
    queued.store(0, Ordering::Release);
}

fn read_fd(fd: std::os::fd::RawFd, buf: &mut [u8]) -> std::io::Result<usize> {
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(n as usize)
}

fn write_fd(fd: std::os::fd::RawFd, buf: &[u8]) -> std::io::Result<usize> {
    let n = unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(n as usize)
}

/// Awaits the child through the runtime's SIGCHLD handling rather than a thread per child.
async fn reap(mut child: Child, tx: oneshot::Sender<ExitInfo>, reaped: Arc<AtomicBool>) {
    let info = match child.wait().await {
        Ok(status) => match status.signal() {
            Some(sig) => ExitInfo {
                code: 128 + sig,
                signal: Some(sig),
            },
            None => ExitInfo {
                code: status.code().unwrap_or(0),
                signal: None,
            },
        },
        Err(_) => ExitInfo {
            code: -1,
            signal: None,
        },
    };
    // Flag before publishing the status: once waitpid has returned, the pid is free for
    // reuse and must never be signalled again.
    reaped.store(true, Ordering::SeqCst);
    let _ = tx.send(info);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn req<'a>(argv: &'a [String], env: &'a [(String, String)]) -> SpawnRequest<'a> {
        SpawnRequest {
            argv,
            env,
            cwd: None,
            columns: 80,
            rows: 24,
            read_chunk: DEFAULT_READ_CHUNK,
        }
    }

    async fn drain(rx: &mut mpsc::Receiver<Vec<u8>>, needle: &str) -> String {
        let mut seen = String::new();
        for _ in 0..200 {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
                Ok(Some(chunk)) => {
                    seen.push_str(&String::from_utf8_lossy(&chunk));
                    if seen.contains(needle) {
                        return seen;
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
        seen
    }

    #[tokio::test]
    async fn child_output_reaches_the_reader() {
        let argv = vec!["/bin/sh".into(), "-c".into(), "echo marker-ok".into()];
        let mut s = spawn(req(&argv, &[])).expect("spawn");
        let seen = drain(&mut s.output, "marker-ok").await;
        assert!(seen.contains("marker-ok"), "got {seen:?}");
    }

    #[tokio::test]
    async fn environment_is_passed_to_the_child() {
        let argv = vec![
            "/bin/sh".into(),
            "-c".into(),
            "echo T=$TERM U=$TTYD_USER".into(),
        ];
        let env = vec![
            ("TERM".to_string(), "vt220".to_string()),
            ("TTYD_USER".to_string(), "alice".to_string()),
        ];
        let mut s = spawn(req(&argv, &env)).expect("spawn");
        let seen = drain(&mut s.output, "U=alice").await;
        assert!(seen.contains("T=vt220"), "got {seen:?}");
        assert!(seen.contains("U=alice"), "got {seen:?}");
    }

    #[tokio::test]
    async fn exit_code_is_reported() {
        let argv = vec!["/bin/sh".into(), "-c".into(), "exit 3".into()];
        let s = spawn(req(&argv, &[])).expect("spawn");
        let info = tokio::time::timeout(Duration::from_secs(5), s.exit)
            .await
            .expect("exit timed out")
            .expect("exit channel");
        assert_eq!(info.code, 3);
        assert_eq!(info.signal, None);
        assert!(!info.success());
    }

    #[tokio::test]
    async fn signal_death_reports_128_plus_signal() {
        let argv = vec![
            "/bin/sh".into(),
            "-c".into(),
            "kill -TERM $$; sleep 5".into(),
        ];
        let s = spawn(req(&argv, &[])).expect("spawn");
        let info = tokio::time::timeout(Duration::from_secs(5), s.exit)
            .await
            .expect("exit timed out")
            .expect("exit channel");
        assert_eq!(info.signal, Some(libc::SIGTERM));
        assert_eq!(info.code, 128 + libc::SIGTERM);
    }

    #[tokio::test]
    async fn input_is_echoed_back_through_the_terminal() {
        let argv = vec!["/bin/cat".into()];
        let mut s = spawn(req(&argv, &[])).expect("spawn");
        assert!(s.pty.write(b"hello-pty\n".to_vec()));
        let seen = drain(&mut s.output, "hello-pty").await;
        assert!(seen.contains("hello-pty"), "got {seen:?}");
        s.pty.kill(libc::SIGKILL);
    }

    #[tokio::test]
    async fn resize_is_visible_to_the_child() {
        let argv = vec![
            "/bin/sh".into(),
            "-c".into(),
            "sleep 0.4; stty size < /dev/tty".into(),
        ];
        let mut s = spawn(req(&argv, &[])).expect("spawn");
        assert!(s.pty.resize(120, 40));
        let seen = drain(&mut s.output, "40 120").await;
        assert!(seen.contains("40 120"), "expected '40 120', got {seen:?}");
    }

    #[tokio::test]
    async fn kill_terminates_the_whole_process_group() {
        let argv = vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()];
        let s = spawn(req(&argv, &[])).expect("spawn");
        // Let the shell get as far as starting its child before signalling.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(s.pty.is_running());
        assert!(s.pty.kill(libc::SIGHUP));
        let info = tokio::time::timeout(Duration::from_secs(5), s.exit)
            .await
            .expect("exit timed out")
            .expect("exit channel");
        assert_eq!(info.signal, Some(libc::SIGHUP));
    }

    #[tokio::test]
    async fn output_channel_closes_when_the_child_exits() {
        let argv = vec!["/bin/sh".into(), "-c".into(), "echo done".into()];
        let mut s = spawn(req(&argv, &[])).expect("spawn");
        let mut closed = false;
        for _ in 0..50 {
            match tokio::time::timeout(Duration::from_secs(2), s.output.recv()).await {
                Ok(None) => {
                    closed = true;
                    break;
                }
                Ok(Some(_)) => continue,
                Err(_) => break,
            }
        }
        assert!(closed, "reader channel should close at end of session");
    }

    #[tokio::test]
    async fn spawn_fails_for_a_missing_program() {
        let argv = vec!["/nonexistent/definitely-not-here".to_string()];
        assert!(spawn(req(&argv, &[])).is_err());
    }

    #[tokio::test]
    async fn a_child_that_never_reads_makes_its_backlog_report_full() {
        // `sleep` never reads its terminal, so the kernel PTY buffer fills and the writer
        // thread blocks. Before the backlog was bounded, every byte sent after that point
        // was held in the server's memory with nothing to stop it growing.
        let argv = vec!["/bin/sleep".into(), "60".into()];
        let s = spawn(req(&argv, &[])).expect("spawn");

        let chunk = vec![b'A'; 64 * 1024];
        let mut sent = 0usize;
        // The loop is bounded so a regression fails the test rather than running forever.
        while sent <= MAX_QUEUED_INPUT * 2 && !s.pty.input_backlog_is_full() {
            assert!(s.pty.write(chunk.clone()), "writer died unexpectedly");
            sent += chunk.len();
            tokio::task::yield_now().await;
        }

        assert!(
            s.pty.input_backlog_is_full(),
            "wrote {sent} bytes to a child that never reads without the backlog filling"
        );
        assert!(
            sent <= MAX_QUEUED_INPUT + chunk.len(),
            "backlog reported full only after {sent} bytes, past the {MAX_QUEUED_INPUT} ceiling"
        );

        let _ = s.pty.kill(9);
    }

    #[tokio::test]
    async fn a_child_that_keeps_up_drains_its_backlog() {
        // The ceiling must be a moving bound, not a per-session budget: a child that reads
        // lets far more than MAX_QUEUED_INPUT pass through, because the backlog drains as it
        // goes. `cat` echoes, so its output has to be drained too or it stops reading.
        let argv = vec!["/bin/cat".into()];
        let Spawned {
            pty,
            mut output,
            exit: _exit,
        } = spawn(req(&argv, &[])).expect("spawn");
        tokio::spawn(async move { while output.recv().await.is_some() {} });

        let chunk = vec![b'x'; 4096];
        let mut sent = 0usize;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while sent < MAX_QUEUED_INPUT * 2 {
            if pty.input_backlog_is_full() {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "backlog never drained; {sent} bytes through"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
                continue;
            }
            assert!(pty.write(chunk.clone()), "writer died after {sent} bytes");
            sent += chunk.len();
            tokio::task::yield_now().await;
        }

        let _ = pty.kill(9);
    }
}
