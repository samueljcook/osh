//! Password-derived root keys with stored parameters and explicit resource limits.
//!
//! Argon2id version 19 performs password stretching once. The shared hedged KDF
//! then derives a root key that binds the stored parameters. Consumers derive
//! separate application keys from that root through [`crate::crypto::key_derivation`].
//! Neither this operation nor the fast KDF adds password entropy. Resistance to
//! inexpensive password guessing depends on Argon2id; the fast KDF does not
//! preserve memory hardness if Argon2id fails.
//!
//! Choose costs for the target devices and bound concurrent invocations. There
//! is no implicit profile, automatic calibration, or downgrade. [RFC 9106's](https://www.rfc-editor.org/rfc/rfc9106.html#section-4)
//! memory-constrained recommendation uses 64 MiB, three passes, and four lanes;
//! applications must measure their selected profile. Run synchronous derivation
//! on a worker when an event loop must remain responsive. Password bytes are
//! accepted exactly as provided; the application owns text encoding and policy.
//!
//! # Version 1 protocol
//!
//! Parameters encode as `PWDK || 0x01 || memory_kib:u32be || iterations:u32be ||
//! lanes:u32be || salt:32`, exactly 49 bytes. Parsing checks the algorithm bounds,
//! without allocating a workspace or authenticating the parameters. An explicit
//! resource policy must accept the costs and password length before derivation.
//! Select that policy independently of untrusted parameters. Applications can
//! compare [`Parameters::profile`] with an approved profile to enforce cost
//! floors or an exact allowlist.
//!
//! Compute `A = Argon2id-19(password, salt, costs, output_len=32, secret,
//! associated_data=empty)`. The optional 32-byte additional secret uses Argon2's
//! secret input and is never stored in the parameters. Then compute
//! `key_derivation::derive_key(A, Context { salt: empty,
//! purpose: "primitives-rs/password-derivation/v1", fields: [encoded_parameters] })`.
//! The temporary `A` and workspace are cleared. The resulting root is secret
//! key material and must not be stored as a public password verifier.
//!
//! Changing parameters or the password changes the derived root. To preserve
//! encrypted application data across such changes, protect an independently
//! generated master key with [`crate::crypto::password_encrypted_key`].

use crate::crypto::{Key32, RandomError, algorithms::argon2id, key_derivation};

/// The encoded size of version 1 password-derivation parameters.
pub const PARAMETERS_LEN: usize = 49;
const PREFIX: &[u8; 5] = b"PWDK\x01";

/// A 32-byte public salt. Generate a fresh salt for each new password record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PasswordSalt([u8; 32]);
impl PasswordSalt {
	/// Generates a salt through the platform's secure random source.
	pub fn generate() -> Result<Self, RandomError> {
		crate::random::secure_bytes::generate_random_byte_array().map(Self)
	}
	/// Wraps an existing protocol's salt bytes.
	pub const fn from_array(bytes: [u8; 32]) -> Self {
		Self(bytes)
	}
	/// Returns the public salt bytes.
	pub const fn as_bytes(&self) -> &[u8; 32] {
		&self.0
	}
}

/// Parsed public parameters. Their validity does not establish their authenticity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Parameters {
	profile: argon2id::Profile,
	salt: PasswordSalt,
}

/// Secret inputs that the caller must protect and clear.
#[derive(Clone, Copy)]
pub struct Input<'a> {
	/// Exact password bytes, including any embedded zero bytes.
	pub password: &'a [u8],
	/// An optional separately held secret. Every derivation requires the same value.
	pub additional_secret: Option<&'a Key32>,
}

/// Maximum per-invocation costs, chosen independently of an untrusted record.
///
/// These ceilings do not enforce password quality or minimum costs. Memory is
/// measured in KiB; the lane count does not cause additional threads to start.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceLimits {
	/// Maximum Argon2 memory cost in KiB.
	pub max_memory_kib: u32,
	/// Maximum passes over memory.
	pub max_iterations: u32,
	/// Maximum algorithm lanes.
	pub max_lanes: u32,
	/// Maximum password length in bytes, checked before Argon2 executes.
	pub max_password_len: usize,
}
impl ResourceLimits {
	/// Checks all costs and the password length without allocating a workspace.
	pub fn check(&self, profile: &argon2id::Profile, password_len: usize) -> Result<(), Error> {
		let costs = profile.costs();
		if costs.memory_kib > self.max_memory_kib
			|| costs.iterations > self.max_iterations
			|| costs.lanes > self.max_lanes
		{
			return Err(Error::ResourceLimitExceeded);
		}
		if password_len > self.max_password_len {
			return Err(Error::PasswordTooLong);
		}
		Ok(())
	}
}

impl Parameters {
	/// Combines a chosen profile with an existing protocol salt.
	pub fn new(profile: argon2id::Profile, salt: PasswordSalt) -> Self {
		Self { profile, salt }
	}
	/// Generates a fresh salt for a chosen profile.
	pub fn generate(profile: argon2id::Profile) -> Result<Self, RandomError> {
		Ok(Self::new(profile, PasswordSalt::generate()?))
	}
	/// Returns the algorithm profile without performing derivation.
	pub fn profile(&self) -> &argon2id::Profile {
		&self.profile
	}
	/// Returns the public salt.
	pub fn salt(&self) -> &PasswordSalt {
		&self.salt
	}
	/// Encodes the version, exact costs, and salt.
	pub fn to_bytes(&self) -> [u8; PARAMETERS_LEN] {
		let mut bytes = [0; PARAMETERS_LEN];
		bytes[..5].copy_from_slice(PREFIX);
		let costs = self.profile.costs();
		bytes[5..9].copy_from_slice(&costs.memory_kib.to_be_bytes());
		bytes[9..13].copy_from_slice(&costs.iterations.to_be_bytes());
		bytes[13..17].copy_from_slice(&costs.lanes.to_be_bytes());
		bytes[17..].copy_from_slice(self.salt.as_bytes());
		bytes
	}
	/// Parses an exact version 1 encoding without allocating a workspace.
	pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
		if bytes.len() != PARAMETERS_LEN || &bytes[..5] != PREFIX {
			return Err(Error::InvalidParameters);
		}
		let number = |range: std::ops::Range<usize>| {
			u32::from_be_bytes(bytes[range].try_into().expect("four bytes"))
		};
		let profile = argon2id::Profile::new(argon2id::Costs {
			memory_kib: number(5..9),
			iterations: number(9..13),
			lanes: number(13..17),
		})
		.map_err(|_| Error::InvalidParameters)?;
		Ok(Self::new(
			profile,
			PasswordSalt::from_array(bytes[17..].try_into().expect("32 bytes")),
		))
	}
	/// Derives a root after checking the explicit resource limits.
	pub fn derive_key(&self, input: Input<'_>, limits: ResourceLimits) -> Result<Key32, Error> {
		limits.check(&self.profile, input.password.len())?;
		let root = self
			.profile
			.derive_key(argon2id::Input {
				password: input.password,
				salt: self.salt.as_bytes(),
				secret: input
					.additional_secret
					.map_or(&[], |key| key.expose_secret()),
				associated_data: b"",
			})
			.map_err(Error::Argon2id)?;
		Ok(key_derivation::derive_key(
			&root,
			key_derivation::Context {
				salt: b"",
				purpose: b"primitives-rs/password-derivation/v1",
				fields: &[&self.to_bytes()],
			},
		))
	}
}

/// A parameter, resource-policy, or derivation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
	/// The parameter encoding, version, or algorithm costs are invalid.
	InvalidParameters,
	/// At least one cost exceeds the caller's resource budget.
	ResourceLimitExceeded,
	/// The password length exceeds the caller's limit.
	PasswordTooLong,
	/// The underlying algorithm rejected an input or could not complete.
	Argon2id(argon2id::Error),
}
impl std::fmt::Display for Error {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::InvalidParameters => f.write_str("invalid password-derivation parameters"),
			Self::ResourceLimitExceeded => {
				f.write_str("password-derivation resource limit exceeded")
			}
			Self::PasswordTooLong => f.write_str("password length limit exceeded"),
			Self::Argon2id(error) => error.fmt(f),
		}
	}
}
impl std::error::Error for Error {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::Argon2id(error) => Some(error),
			_ => None,
		}
	}
}
