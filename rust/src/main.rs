use std::sync::Arc;
use ttyd::auth::Authenticator;
use ttyd::cli::{self, Outcome};
use ttyd::state::AppState;
use ttyd::{http, serve, utils};

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let cfg = match cli::parse(&args) {
        Outcome::Run(cfg) => *cfg,
        Outcome::Exit(code) => return std::process::ExitCode::from(code as u8),
    };

    init_logging(cfg.debug_level);

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("ttyd: cannot start the async runtime: {e}");
            return std::process::ExitCode::from(1);
        }
    };

    match runtime.block_on(run(cfg)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            // Not `tracing::error!`: `-d 0` installs no subscriber at all, so the process
            // would exit 1 having said nothing about why.
            eprintln!("ttyd: {e:#}");
            std::process::ExitCode::from(1)
        }
    }
}

async fn run(cfg: cli::Config) -> anyhow::Result<()> {
    // rustls needs a process-wide crypto provider before any TLS configuration is built.
    let _ = rustls::crypto::ring::default_provider().install_default();

    tracing::info!("ttyd {} (rust)", cli::VERSION);
    for line in cli::config_summary(&cfg) {
        tracing::info!("{line}");
    }
    if !cfg.writable {
        tracing::warn!("The --writable option is not set, will start in readonly mode");
    }

    let tls = if cfg.ssl {
        Some(serve::tls_config(&cfg)?)
    } else {
        None
    };

    let listener = serve::bind(&cfg).await?;
    let port = listener.port();

    // Privileges are dropped only after the socket exists, so a low port can still be bound.
    serve::drop_privileges(&cfg)?;

    if cfg.is_unix_socket() {
        tracing::info!(
            " Listening on unix socket: {}",
            cfg.iface.as_deref().unwrap_or("")
        );
    } else {
        tracing::info!(" Listening on port: {port}");
    }

    if cfg.browser {
        let scheme = if cfg.ssl { "https" } else { "http" };
        utils::open_uri(&format!("{scheme}://localhost:{port}"));
    }

    let cfg = Arc::new(cfg);
    let auth = Authenticator::new(cfg.auth.clone())?;
    let state = AppState::new(cfg, auth);
    let router = http::router(state.clone());

    tokio::spawn(watch_signals(state.clone()));

    serve::run(state, router, listener, tls).await;
    Ok(())
}

/// Reproduces the C signal handling: the first INT/TERM stops the server, a second one
/// terminates immediately.
async fn watch_signals(state: Arc<AppState>) {
    use tokio::signal::unix::{signal, SignalKind};

    let Ok(mut interrupt) = signal(SignalKind::interrupt()) else {
        return;
    };
    let Ok(mut terminate) = signal(SignalKind::terminate()) else {
        return;
    };

    let name = tokio::select! {
        _ = interrupt.recv() => "SIGINT",
        _ = terminate.recv() => "SIGTERM",
    };
    tracing::info!("received signal: {name}, exiting...");
    state.set_force_exit();
    state.begin_shutdown();
    tracing::info!("send ^C to force exit.");

    // Live sessions now have a moment to signal their child processes and let go of the
    // terminal. A second signal in that window gives up waiting, as the C version does.
    tokio::select! {
        _ = interrupt.recv() => std::process::exit(1),
        _ = terminate.recv() => std::process::exit(1),
        _ = wait_for_sessions_to_end(&state) => {}
    }
    std::process::exit(0);
}

/// Terminates the running terminals and gives them a moment to go away.
///
/// The children are signalled here rather than left to each session task: a task that is
/// not scheduled in time would otherwise leave its terminal running after the server is
/// gone, and because each child leads its own process group nothing else would reap it.
async fn wait_for_sessions_to_end(state: &AppState) {
    let signalled = state.signal_children(state.cfg.sig_code);
    if signalled > 0 {
        tracing::info!("signalled {signalled} running terminal(s)");
    }

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    while state.client_count() > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    // Give the child processes a beat to actually die after being signalled.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
}

/// Maps the libwebsockets log-level bitmask that `-d` accepts onto tracing levels.
fn init_logging(debug_level: i32) {
    let level = match debug_level {
        l if l & 16 != 0 => tracing::Level::TRACE,
        l if l & 8 != 0 => tracing::Level::DEBUG,
        l if l & 4 != 0 => tracing::Level::INFO,
        l if l & 2 != 0 => tracing::Level::WARN,
        l if l & 1 != 0 => tracing::Level::ERROR,
        _ => return,
    };

    let _ = tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
}
