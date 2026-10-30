use std::path::PathBuf;

use anyhow::{Result, bail};
use baocode_relay::{Relay, config::Config};
use clap::Parser;
use tracing_subscriber::{EnvFilter, prelude::*};

/// BaoCode's iroh relay.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// The configuration file (see deploy/relay.toml).
    #[arg(long, short)]
    config: Option<PathBuf>,
    /// Plain HTTP on 127.0.0.1, everyone admitted, no QUIC address
    /// discovery: for trying things out on one machine.
    #[arg(long)]
    dev: bool,
    /// The port for --dev.
    #[arg(long, default_value_t = 3340)]
    dev_port: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cli = Cli::parse();
    let config = match (&cli.config, cli.dev) {
        (_, true) => Config::dev(cli.dev_port),
        (Some(path), false) => Config::read(path)?,
        (None, false) => bail!("--config <file> or --dev"),
    };
    let path = if cli.dev { None } else { cli.config.clone() };
    let mut relay = Relay::spawn(config, path).await?;
    tokio::select! {
        biased;
        _ = shutdown_signal() => tracing::info!("stopping"),
        _ = relay.join() => bail!("the relay stopped on its own"),
    }
    relay.shutdown().await;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
