//! Application libraries built on the independent `primitives-rs` facilities.
//!
//! Enable `local-env-vault` for Unix encrypted environment storage, or
//! `local-env-vault-cli` for its interactive command wrapper.

#[cfg(all(unix, feature = "local-env-vault"))]
pub mod local_env_vault;
