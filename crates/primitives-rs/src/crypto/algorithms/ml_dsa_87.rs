//! ML-DSA-87 signatures with FIPS 204 seed encoding and explicit context.
//!
//! Signing uses fresh 32-byte randomization. This algorithm is a standalone
//! building block; [`crate::crypto::signature`] combines ML-DSA-87 with SLH-DSA.
crate::crypto::ml_dsa_impl::define_ml_dsa!(ml_dsa::MlDsa87, 2592, 4627);
