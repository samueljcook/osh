//! HKDF-SHA512 extraction and expansion from RFC 5869.
hkdf_algorithm!(::sha2::Sha512, 64);
