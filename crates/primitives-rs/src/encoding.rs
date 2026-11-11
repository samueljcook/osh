//! Reversible byte and text encodings.

#[cfg(feature = "byte-encoding")]
pub mod binary_to_text;
pub mod decimal;
#[cfg(feature = "mnemonic")]
pub mod mnemonic;
