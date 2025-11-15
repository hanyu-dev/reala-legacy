#![doc = include_str!("../README.md")]

mod config;
mod relay;
mod util;

use anyhow::Result;

use crate::config::Config;
use crate::relay::Server;

// Use mimalloc.
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main]
async fn main() -> Result<()> {
    // The tracing-subscriber initialization.
    {
        use tracing_subscriber::filter::LevelFilter;
        use tracing_subscriber::fmt::time::ChronoLocal;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        use tracing_subscriber::{EnvFilter, Layer};

        let fmt_layer = tracing_subscriber::fmt::layer()
            .pretty()
            .with_timer(ChronoLocal::rfc_3339())
            .with_filter(
                EnvFilter::builder()
                    .with_default_directive(LevelFilter::DEBUG.into())
                    .from_env_lossy(),
            );

        tracing_subscriber::registry().with(fmt_layer).init();
    }

    tracing::info!("{} built at {}", util::VERSION, util::BUILD_TIME);

    // Loads the initial config.
    {
        let config = Config::load(None).await?;

        Server::spawn(config).await;
    }

    tokio::select! {
        biased;
        _ = signal_handler() => {
            Ok(())
        },
    }
}

async fn signal_handler() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut sigterm = signal(SignalKind::terminate()).expect("Failed to create SIGTERM signal");
        let mut sigint = signal(SignalKind::interrupt()).expect("Failed to create SIGINT signal");
        let mut sighup = signal(SignalKind::hangup()).expect("Failed to create SIGHUP signal");

        loop {
            tokio::select! {
                    _ = sigterm.recv() => {
                        tracing::info!("Received SIGTERM, shutting down...");
                        break;
                    }
                    _ = sigint.recv() => {
                        tracing::info!("Received SIGINT, shutting down...");
                        break;
                    }
                    _ = sighup.recv() => {
                        tracing::info!("Received SIGHUP, reloading configuration...");

                        match Config::load(None).await {
                            Ok(config) => {
                                Server::spawn(config).await;

                                tracing::info!("Configuration reloaded successfully.");
                            }
                            Err(e) => {
                                tracing::error!("Failed to reload configuration: {:?}", e);
                            }
                        }
                    }
            }
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to listen for Ctrl-C");
        tracing::info!("Received Ctrl-C, shutting down...");
    }
}
