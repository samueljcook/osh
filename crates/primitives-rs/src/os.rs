//! Operating-system facilities with explicit platform selection.

#[cfg(feature = "app-dirs")]
pub mod app_dirs;

#[cfg(feature = "credential-store")]
pub mod credential_store;
