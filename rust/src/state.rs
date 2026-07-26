//! Server-wide mutable state shared by every connection.

use crate::auth::Authenticator;
use crate::cli::Config;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

/// Per-connection facts the accept loop records and the handlers read back out of the
/// request extensions.
#[derive(Clone, Copy, Debug, Default)]
pub struct ConnInfo {
    pub peer: Option<std::net::SocketAddr>,
    pub tls: bool,
}

impl ConnInfo {
    /// Renders the peer the way the C access log does, falling back to a placeholder for
    /// UNIX domain sockets where there is no address to report.
    pub fn peer_display(&self) -> String {
        match self.peer {
            Some(addr) => addr.ip().to_string(),
            None => "unix".to_string(),
        }
    }
}

pub struct AppState {
    pub cfg: Arc<Config>,
    pub auth: Arc<Authenticator>,
    client_count: AtomicI64,
    /// Set once the server has decided to exit but is waiting for a child process to die,
    /// mirroring the C `force_exit` flag.
    force_exit: AtomicBool,
    accepting: AtomicBool,
    shutdown: watch::Sender<bool>,
    /// Process-group leaders of every running child. Shutdown signals these directly rather
    /// than waiting for each session task to notice, which under load it may not do in time.
    children: Mutex<HashSet<i32>>,
}

impl AppState {
    pub fn new(cfg: Arc<Config>, auth: Arc<Authenticator>) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            auth,
            client_count: AtomicI64::new(0),
            force_exit: AtomicBool::new(false),
            accepting: AtomicBool::new(true),
            shutdown: watch::channel(false).0,
            children: Mutex::new(HashSet::new()),
        })
    }

    pub fn client_count(&self) -> i64 {
        self.client_count.load(Ordering::SeqCst)
    }

    /// Reserves a client slot, honouring `--once` and `--max-clients`. Returns false when the
    /// connection must be refused.
    pub fn try_acquire_client(&self) -> bool {
        let mut current = self.client_count.load(Ordering::SeqCst);
        loop {
            if self.cfg.once && current > 0 {
                return false;
            }
            if self.cfg.max_clients > 0 && current >= self.cfg.max_clients as i64 {
                return false;
            }
            match self.client_count.compare_exchange_weak(
                current,
                current + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    /// Releases a client slot and reports the remaining count.
    pub fn release_client(&self) -> i64 {
        self.client_count.fetch_sub(1, Ordering::SeqCst) - 1
    }

    pub fn register_child(&self, pid: i32) {
        if let Ok(mut children) = self.children.lock() {
            children.insert(pid);
        }
    }

    pub fn unregister_child(&self, pid: i32) {
        if let Ok(mut children) = self.children.lock() {
            children.remove(&pid);
        }
    }

    /// Signals every registered child's process group. Returns how many were signalled.
    pub fn signal_children(&self, signal: i32) -> usize {
        let Ok(children) = self.children.lock() else {
            return 0;
        };
        let mut signalled = 0;
        for pid in children.iter() {
            // Safety: signalling a process group by negated pid; a stale pid at worst
            // returns ESRCH, which is why the result is ignored.
            if unsafe { libc::kill(-pid, signal) } == 0 {
                signalled += 1;
            }
        }
        signalled
    }

    pub fn set_force_exit(&self) {
        self.force_exit.store(true, Ordering::SeqCst);
    }

    pub fn force_exit(&self) -> bool {
        self.force_exit.load(Ordering::SeqCst)
    }

    pub fn is_accepting(&self) -> bool {
        self.accepting.load(Ordering::SeqCst)
    }

    /// Stops the accept loop and tells live sessions to wind down.
    pub fn begin_shutdown(&self) {
        self.accepting.store(false, Ordering::SeqCst);
        let _ = self.shutdown.send(true);
    }

    /// Resolves once shutdown has begun, including when it began before this was called.
    pub async fn wait_for_shutdown(&self) {
        let mut rx = self.shutdown.subscribe();
        if *rx.borrow() {
            return;
        }
        let _ = rx.changed().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::AuthMode;

    fn state(mutate: impl FnOnce(&mut Config)) -> Arc<AppState> {
        let mut cfg = Config {
            argv: vec!["bash".into()],
            command: "bash".into(),
            ..Default::default()
        };
        mutate(&mut cfg);
        let auth = Authenticator::new(AuthMode::None).unwrap();
        AppState::new(Arc::new(cfg), auth)
    }

    #[test]
    fn unlimited_by_default() {
        let s = state(|_| {});
        for _ in 0..100 {
            assert!(s.try_acquire_client());
        }
        assert_eq!(s.client_count(), 100);
    }

    #[test]
    fn max_clients_is_enforced() {
        let s = state(|c| c.max_clients = 2);
        assert!(s.try_acquire_client());
        assert!(s.try_acquire_client());
        assert!(!s.try_acquire_client());
        assert_eq!(s.release_client(), 1);
        assert!(s.try_acquire_client());
    }

    #[test]
    fn once_allows_a_single_concurrent_client() {
        let s = state(|c| c.once = true);
        assert!(s.try_acquire_client());
        assert!(!s.try_acquire_client());
    }

    #[test]
    fn shutdown_stops_accepting() {
        let s = state(|_| {});
        assert!(s.is_accepting());
        s.begin_shutdown();
        assert!(!s.is_accepting());
    }
}
