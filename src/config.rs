//! Config & CLI

use std::collections::HashSet;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::Duration;

use arc_swap::ArcSwapOption;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio_uni_stream::{TcpKeepalive, UniSocket, UniStream};
use uni_addr::UniAddr;

/// The default config file path.
pub(crate) const DEFAULT_CONFIG_FILE_PATH: &str = "./config.toml";

/// By default, we utilize `splice(2)` when copying data between TCP sockets on
/// Linux, to improve performance and reduce CPU usage.
///
/// For debugging purposes, you can disable it by setting the
/// `REALA_ENABLE_ZERO_COPY` environment variable to `0` or `false`.
pub(crate) static ENABLE_ZERO_COPY: LazyLock<bool> = LazyLock::new(|| {
    std::env::var("REALA_ENABLE_ZERO_COPY")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(cfg!(unix))
});

/// The path to the config file.
pub(crate) static CONFIG_FILE_PATH: OnceLock<PathBuf> = OnceLock::new();

/// The global config
pub(crate) static CONFIG_GLOBAL: ArcSwapOption<GlobalConf> = ArcSwapOption::const_empty();

#[derive(Debug, Clone)]
#[derive(serde::Serialize, serde::Deserialize)]
/// The main config structure.
pub(crate) struct Config {
    #[serde(default = "current_config_version")]
    /// The config version
    pub version: u8,

    #[serde(default)]
    /// Global configurations.
    pub global: Arc<GlobalConf>,

    #[serde(default)]
    /// A list of relays.
    pub endpoints: EndpointSet,
}

fn current_config_version() -> u8 {
    const CONFIG_VERSION: u8 = 0;

    CONFIG_VERSION
}

impl Config {
    /// Load the config from the config file.
    ///
    /// If `strict` is `true`, errors will be returned when invalid config
    /// is detected and will not attempt to create an example config file.
    pub(crate) async fn load(path: Option<&str>, strict: bool) -> Result<Self, InvalidConfig> {
        let file = fs::read(
            CONFIG_FILE_PATH
                .get_or_init(|| PathBuf::from(path.unwrap_or(DEFAULT_CONFIG_FILE_PATH))),
        )
        .await;

        let this: Config = match file {
            Ok(bytes) => toml::from_slice(&bytes)?,
            Err(e) if !strict && matches!(e.kind(), io::ErrorKind::NotFound) => {
                Self::example().save().await?;

                tracing::info!(
                    "An example config file has been created at {:?}",
                    CONFIG_FILE_PATH
                        .get()
                        .expect("Have initialized config file path")
                        .canonicalize()
                        .expect("The path should be valid")
                );

                return Err(InvalidConfig::Io(e));
            }
            Err(e) => return Err(InvalidConfig::Io(e)),
        };

        // Verify the config.
        this.verify()?;

        // Initialize the global config.
        CONFIG_GLOBAL.store(Some(this.global.clone()));

        Ok(this)
    }

    fn verify(&self) -> Result<(), InvalidConfig> {
        if self.version != current_config_version() {
            return Err(InvalidConfig::InvalidVersion);
        }

        {
            let mut seen = HashSet::with_capacity(self.endpoints.len());
            for listen in self.endpoints.iter().map(|endpoint| &endpoint.listen) {
                if !seen.insert(listen) {
                    return Err(InvalidConfig::EndpointDuplicated(listen.clone()));
                }
            }
        }

        Ok(())
    }

    /// Save the config to the config file.
    pub(crate) async fn save(self) -> Result<(), InvalidConfig> {
        let path = CONFIG_FILE_PATH
            .get()
            .expect("Must have loaded the config before saving it");

        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)
            .await?;

        let serialized = toml::to_string(&self).expect("Must be valid TOML");

        file.write_all(serialized.as_bytes()).await?;

        Ok(())
    }

    fn example() -> Self {
        Config {
            version: current_config_version(),
            global: Arc::new(GlobalConf::default()),
            endpoints: vec![Endpoint {
                listen: "0.0.0.0:8080".parse().unwrap(),
                remote: "127.0.0.1:80".parse().unwrap(),
                conf: EndpointConf::default(),
            }]
            .into(),
        }
    }
}

#[derive(Debug)]
#[derive(thiserror::Error)]
/// Errors related to invalid config.
pub(crate) enum InvalidConfig {
    #[error("Invalid config version")]
    /// The config version is invalid.
    InvalidVersion,

    #[error("Read / write config file error: {0}")]
    /// IO error.
    Io(#[from] io::Error),

    #[error("Parse config file error: {0}")]
    /// TOML parse error.
    ParseError(#[from] toml::de::Error),

    #[error("Found duplicate endpoint listening on {0}")]
    /// Duplicate endpoint found.
    EndpointDuplicated(UniAddr),

    #[error("No endpoint found listening on {0}")]
    /// Endpoint not found.
    EndpointNotFound(UniAddr),
}

#[derive(Debug, Clone, Copy)]
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
/// Global configurations.
pub(crate) struct GlobalConf {
    /// Whether to reload the config file automatically when it changes.
    ///
    /// Defaults to `true`.
    pub auto_reload: bool,
}

impl Default for GlobalConf {
    fn default() -> Self {
        Self { auto_reload: true }
    }
}

wrapper_lite::wrapper!(
    #[wrapper_impl(Debug)]
    #[wrapper_impl(DerefMut)]
    #[wrapper_impl(From)]
    #[derive(Clone, Default)]
    #[derive(serde::Serialize, serde::Deserialize)]
    #[serde(transparent)]
    #[serde(default)]
    /// [`Endpoint`]s
    pub(crate) struct EndpointSet(Vec<Endpoint>);
);

impl IntoIterator for EndpointSet {
    type IntoIter = std::vec::IntoIter<Endpoint>;
    type Item = Endpoint;

    fn into_iter(self) -> Self::IntoIter {
        self.inner.into_iter()
    }
}

impl EndpointSet {
    /// Adds an [`Endpoint`] to the set.
    ///
    /// # Errors
    ///
    /// If `overwrite` is `true`, existing endpoint with the same
    /// [`Endpoint::listen`] address will be overwritten, otherwise an error
    /// will be returned.
    pub(crate) fn add(&mut self, endpoint: Endpoint, overwrite: bool) -> Result<(), InvalidConfig> {
        if overwrite {
            if let Some(found) = self.find_endpoint_mut(&endpoint.listen) {
                *found = endpoint;
            } else {
                self.push(endpoint);
            }
        } else {
            if let Some(found) = self.find_endpoint_mut(&endpoint.listen) {
                return Err(InvalidConfig::EndpointDuplicated(found.listen.clone()));
            }

            self.push(endpoint);
        }

        Ok(())
    }

    fn find_endpoint_mut(&mut self, listen: &UniAddr) -> Option<&mut Endpoint> {
        self.iter_mut().find(|endpoint| &endpoint.listen == listen)
    }

    /// Deletes an [`Endpoint`] from the set.
    ///
    /// # Errors
    ///
    /// If `strict` is `true`, an error will be returned if no such endpoint is
    /// found.
    pub(crate) fn delete(&mut self, listen: &UniAddr, strict: bool) -> Result<(), InvalidConfig> {
        if let Some(pos) = self.iter().position(|endpoint| &endpoint.listen == listen) {
            let removed = self.remove(pos);

            println!("The deleted endpoint:\n------\n{removed:#?}\n------");
        } else {
            tracing::warn!("No endpoint found listening on {listen:?}");

            if strict {
                return Err(InvalidConfig::EndpointNotFound(listen.clone()));
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone)]
#[derive(serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "cli", derive(clap::Args))]
/// An endpoint configuration.
pub(crate) struct Endpoint {
    #[cfg_attr(feature = "cli", clap(short, long))]
    //  === Listening options ===
    /// The address to listen on.
    pub listen: UniAddr,

    // === Remote options ===
    #[serde(alias = "target")]
    #[cfg_attr(feature = "cli", clap(short('t'), long))]
    /// The remote address to forward traffic to.
    pub remote: UniAddr,

    #[serde(flatten)]
    #[cfg_attr(feature = "cli", clap(flatten))]
    /// Endpoint specific configurations.
    pub conf: EndpointConf,
}

#[derive(Debug, Clone)]
#[derive(serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[serde(default)]
/// Endpoint specific configurations.
pub(crate) struct EndpointConf {
    #[cfg_attr(feature = "cli", clap(long, default_value_t = Self::default().listen_enable_tfo))]
    /// Whether to enable TCP Fast Open when accepting TCP connections.
    ///
    /// Defaults to `true`.
    pub listen_enable_tfo: bool,

    #[cfg_attr(feature = "cli", clap(long, default_value_t = Self::default().listen_proxy_protocol_v2))]
    /// Whether to enable PROXY protocol (v2) support when accepting
    /// connections.
    ///
    /// When enabled, the actual target address will be extracted from the
    /// PROXY protocol header instead of using the one from `target` field.
    ///
    /// Defaults to `false`.
    pub listen_proxy_protocol_v2: bool,

    #[cfg_attr(feature = "cli", clap(long, default_value_t = Self::default().listen_require_proxy_protocol_v2))]
    /// Whether we require a PROXY protocol (v2) header when accepting
    /// connections.
    ///
    /// When [`Endpoint::listen_proxy_protocol_v2`] is disabled, this option
    /// does nothing; otherwise, we will reject connections without a valid
    /// PROXY protocol (v2) header.
    ///
    /// Defaults to `true`.
    pub listen_require_proxy_protocol_v2: bool,

    #[cfg_attr(feature = "cli", clap(long, default_value_t = Self::default().accepted_conn_ipv4_tos))]
    /// IP TOS (Type of Service) value for accepted connections.
    ///
    /// This only works for IPv4 sockets.
    ///
    /// - LOWDELAY: 0x10
    /// - THROUGHPUT: 0x08
    /// - RELIABILITY: 0x04
    /// - MINCOST: 0x02
    ///
    /// Defaults to `0`, i.e., do not set.
    pub accepted_conn_ipv4_tos: u32,

    #[cfg_attr(feature = "cli", clap(long, default_value_t = Self::default().accepted_conn_tcp_keep_alive_time))]
    /// The amount of time after which TCP keepalive probes will be sent on idle
    /// connections.
    ///
    /// Set to `0` to disable TCP keep alive.
    ///
    /// Defaults to 15s.
    pub accepted_conn_tcp_keep_alive_time: u64,

    #[cfg_attr(feature = "cli", clap(long, default_value_t = Self::default().accepted_conn_tcp_keep_alive_interval))]
    /// The time interval between TCP keepalive probes.
    ///
    /// Set to `0` to disable TCP keep alive.
    ///
    /// Defaults to 15s.
    pub accepted_conn_tcp_keep_alive_interval: u64,

    #[cfg_attr(feature = "cli", clap(long, default_value_t = Self::default().accepted_conn_tcp_keep_alive_retries))]
    /// The maximum number of TCP keepalive probes that will be sent before
    /// dropping a connection, if TCP keepalive is enabled on this socket.
    ///
    /// Set to `0` to disable TCP keep alive.
    ///
    /// Defaults to 3.
    pub accepted_conn_tcp_keep_alive_retries: u32,

    #[cfg_attr(feature = "cli", clap(long, default_value_t = Self::default().accepted_conn_tcp_no_delay))]
    /// Whether to disable TCP_NODELAY (Nagle's algorithm) for accepted TCP
    /// connections.
    ///
    /// This may decrease latency for small packets, but add more overhead
    /// for TCP connections.
    ///
    /// When zero-copy mode is enabled, this option may have no effect.
    ///
    /// Defaults to `true`.
    pub accepted_conn_tcp_no_delay: bool,

    #[serde(alias = "connecting_enable_preconnect")]
    #[cfg_attr(feature = "cli", clap(long, default_value_t = Self::default().connect_enable_preconnect))]
    /// Whether to enable preconnecting to the [`Endpoint::remote`].
    ///
    /// Note that the [`Endpoint::remote`] should be able to accept multiple
    /// connections simultaneously, and tolerate no application data input for
    /// several seconds without interrupting the connection at the application
    /// layer.
    ///
    /// Defaults to `false`.
    pub connect_enable_preconnect: bool,

    #[serde(alias = "connecting_enable_tfo")]
    #[cfg_attr(feature = "cli", clap(long, default_value_t = Self::default().connect_enable_tfo))]
    /// Whether to enable TCP Fast Open when connecting to the
    /// [`Endpoint::remote`].
    ///
    /// Defaults to `false`.
    pub connect_enable_tfo: bool,

    #[cfg_attr(feature = "cli", clap(long, default_value_t = Self::default().connected_conn_ipv4_tos))]
    /// IP TOS (Type of Service) value for connected connections.
    ///
    /// This only works for IPv4 sockets.
    ///
    /// - LOWDELAY: 0x10
    /// - THROUGHPUT: 0x08
    /// - RELIABILITY: 0x04
    /// - MINCOST: 0x02
    ///
    /// Defaults to `0`, i.e., do not set.
    pub connected_conn_ipv4_tos: u32,

    #[cfg_attr(feature = "cli", clap(long, default_value_t =  Self::default().connected_conn_tcp_keep_alive_time))]
    /// The amount of time after which TCP keepalive probes will be sent on idle
    /// connections.
    ///
    /// Set to `0` to disable TCP keep alive.
    ///
    /// Defaults to `15s`.
    pub connected_conn_tcp_keep_alive_time: u64,

    #[cfg_attr(feature = "cli", clap(long, default_value_t =  Self::default().connected_conn_tcp_keep_alive_interval))]
    /// The time interval between TCP keepalive probes.
    ///
    /// Set to `0` to disable TCP keep alive.
    ///
    /// Defaults to `15s`.
    pub connected_conn_tcp_keep_alive_interval: u64,

    #[cfg_attr(feature = "cli", clap(long, default_value_t =  Self::default().connected_conn_tcp_keep_alive_retries))]
    /// The maximum number of TCP keepalive probes that will be sent before
    /// dropping a connection, if TCP keepalive is enabled on this socket.
    ///
    /// Set to `0` to disable TCP keep alive.
    ///
    /// Defaults to `15s`.
    pub connected_conn_tcp_keep_alive_retries: u32,

    #[cfg_attr(feature = "cli", clap(long, default_value_t = Self::default().connected_conn_tcp_no_delay))]
    /// Whether to disable TCP_NODELAY (Nagle's algorithm) for connected TCP
    /// connections to the [`Endpoint::remote`].
    ///
    /// This may decrease latency for small packets, but add more overhead
    /// for TCP connections.
    ///
    /// Defaults to `true`.
    pub connected_conn_tcp_no_delay: bool,
}

impl Default for EndpointConf {
    fn default() -> Self {
        const DEFAULT_TCP_KEEP_ALIVE_INTERVAL: u64 = 15;
        const DEFAULT_TCP_KEEP_ALIVE_RETRIES: u32 = 3;
        const DEFAULT_TCP_KEEP_ALIVE_TIME: u64 = 15;

        Self {
            listen_enable_tfo: true,
            listen_proxy_protocol_v2: false,
            listen_require_proxy_protocol_v2: true,
            accepted_conn_ipv4_tos: 0,
            accepted_conn_tcp_keep_alive_time: DEFAULT_TCP_KEEP_ALIVE_TIME,
            accepted_conn_tcp_keep_alive_interval: DEFAULT_TCP_KEEP_ALIVE_INTERVAL,
            accepted_conn_tcp_keep_alive_retries: DEFAULT_TCP_KEEP_ALIVE_RETRIES,
            accepted_conn_tcp_no_delay: true,
            connect_enable_preconnect: false,
            connect_enable_tfo: true,
            connected_conn_ipv4_tos: 0,
            connected_conn_tcp_keep_alive_time: DEFAULT_TCP_KEEP_ALIVE_TIME,
            connected_conn_tcp_keep_alive_interval: DEFAULT_TCP_KEEP_ALIVE_INTERVAL,
            connected_conn_tcp_keep_alive_retries: DEFAULT_TCP_KEEP_ALIVE_RETRIES,
            connected_conn_tcp_no_delay: true,
        }
    }
}

impl Endpoint {
    pub(crate) fn apply_socket_options_listener(&self, socket: &UniSocket) -> io::Result<()> {
        #[cfg(not(any(
            target_os = "fuchsia",
            target_os = "redox",
            target_os = "solaris",
            target_os = "illumos",
            target_os = "haiku",
        )))]
        if self.conf.accepted_conn_ipv4_tos != 0 {
            socket.set_tos_v4(self.conf.accepted_conn_ipv4_tos)?;
        }

        if self.conf.accepted_conn_tcp_keep_alive_time > 0
            && self.conf.accepted_conn_tcp_keep_alive_interval > 0
            && self.conf.accepted_conn_tcp_keep_alive_retries > 0
        {
            #[cfg_attr(
                not(any(
                    target_os = "android",
                    target_os = "dragonfly",
                    target_os = "freebsd",
                    target_os = "fuchsia",
                    target_os = "illumos",
                    target_os = "ios",
                    target_os = "visionos",
                    target_os = "linux",
                    target_os = "macos",
                    target_os = "netbsd",
                    target_os = "tvos",
                    target_os = "watchos",
                    target_os = "windows",
                    target_os = "cygwin",
                )),
                allow(unused_mut)
            )]
            let mut tcp_keepalive = TcpKeepalive::new().with_time(Duration::from_secs(
                self.conf.accepted_conn_tcp_keep_alive_time,
            ));

            #[cfg(any(
                target_os = "android",
                target_os = "dragonfly",
                target_os = "freebsd",
                target_os = "fuchsia",
                target_os = "illumos",
                target_os = "ios",
                target_os = "visionos",
                target_os = "linux",
                target_os = "macos",
                target_os = "netbsd",
                target_os = "tvos",
                target_os = "watchos",
                target_os = "windows",
                target_os = "cygwin",
            ))]
            {
                tcp_keepalive = tcp_keepalive
                    .with_interval(Duration::from_secs(
                        self.conf.accepted_conn_tcp_keep_alive_interval,
                    ))
                    .with_retries(self.conf.accepted_conn_tcp_keep_alive_retries);
            }

            socket.set_tcp_keepalive(&tcp_keepalive)?;
        }

        if self.conf.accepted_conn_tcp_no_delay {
            socket.set_tcp_nodelay(true)?;
        }

        Ok(())
    }

    pub(crate) fn apply_socket_options_connected(&self, socket: &UniStream) -> io::Result<()> {
        let socket = socket.as_socket_ref();

        #[cfg(not(any(
            target_os = "fuchsia",
            target_os = "redox",
            target_os = "solaris",
            target_os = "illumos",
            target_os = "haiku",
        )))]
        if self.conf.connected_conn_ipv4_tos != 0 {
            socket.set_tos_v4(self.conf.connected_conn_ipv4_tos)?;
        }

        if self.conf.connected_conn_tcp_keep_alive_time > 0
            && self.conf.connected_conn_tcp_keep_alive_interval > 0
            && self.conf.connected_conn_tcp_keep_alive_retries > 0
        {
            #[cfg_attr(
                not(any(
                    target_os = "android",
                    target_os = "dragonfly",
                    target_os = "freebsd",
                    target_os = "fuchsia",
                    target_os = "illumos",
                    target_os = "ios",
                    target_os = "visionos",
                    target_os = "linux",
                    target_os = "macos",
                    target_os = "netbsd",
                    target_os = "tvos",
                    target_os = "watchos",
                    target_os = "windows",
                    target_os = "cygwin",
                )),
                allow(unused_mut)
            )]
            let mut tcp_keepalive = TcpKeepalive::new().with_time(Duration::from_secs(
                self.conf.connected_conn_tcp_keep_alive_time,
            ));

            #[cfg(any(
                target_os = "android",
                target_os = "dragonfly",
                target_os = "freebsd",
                target_os = "fuchsia",
                target_os = "illumos",
                target_os = "ios",
                target_os = "visionos",
                target_os = "linux",
                target_os = "macos",
                target_os = "netbsd",
                target_os = "tvos",
                target_os = "watchos",
                target_os = "windows",
                target_os = "cygwin",
            ))]
            {
                tcp_keepalive = tcp_keepalive
                    .with_interval(Duration::from_secs(
                        self.conf.connected_conn_tcp_keep_alive_interval,
                    ))
                    .with_retries(self.conf.connected_conn_tcp_keep_alive_retries);
            }

            socket.set_tcp_keepalive(&tcp_keepalive)?;
        }

        if self.conf.connected_conn_tcp_no_delay {
            socket.set_tcp_nodelay(true)?;
        }

        Ok(())
    }
}
