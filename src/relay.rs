//! Spawn relay tasks.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;

use tokio::task::JoinSet;
use tokio_uni_stream::{OwnedReadHalf, OwnedWriteHalf, UniListener, UniStream};
use uni_addr::UniAddr;

use crate::config::{Config, Endpoint};

/// The relay server.
pub(crate) struct Server {
    endpoint: Endpoint,
}

impl Server {
    /// Spawns relay tasks based on the given config, and aborts any previously
    /// running tasks.
    ///
    /// The existing connections will not be affected.
    pub(crate) async fn spawn(config: Config) {
        static RUNNING: LazyLock<Mutex<Option<JoinSet<io::Result<()>>>>> =
            LazyLock::new(Default::default);

        let _running = RUNNING.lock().expect("Lock poisoned").take();

        let mut running = JoinSet::new();

        tracing::info!("Spawning {} relay endpoint(s)", config.endpoints.len());

        for endpoint in config.endpoints {
            running.spawn(Self { endpoint }.execute());
        }

        tracing::info!("Spawned all relay endpoint(s)");

        *RUNNING.lock().expect("Lock poisoned") = Some(running);
    }

    #[tracing::instrument(
        level = "DEBUG",
        name = "Server::execute",
        skip(self),
        fields(
            listen = %self.endpoint.listen,
            target = %self.endpoint.target
        ),
        err
    )]
    async fn execute(self) -> io::Result<()> {
        let accepting = UniListener::bind(&self.endpoint.listen).await?;

        loop {
            let (accepted, peer_addr) = accepting.accept().await?;

            tokio::spawn(Self::relay(
                self.endpoint.listen.clone(),
                peer_addr,
                accepted,
                self.endpoint.target.clone(),
                self.endpoint.options.splice,
            ));
        }
    }

    #[tracing::instrument(level = "DEBUG", name = "Server::relay", skip(accepted), err)]
    async fn relay(
        local_addr: UniAddr,
        peer_addr: UniAddr,
        accepted: UniStream,
        target: UniAddr,
        splice: bool,
    ) -> io::Result<()> {
        let now = Instant::now();

        tracing::info!(
            elapsed = ?now.elapsed(),
            "Accepted connection from {}, local address is {}",
            peer_addr,
            local_addr
        );

        let connected = UniStream::connect(&target).await?;

        tracing::debug!(elapsed = ?now.elapsed(), "Connection fully established, spawn task to relay traffic");

        let ret = Self::copy_bidirectional(accepted, connected, splice).await?;

        const FORMATTER: humat::Formatter<9> = humat::Formatter::BINARY.with_custom_unit("B");

        tracing::info!(
            elapsed = ?now.elapsed(),
            tx = %FORMATTER.format(ret.tx),
            rx = %FORMATTER.format(ret.rx),
            "Connection closed",
        );

        Ok(())
    }

    #[cfg_attr(not(unix), allow(unused_mut))]
    async fn copy_bidirectional(
        mut accepted: UniStream,
        mut connected: UniStream,
        splice: bool,
    ) -> io::Result<TrafficResult> {
        if splice {
            #[cfg(unix)]
            {
                match tokio_splice2::copy_bidirectional(&mut accepted, &mut connected).await {
                    Ok(tokio_splice2::traffic::TrafficResult {
                        tx: 0,
                        rx: 0,
                        error: Some(error),
                    }) => {
                        tracing::warn!("`splice(2)` may be not supported: {error:?}, falling back");
                    }
                    Ok(tokio_splice2::traffic::TrafficResult { tx, rx, error }) => {
                        return Ok(TrafficResult {
                            tx: tx as u64,
                            rx: rx as u64,
                            error,
                        });
                    }
                    Err(e) => {
                        tracing::warn!("`splice(2)` error: {e:?}, falling back");
                    }
                }
            }

            #[cfg(not(unix))]
            {
                tracing::warn!(
                    "`splice` option is enabled, but the current platform does not support, \
                     falling back"
                );
            }
        }

        let (accepted_r, accepted_w) = accepted.into_split();
        let (connected_r, connected_w) = connected.into_split();

        let tx = tokio::spawn(Self::copy_unidirectional::<true, false>(
            accepted_r,
            connected_w,
        ));

        let rx = tokio::spawn(Self::copy_unidirectional::<false, true>(
            connected_r,
            accepted_w,
        ));

        match tokio::try_join!(tx, rx) {
            Ok((tx, rx)) => {
                let merged = tx.merge(rx);

                Ok(merged)
            }
            Err(e) => {
                tracing::error!("`copy_bidirectional` task error: {e}");
                Err(io::Error::other(e))
            }
        }
    }

    async fn copy_unidirectional<const TX: bool, const RX: bool>(
        mut r: OwnedReadHalf,
        mut w: OwnedWriteHalf,
    ) -> TrafficResult {
        let amt = Arc::new(AtomicU64::new(0));

        let error = crate::util::copy(&mut r, &mut w, amt.clone()).await.err();

        TrafficResult {
            tx: if TX { amt.load(Ordering::Relaxed) } else { 0 },
            rx: if RX { amt.load(Ordering::Relaxed) } else { 0 },
            error,
        }
    }
}

#[derive(Debug)]
struct TrafficResult {
    tx: u64,
    rx: u64,
    error: Option<io::Error>,
}

impl TrafficResult {
    fn merge(mut self, other: Self) -> Self {
        self.tx += other.tx;
        self.rx += other.rx;

        if self.error.is_none() {
            self.error = other.error;
        }

        self
    }
}
