#![doc = include_str!("../README.md")]
#![cfg_attr(not(debug_assertions), allow(unused))]
#![cfg_attr(not(debug_assertions), allow(dead_code))]

#[cfg(feature = "cli")]
mod cli;
mod config;
mod relay;
mod util;

use anyhow::Result;
#[cfg(feature = "cli")]
use clap::Parser;

#[cfg(feature = "cli")]
use crate::cli::{Cli, CommandHandled};
use crate::config::{CONFIG_FILE_PATH, CONFIG_GLOBAL, Config};
use crate::relay::Server;

// Use mimalloc.
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main]
async fn main() -> Result<()> {
    #[cfg(feature = "cli")]
    let Cli { config, command } = Cli::parse();

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

    #[cfg(feature = "cli")]
    if let Some(command) = command {
        match command.handle(&config).await? {
            CommandHandled::Exit => {
                return Ok(());
            }
            CommandHandled::Continue => {
                // Continue to start the server.
            }
        }
    }

    tracing::info!("Running {} built at {}", util::VERSION, util::BUILD_TIME);

    // Loads the initial config.
    {
        #[cfg(not(feature = "cli"))]
        let config = Config::load(None, false).await?;

        #[cfg(feature = "cli")]
        let config = Config::load(Some(&config), false).await?;

        // Spawns the server.
        Server::spawn(config).await;
    }

    tokio::select! {
        biased;
        _ = handle_termination() => {
            Ok(())
        },
        _ = handle_config_reload() => {
            Ok(())
        }
    }
}

// === Signal Handlers ===

async fn handle_termination() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut sigterm = signal(SignalKind::terminate()).expect("Failed to create SIGTERM signal");
        let mut sigint = signal(SignalKind::interrupt()).expect("Failed to create SIGINT signal");

        tokio::select! {
            biased;
            _ = sigterm.recv() => {
                tracing::info!("Received SIGTERM, shutting down...");
            }
            _ = sigint.recv() => {
                tracing::info!("Received SIGINT, shutting down...");
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

async fn handle_config_reload() {
    use crate::util::file::Changing;

    #[cfg(unix)]
    let mut sighup = {
        use tokio::signal::unix::{SignalKind, signal};

        signal(SignalKind::hangup()).expect("Failed to create SIGHUP signal")
    };

    let mut changing = if CONFIG_GLOBAL
        .load()
        .as_ref()
        .expect("Must have global config initialized")
        .auto_reload
    {
        Changing::new_or_noop(
            CONFIG_FILE_PATH
                .get()
                .expect("Config file path must be set"),
        )
    } else {
        Changing::new_noop()
    };

    loop {
        tokio::select! {
            _ = async {
                #[cfg(unix)]
                {
                    sighup.recv().await
                }

                #[cfg(not(unix))]
                {
                    std::future::pending::<()>().await
                }
            } => {
                tracing::info!("Received SIGHUP, reloading configuration...");
            },
            res = &mut changing => {
                match res {
                    Ok(_) => {
                        tracing::info!("Configuration file changed, reloading...");
                    }
                    Err(e) => {
                        tracing::error!("Failed to watch configuration file changes: {:?}", e);
                        // Stop auto-reloading on error.
                        changing = Changing::new_noop();
                    }
                }
            }
        }

        match Config::load(None, true).await {
            Ok(config) => {
                Server::spawn(config).await;

                tracing::info!("Configuration reloaded successfully.");
            }
            Err(e) => {
                tracing::error!("Malformed configuration, ignored: {:?}", e);
            }
        }
    }
}
