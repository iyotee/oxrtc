//! `oxrtc -config <path>`: the media gateway.
//!
//! Exit codes: 0 after a signal-driven stop, 1 when the server cannot start
//! (configuration, a port in use), 2 for a usage error, the code
//! `POST /api/exit?code=` gave (0 to 125) when the API asked for it.
#![forbid(unsafe_code)]

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use oxrtc::cli::{self, Command};
use oxrtc::logging::{self, LogHandle};
use oxrtc::{App, Launch, VERSION};
use oxrtc_api::Command as ApiCommand;

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

/// Runs the server until a signal, or until `POST /api/exit`; `POST
/// /api/restart` stops it and starts it again from the file, in this process.
/// A restart that cannot start (the file changed since the route checked it, a
/// port another program holds) starts again from the configuration that ran
/// before, so that the API asked to restart is still there to answer (go2rtc
/// goes on without a listener it cannot bind).
async fn run(path: &Path, logs: &LogHandle) -> ExitCode {
    tracing::info!(version = VERSION, platform = std::env::consts::OS, arch = std::env::consts::ARCH, "oxrtc");
    // As go2rtc names it on `GET /api`: absolute.
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_owned());
    // One command at a time: a second while the first is acted on waits.
    let (commands, mut received) = tokio::sync::mpsc::channel(1);
    // The configuration the running server started from, once one has.
    let mut ran: Option<oxrtc_api::config::Config> = None;
    loop {
        let launch =
            || Launch { config_path: Some(path.clone()), logs: Some(logs.memory()), commands: Some(commands.clone()) };
        let started = match oxrtc_api::config::load(&path) {
            Ok(config) => start(config, logs, launch()).await,
            // A YAML error may quote the line it failed on.
            Err(error) => Err(format!("config: {}", oxrtc_core::spec::redact(&error.to_string()))),
        };
        let (config, app) = match (started, ran.take()) {
            (Ok(started), _) => started,
            (Err(error), None) => {
                tracing::error!("{error}");
                return ExitCode::FAILURE;
            }
            (Err(error), Some(previous)) => {
                tracing::error!("{error}: starting again from the configuration that ran before");
                match start(previous, logs, launch()).await {
                    Ok(started) => started,
                    Err(error) => {
                        tracing::error!("{error}");
                        return ExitCode::FAILURE;
                    }
                }
            }
        };
        ran = Some(config);
        let command = tokio::select! {
            () = stop_signal() => None,
            command = received.recv() => command,
        };
        tracing::info!("stopping");
        app.shutdown().await;
        tracing::info!("stopped");
        match command {
            Some(ApiCommand::Restart) => tracing::info!("restarting from {}", path.display()),
            Some(ApiCommand::Exit(code)) => return ExitCode::from(code),
            None => return ExitCode::SUCCESS,
        }
    }
}

/// Starts the server from `config`, its log level first; the configuration
/// is handed back with the running server.
async fn start(
    config: oxrtc_api::config::Config,
    logs: &LogHandle,
    launch: Launch,
) -> Result<(oxrtc_api::config::Config, App), String> {
    logs.set_level(&config.log.level);
    match App::start_with(&config, std::env::var_os("OXRTC_FFMPEG"), launch).await {
        Ok(app) => Ok((config, app)),
        Err(error) => Err(error.to_string()),
    }
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
