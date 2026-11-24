//! `oxrtc -config <path>`: the media gateway.
//!
//! Exit codes: 0 after a signal-driven stop, 1 when the server cannot start
//! (configuration, a port in use), 2 for a usage error.
#![forbid(unsafe_code)]

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use oxrtc::cli::{self, Command};
use oxrtc::logging::{self, LogHandle};
use oxrtc::{App, Launch, VERSION};

fn main() -> ExitCode {
    let args: Result<Vec<String>, _> = std::env::args_os().skip(1).map(|a| a.into_string()).collect();
    let Ok(args) = args else {
        eprintln!("oxrtc: an argument is not valid UTF-8\n{}", cli::USAGE);
        return ExitCode::from(2);
    };
    let path = match cli::parse(args) {
        Ok(Command::Version) => {
            println!("{}", cli::version_line(VERSION));
            return ExitCode::SUCCESS;
        }
        Ok(Command::Run(path)) => path,
        Err(error) => {
            eprintln!("oxrtc: {error}\n{}", cli::USAGE);
            return ExitCode::from(2);
        }
    };
    let logs = match logging::init() {
        Ok(logs) => logs,
        Err(error) => {
            eprintln!("oxrtc: cannot install the logger: {error}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!("cannot start the runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    let code = runtime.block_on(run(&path, &logs));
    // Nothing left to wait for once the servers are down; with the 4 s task
    // deadline the stop stays inside the 5 s openIVR gives before a kill.
    runtime.shutdown_timeout(Duration::from_millis(500));
    code
}

async fn run(path: &Path, logs: &LogHandle) -> ExitCode {
    tracing::info!(version = VERSION, platform = std::env::consts::OS, arch = std::env::consts::ARCH, "oxrtc");
    let config = match oxrtc_api::config::load(path) {
        Ok(config) => config,
        Err(error) => {
            // A YAML error may quote the line it failed on.
            tracing::error!("config: {}", oxrtc_core::spec::redact(&error.to_string()));
            return ExitCode::FAILURE;
        }
    };
    logs.set_level(&config.log.level);
    // As go2rtc names it on `GET /api`: absolute.
    let launch = Launch { config_path: Some(std::path::absolute(path).unwrap_or_else(|_| path.to_owned())) };
    let app = match App::start_with(&config, std::env::var_os("OXRTC_FFMPEG"), launch).await {
        Ok(app) => app,
        Err(error) => {
            tracing::error!("{error}");
            return ExitCode::FAILURE;
        }
    };
    stop_signal().await;
    tracing::info!("stopping");
    app.shutdown().await;
    tracing::info!("stopped");
    ExitCode::SUCCESS
}

/// Waits for Ctrl+C, or SIGTERM on Unix (Ctrl+Break, console close and
/// system shutdown on Windows). If no signal can be listened to, waits
/// forever: stopping at once would be worse than needing a kill.
async fn stop_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!("cannot listen for Ctrl+C: {error}");
            std::future::pending::<()>().await;
        }
    };
    tokio::select! {
        () = ctrl_c => {}
        () = platform_signal() => {}
    }
}

#[cfg(unix)]
async fn platform_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::terminate()) {
        Ok(mut term) => {
            term.recv().await;
        }
        Err(error) => {
            tracing::error!("cannot listen for SIGTERM: {error}");
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(windows)]
async fn platform_signal() {
    use tokio::signal::windows;
    let (Ok(mut brk), Ok(mut close), Ok(mut shutdown)) =
        (windows::ctrl_break(), windows::ctrl_close(), windows::ctrl_shutdown())
    else {
        tracing::error!("cannot listen for the console's stop events");
        return std::future::pending::<()>().await;
    };
    tokio::select! {
        _ = brk.recv() => {}
        _ = close.recv() => {}
        _ = shutdown.recv() => {}
    }
}

#[cfg(not(any(unix, windows)))]
async fn platform_signal() {
    std::future::pending::<()>().await;
}
