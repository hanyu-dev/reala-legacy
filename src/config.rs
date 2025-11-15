//! Config & CLI

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result};
use base64::Engine;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use uni_addr::UniAddr;

#[derive(Debug, Clone)]
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Config {
    #[serde(default = "current_config_version")]
    /// The config version
    pub version: u8,

    /// A list of relays.
    pub endpoints: Vec<Endpoint>,
}

fn current_config_version() -> u8 {
    const CONFIG_VERSION: u8 = 0;

    CONFIG_VERSION
}

impl Config {
    /// Load the config from the config file.
    pub(crate) async fn load(path: Option<&str>) -> Result<Self> {
        static CONFIG_FILE_PATH: OnceLock<PathBuf> = OnceLock::new();

        let path = CONFIG_FILE_PATH.get_or_init(|| {
            const DEFAULT_CONFIG_FILE_PATH: &str = "./config.yaml";

            PathBuf::from(path.unwrap_or(DEFAULT_CONFIG_FILE_PATH))
        });

        let file = fs::read(path).await;

        if let Err(e) = &file
            && e.kind() == io::ErrorKind::NotFound
        {
            tracing::error!("Config file does not exist at {path:?}, generate a default one");

            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .await
                .context("Create default config file error")?;

            let config = Config {
                version: current_config_version(),
                endpoints: vec![Endpoint {
                    listen: "0.0.0.0:8080".parse().expect("Must be valid"),
                    target: "127.0.0.1:80".parse().expect("Must be valid"),
                    options: RelayOptions::default(),
                }],
            };

            let serialized =
                serde_saphyr::to_string(&config).context("Serialize default config error")?;

            file.write_all(serialized.as_bytes())
                .await
                .context("Write default config file error")?;
        }

        serde_saphyr::from_slice(&file?).context("Parse config file error")
    }
}

#[derive(Debug, Clone)]
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Endpoint {
    /// The listen address
    pub listen: UniAddr,

    /// The target address to forward to.
    ///
    /// When [`RelayOptions::accept_proxy_protocol`] is enabled,
    /// this is the
    pub target: UniAddr,

    /// The relay target
    #[serde(default)]
    pub options: RelayOptions,
}

#[derive(Debug, Clone)]
#[derive(serde::Serialize, serde::Deserialize)]
/// Options for the relay behavior.
pub(crate) struct RelayOptions {
    /// Whether to enable TCP `splice(2)`, i.e., zero-copy forwarding between
    /// the accepted connection and the target connection.
    ///
    /// Defaults to `true`.
    pub splice: bool,

    /// IP TOS (Type of Service) value for accepted connections / outgoing
    /// connections to the target.
    ///
    /// - LOWDELAY: 0x10
    /// - THROUGHPUT: 0x08
    /// - RELIABILITY: 0x04
    /// - MINCOST: 0x02
    ///
    /// Defaults to `None`.
    pub tos: Option<u32>,

    /// Whether to enable TCP Fast Open when accepting TCP connections /
    /// connecting to the target.
    ///
    /// Currently unused.
    pub tfo: bool,

    /// Whether to parse PROXY protocol (v2) header when accepting connections.
    ///
    /// When enabled, the actual target address will be extracted from the
    /// PROXY protocol header instead of using the `target` field.
    ///
    /// For security considerations, it's recommended to enable
    /// `proxy_protocol_header_encryption_psk` when enabling this option.
    ///
    /// Defaults to `false`.
    pub accept_proxy_protocol: bool,

    /// Whether to require a PROXY protocol (v2) header when accepting
    /// connections.
    ///
    /// Defaults to `true`.
    pub require_proxy_protocol: bool,

    /// PSK for encrypting / decrypting the PROXY protocol (v2) header.
    ///
    /// Since expose the actual target address to public may lead to security
    /// issues, encrypting the header can mitigate this.
    ///
    /// - Cipher: `AES-128-GCM`.
    ///
    /// ```no_run
    /// +-----------------+------------------+------------------------+----------------+
    /// | Length (uint16) | Nonce (12 bytes) | Ciphertext (Var bytes) | Tag (32 bytes) |
    /// +-----------------+------------------+------------------------+----------------+
    /// ```
    ///
    /// Can be generated with `openssl rand -base64 32`.
    ///
    /// Defaults to `None`.
    pub proxy_protocol_header_encryption_psk: Option<AeadKey>,
}

impl Default for RelayOptions {
    fn default() -> Self {
        Self {
            splice: true,
            tos: None,
            tfo: false,
            accept_proxy_protocol: false,
            require_proxy_protocol: true,
            proxy_protocol_header_encryption_psk: None,
        }
    }
}

wrapper_lite::wrapper!(
    #[wrapper_impl(DebugName)]
    #[wrapper_impl(AsRef<[u8; 32]>)]
    #[wrapper_impl(Deref<[u8; 32]>)]
    #[derive(Clone)]
    /// AEAD key for encrypting / decrypting the PROXY protocol header.
    pub struct AeadKey(Arc<[u8; 32]>);
);

impl serde::Serialize for AeadKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(self.as_ref()))
    }
}

impl<'de> serde::Deserialize<'de> for AeadKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = <&str>::deserialize(deserializer)?;

        let bytes = base64::engine::general_purpose::STANDARD
            .decode(s)
            .map_err(serde::de::Error::custom)?;

        if bytes.len() != 32 {
            return Err(serde::de::Error::custom(
                "Invalid AEAD key length, must be 32 bytes",
            ));
        }

        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes);

        Ok(Self::const_from(Arc::new(key)))
    }
}
