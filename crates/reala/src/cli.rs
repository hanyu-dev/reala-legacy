//! Command line interface.

use std::sync::Arc;

use anyhow::Result;
use uni_addr::UniAddr;

use crate::config::{Config, DEFAULT_CONFIG_FILE_PATH, Endpoint};
use crate::util;

#[derive(Debug, Clone)]
#[derive(clap::Parser)]
#[command(version = util::VERSION)]
/// The command line interface.
pub(crate) struct Cli {
    #[clap(short, long, default_value = DEFAULT_CONFIG_FILE_PATH)]
    /// The path to the config file.
    ///
    /// By default, we read the config from the `config.toml` file under the
    /// current working directory.
    pub config: Arc<str>,

    #[clap(subcommand)]
    /// See [`Command`].
    pub command: Option<Command>,
}

#[derive(Debug, Clone)]
#[derive(clap::Subcommand)]
/// The supported commands.
pub(crate) enum Command {
    /// Edits the config endpoints.
    ///
    /// When auto-reloading is enabled, the server will pick up the changes
    /// automatically, or you may manually trigger a reload.
    Endpoint {
        #[clap(subcommand)]
        /// See [`EndpointCommand`].
        cmd: EndpointCommand,
    },
}

impl Command {
    /// Handles the command input by the user.
    ///
    /// Returns whether should the program exit after handling the command.
    pub(crate) async fn handle(self, config: &str) -> Result<CommandHandled> {
        match self {
            Command::Endpoint { cmd } => {
                let mut config = Config::load(Some(config), true).await?;

                match cmd {
                    EndpointCommand::Add {
                        endpoint,
                        overwrite,
                    } => {
                        config.endpoints.add(endpoint, overwrite)?;
                    }
                    EndpointCommand::Delete { listen, strict } => {
                        config.endpoints.delete(&listen, strict)?;
                    }
                }

                // The server will automatically pick up the changes in the config file.
                config.save().await?;

                Ok(CommandHandled::Exit)
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
/// The result handling a command.
pub(crate) enum CommandHandled {
    /// The program should exit.
    Exit,

    #[allow(dead_code)]
    /// The program should continue running.
    Continue,
}

#[derive(Debug, Clone)]
#[derive(clap::Subcommand)]
/// The supported commands.
pub(crate) enum EndpointCommand {
    /// Adds a relay endpoint at runtime.
    ///
    /// This will fail if an endpoint with the same [`Endpoint::listen`] address
    /// already exists.
    Add {
        #[clap(flatten)]
        endpoint: Endpoint,

        #[clap(long, default_value_t = false)]
        /// Whether to overwrite an existing endpoint with the same
        /// [`Endpoint::listen`] address.
        ///
        /// Defaults to `false`.
        overwrite: bool,
    },

    /// Deletes a relay endpoint at runtime.
    ///
    /// This will fail if the specified endpoint does not exist.
    Delete {
        #[clap(short, long)]
        /// The address to listen on.
        listen: UniAddr,

        #[clap(long, default_value_t = true)]
        /// Whether to enforce that the endpoint must exist, or return an error.
        ///
        /// Defaults to `true`.
        strict: bool,
    },
}
