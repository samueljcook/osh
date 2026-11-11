//! Cookie header handling and managed cookie policies.
#[cfg(feature = "cookie-headers")]
pub mod headers;
#[cfg(feature = "cookies")]
pub mod manager;
