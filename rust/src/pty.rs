//! PTY process management.
//!
//! This mirrors the Unix half of the C `src/pty.c`: a pseudo terminal is opened, the child
//! becomes a session leader with the slave as its controlling terminal, and the master end is
//! pumped by dedicated threads. Output flows through a bounded channel, so a slow WebSocket
//! client stops the reader thread and the kernel PTY buffer applies backpressure to the child —
//! the same effect the C version achieves with libuv's explicit read pause/resume.

use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

/// Size of a single read from the PTY master when `--srv-buf-size` is not set, matching
/// libuv's suggested buffer size.
pub const DEFAULT_READ_CHUNK: usize = 65536;

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

    let pid = child.id() as i32;

    set_cloexec(master.as_raw_fd())?;

    let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>(1);
    let (exit_tx, exit_rx) = oneshot::channel::<ExitInfo>();
    let (write_tx, write_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    spawn_reader(dup_as_file(&master)?, out_tx, req.read_chunk.max(1));
    spawn_writer(dup_as_file(&master)?, write_rx);
    let reaped = Arc::new(AtomicBool::new(false));
    spawn_reaper(child, exit_tx, reaped.clone());

    Ok(Spawned {
        pty: Pty {
            pid,
            reaped,
            master,
            writer: write_tx,
            columns: req.columns,
            rows: req.rows,
        },
        output: out_rx,
        exit: exit_rx,
    })
}

impl Pty {
    /// Queues data for the child's terminal input. Returns false once the writer is gone.
    pub fn write(&self, data: Vec<u8>) -> bool {
        self.writer.send(data).is_ok()
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

fn dup_as_file(master: &Arc<OwnedFd>) -> Result<std::fs::File> {
    let dup = master.try_clone().context("dup pty master")?;
    set_cloexec(dup.as_raw_fd())?;
    Ok(std::fs::File::from(dup))
}

fn spawn_reader(mut file: std::fs::File, tx: mpsc::Sender<Vec<u8>>, chunk: usize) {
    std::thread::spawn(move || {
        let mut buf = vec![0u8; chunk];
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    // `blocking_send` parks this thread while the client is behind, which is
                    // what throttles the child process.
                    if tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                // Linux reports EIO on the master once the last slave descriptor is closed,
                // which is the normal end-of-session condition rather than a failure.
                Err(_) => break,
            }
        }
    });
}

fn spawn_writer(mut file: std::fs::File, mut rx: mpsc::UnboundedReceiver<Vec<u8>>) {
    std::thread::spawn(move || {
        while let Some(chunk) = rx.blocking_recv() {
            if file.write_all(&chunk).is_err() {
                break;
            }
        }
    });
}

fn spawn_reaper(mut child: Child, tx: oneshot::Sender<ExitInfo>, reaped: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let info = match child.wait() {
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
    });
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
}
