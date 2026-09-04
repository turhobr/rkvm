mod client;
mod config;
mod tls;

use clap::Parser;
use config::Config;
use rkvm_net::clipboard;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};
use tokio::{fs, signal, time};
use tracing::subscriber;
use tracing_subscriber::filter::{EnvFilter, LevelFilter};
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

const RECONNECT_MIN: Duration = Duration::from_millis(500);
const RECONNECT_MAX: Duration = Duration::from_secs(5);

#[derive(Parser)]
#[structopt(name = "rkvm-client", about = "The rkvm client application")]
struct Args {
    #[clap(help = "Path to configuration file")]
    config_path: PathBuf,
}

#[tokio::main]
async fn main() -> ExitCode {
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();

    let registry = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().without_time());

    subscriber::set_global_default(registry).unwrap();

    let args = Args::parse();
    let config = match fs::read_to_string(&args.config_path).await {
        Ok(config) => config,
        Err(err) => {
            tracing::error!("Error reading config: {}", err);
            return ExitCode::FAILURE;
        }
    };

    if let Ok(metadata) = fs::metadata(&args.config_path).await {
        if metadata.permissions().mode() & 0o077 != 0 {
            tracing::warn!(
                "Config file {:?} is accessible to other users, it contains the password",
                args.config_path
            );
        }
    }

    let config = match toml::from_str::<Config>(&config) {
        Ok(config) => config,
        Err(err) => {
            tracing::error!("Error parsing config: {}", err);
            return ExitCode::FAILURE;
        }
    };

    let connector = match tls::configure(&config.certificate).await {
        Ok(connector) => connector,
        Err(err) => {
            tracing::error!("Error configuring TLS: {}", err);
            return ExitCode::FAILURE;
        }
    };

    let (mut changes, applier) = clipboard::new(config.clipboard.clone());

    let run = async {
        let mut delay = RECONNECT_MIN;

        loop {
            let start = Instant::now();
            let result = client::run(&config, connector.clone(), &mut changes, &applier).await;

            match result {
                Ok(()) => tracing::info!("Disconnected"),
                Err(err) => tracing::error!("Error: {}", err),
            }

            if start.elapsed() >= RECONNECT_MAX {
                delay = RECONNECT_MIN;
            }

            tracing::info!("Reconnecting in {:?}", delay);
            time::sleep(delay).await;

            delay = (delay * 2).min(RECONNECT_MAX);
        }
    };

    tokio::select! {
        _ = run => {}
        // This is needed to properly clean libevdev stuff up.
        result = signal::ctrl_c() => {
            if let Err(err) = result {
                tracing::error!("Error setting up signal handler: {}", err);
                return ExitCode::FAILURE;
            }

            tracing::info!("Exiting on signal");
        }
    }

    ExitCode::SUCCESS
}
