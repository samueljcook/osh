//! Serialization with explicit representation and resource limits.
#[cfg(feature = "json")]
pub mod json;
#[cfg(feature = "versioned-state")]
pub mod versioned_state;
