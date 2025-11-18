//! Utils

pub(crate) mod file;
pub(crate) mod io;

/// The version of the server.
pub(crate) const VERSION: &str = concat!("v", include_str!(concat!(env!("OUT_DIR"), "/VERSION")));

/// The version of the server.
pub(crate) const BUILD_TIME: &str = include_str!(concat!(env!("OUT_DIR"), "/BUILD_TIME"));
