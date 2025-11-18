//! Spawn relay tasks.

use std::io;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use portable_atomic::{AtomicU64, AtomicUsize, Ordering};
use quanta::Instant;
use tokio::task::JoinSet;
use tokio_uni_stream::{OwnedReadHalf, OwnedWriteHalf, UniSocket, UniStream};
use tracing::Span;
use uni_addr::UniAddr;

#[cfg(target_os = "linux")]
use crate::config::ENABLE_ZERO_COPY;
use crate::config::{Config, Endpoint};

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

        // Will be dropped after this function returns. We have enabled SO_REUSEADDR and
        // SO_REUSEPORT.
        let _running = RUNNING.lock().expect("Lock poisoned").take();

        let mut running = JoinSet::new();

        for endpoint in config.endpoints.into_iter() {
            running.spawn(Self { endpoint }.execute());
        }

        tracing::info!(
            "Server fully (re)started, running {} endpoint(s).",
            running.len()
        );

        *RUNNING.lock().expect("Lock poisoned") = Some(running);
    }

    #[tracing::instrument(
        level = "DEBUG",
        name = "Server::execute",
        skip(self),
        fields(
            listen = %self.endpoint.listen,
            target = %self.endpoint.remote
        ),
        err
    )]
    async fn execute(self) -> io::Result<()> {
        let accepting = {
            let accepting = UniSocket::bind(&self.endpoint.listen)?;

            // Apply socket options before listening, these options will be inherited
            // by all accepted connections
            self.endpoint.apply_socket_options_listener(&accepting)?;

            accepting.listen(128)?
        };

        // Spawn connection handlers
        let (accepted_conn_channel_tx, accepted_conn_channel_rx) =
            crossfire::mpmc::bounded_async(*CONN_HANDLERS_MAX);

        let mut ctx = ConnHandlerCtx::new(accepted_conn_channel_rx, Arc::new(self.endpoint)).await;

        // TODO: add more listening loop?
        loop {
            let accepted = accepting.accept().await?;

            let timer = Instant::now();

            tokio::join!(
                biased;
                async {
                    match accepted_conn_channel_tx.send(accepted).await {
                        Ok(_) => {
                            tracing::debug!(
                                elapsed = ?timer.elapsed(),
                                "Sent accepted connection to handlers"
                            );
                        }
                        Err(_) => {
                            unreachable!(
                                "Rare bug: there must be at least one receiver for accepted \
                                 connections"
                            );
                        }
                    }
                },
                ctx.reconcile()
            );

            tracing::debug!(
                elapsed = ?timer.elapsed(),
                "Accepting next connection"
            );
        }
    }
}

struct ConnHandlerCtx {
    /// The channel receiver for accepted connections.
    accepted_conn_channel_rx: crossfire::MAsyncRx<(UniStream, UniAddr)>,

    /// All running connection handlers for this endpoint.
    handlers: JoinSet<()>,

    /// The base handler to clone from.
    handler: ConnHandler,
}

impl ConnHandlerCtx {
    async fn new(
        accepted_conn_channel_rx: crossfire::MAsyncRx<(UniStream, UniAddr)>,
        endpoint: Arc<Endpoint>,
    ) -> Self {
        Self {
            accepted_conn_channel_rx,
            handlers: JoinSet::new(),
            handler: ConnHandler {
                endpoint,
                running: Arc::new(AtomicUsize::new(0)),
            },
        }
    }

    async fn reconcile(&mut self) {
        while self.handlers.len() < *CONN_HANDLERS_MIN {
            self.handlers.spawn(
                self.handler
                    .clone()
                    .execute(self.accepted_conn_channel_rx.clone()),
            );
        }

        // Scale up if there are queued connections.
        if self.accepted_conn_channel_rx.len() > 0 && self.handlers.len() <= *CONN_HANDLERS_MAX {
            self.handlers.spawn(
                self.handler
                    .clone()
                    .execute(self.accepted_conn_channel_rx.clone()),
            );

            tracing::debug!(
                running = self.handlers.len(),
                queued = self.accepted_conn_channel_rx.len(),
                "Scaled up connection handlers"
            );
        }

        // Clean up finished handlers.
        if self.accepted_conn_channel_rx.len() == 0 {
            let mut finished: usize = 0;

            while let Some(ret) = self.handlers.try_join_next() {
                if let Err(e) = ret {
                    // Only when the handler panic will return error here.
                    tracing::error!("Connection handler task panicked: {e}");

                    // TODO: exits or just ignore?
                }

                finished += 1;
            }

            if finished > 0 {
                tracing::debug!(
                    running = self.handlers.len(),
                    queued = self.accepted_conn_channel_rx.len(),
                    "Cleaned up {finished} exited connection handlers"
                );
            }
        }
    }
}

static CONN_HANDLERS_MIN: LazyLock<usize> = LazyLock::new(|| num_cpus::get().saturating_mul(2));
static CONN_HANDLERS_MAX: LazyLock<usize> = LazyLock::new(|| num_cpus::get().saturating_mul(16));

struct ConnHandler {
    /// The endpoint configuration.
    endpoint: Arc<Endpoint>,

    /// All available conn handlers for this endpoint.
    running: Arc<AtomicUsize>,
}

impl Clone for ConnHandler {
    fn clone(&self) -> Self {
        self.running.fetch_add(1, Ordering::AcqRel);

        ConnHandler {
            endpoint: self.endpoint.clone(),
            running: self.running.clone(),
        }
    }
}

impl Drop for ConnHandler {
    fn drop(&mut self) {
        tracing::debug!(
            listen = %self.endpoint.listen,
            remote = %self.endpoint.remote,
            "Connection handler dropped, remaining: {}",
            self.running.fetch_sub(1, Ordering::AcqRel)
        );
    }
}

impl ConnHandler {
    #[tracing::instrument(
        level = "DEBUG",
        name = "ConnHandler::execute",
        skip_all,
        fields(
            listen = %self.endpoint.listen,
            target = %self.endpoint.remote
        ),
    )]
    async fn execute(self, accepted_conn_channel_rx: crossfire::MAsyncRx<(UniStream, UniAddr)>) {
        let mut preconnected: Option<(UniStream, Instant)> = None;

        loop {
            let accepted = tokio::select! {
                biased;
                accepted = accepted_conn_channel_rx.recv() => {
                    accepted
                }
                _ = tokio::time::sleep(Duration::from_secs(30)) => {
                    let running = self.running.load(Ordering::Acquire);

                    if running > *CONN_HANDLERS_MIN {
                        tracing::debug!(
                            running = running,
                            "Idle for more than 30s, handler exiting to scale down"
                        );

                        return;
                    } else {
                        continue;
                    }
                }
            };

            let Ok(accepted) = accepted else {
                tracing::debug!("Channel of accepted connections is closed, handler exiting");

                return;
            };

            tokio::spawn(
                Running {
                    span: Span::current(),
                    endpoint: self.endpoint.clone(),
                }
                .execute(accepted, preconnected.take()),
            );

            tracing::debug!("Spawned new task for accepted connection");

            // Connects to the remote in advance, during which the other handlers will
            // continue to accept new connections
            if self.endpoint.conf.connect_enable_preconnect
                && !self.endpoint.conf.listen_proxy_protocol_v2
            {
                let timer = Instant::now();

                match UniStream::connect(&self.endpoint.remote)
                    .await
                    .and_then(|connected| {
                        self.endpoint.apply_socket_options_connected(&connected)?;

                        Ok(connected)
                    }) {
                    Ok(connected) => {
                        preconnected = Some((connected, Instant::now()));

                        tracing::debug!(elapsed = ?timer.elapsed(), "Connected to the remote (preconnect)");
                    }
                    Err(e) => {
                        tracing::error!(elapsed = ?timer.elapsed(), "Failed to connect to the remote: {e}");
                    }
                };
            }
        }
    }
}

/// The running task for each accepted connection.
struct Running {
    span: Span,
    endpoint: Arc<Endpoint>,
}

impl Running {
    #[tracing::instrument(
        level = "DEBUG",
        name = "Running::execute",
        parent = &self.span,
        skip_all,
        fields(
            peer_addr = %peer_addr,
        ),
        err
    )]
    async fn execute(
        self,
        (accepted, peer_addr): (UniStream, UniAddr),
        mut preconnected: Option<(UniStream, Instant)>,
    ) -> io::Result<()> {
        let timer = Instant::now();

        let connected = {
            let connecting = async {
                match UniStream::connect(&self.endpoint.remote)
                    .await
                    .and_then(|connected| {
                        self.endpoint.apply_socket_options_connected(&connected)?;

                        Ok(connected)
                    }) {
                    ok @ Ok(_) => {
                        tracing::debug!(
                            elapsed = ?timer.elapsed(),
                            "Connected to the remote"
                        );

                        ok
                    }
                    Err(e) => {
                        tracing::error!(
                            elapsed = ?timer.elapsed(),
                            "Failed to connect to the remote, aborting the accepted connection: {e}"
                        );

                        Err(e)
                    }
                }
            };

            if self.endpoint.conf.listen_proxy_protocol_v2 {
                // TODO: read PROXY Protocol header here.
                let (_proxy_protocol_header, connected) =
                    tokio::try_join!(async { Ok(()) }, connecting)?;

                connected
            } else {
                match preconnected
                    .take_if(|(_, created)| created.elapsed() < Duration::from_secs(15))
                {
                    Some((preconnected, created)) => {
                        tracing::debug!(
                            elapsed = ?timer.elapsed(),
                            "Using preconnected connection established {:?} ago",
                            created.elapsed()
                        );

                        preconnected
                    }
                    None => connecting.await?,
                }
            }
        };

        tracing::debug!(
            elapsed = ?timer.elapsed(),
            "Connection fully established, start to relay traffic"
        );

        let TrafficResult { tx, rx, error } = Self {
            span: Span::current(),
            endpoint: self.endpoint,
        }
        .copy_bidirectional(accepted, connected)
        .await?;

        // TODO: collect metrics here
        {
            const FORMATTER: humat::Formatter<9> = humat::Formatter::BINARY.with_custom_unit("B");

            tracing::info!(
                elapsed = ?timer.elapsed(),
                tx = %FORMATTER.format(tx),
                rx = %FORMATTER.format(rx),
                "Connection closed"
            );
        }

        if let Some(error) = error {
            Err(error)
        } else {
            Ok(())
        }
    }

    #[cfg_attr(not(unix), allow(unused_mut))]
    #[tracing::instrument(
        level = "DEBUG",
        name = "Running::copy_bidirectional",
        parent = &self.span,
        skip_all,
        err
    )]
    async fn copy_bidirectional(
        self,
        mut accepted: UniStream,
        mut connected: UniStream,
    ) -> io::Result<TrafficResult> {
        #[cfg(target_os = "linux")]
        if *ENABLE_ZERO_COPY {
            match tokio_splice2::copy_bidirectional(&mut accepted, &mut connected).await {
                Ok(tokio_splice2::traffic::TrafficResult {
                    tx: 0,
                    rx: 0,
                    error: Some(error),
                }) if matches!(error.kind(), io::ErrorKind::InvalidInput) => {
                    tracing::warn!("`splice(2)` may not be supported: {error:?}, falling back");
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

        let (accepted_r, accepted_w) = accepted.into_split();
        let (connected_r, connected_w) = connected.into_split();

        // TODO: to spawn or not here?
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

        let error = crate::util::io::copy(&mut r, &mut w, amt.clone())
            .await
            .err();

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
