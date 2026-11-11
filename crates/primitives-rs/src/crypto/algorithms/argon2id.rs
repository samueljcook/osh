//! Argon2id version 19 with explicit costs and a zeroizing workspace.
//!
//! This module produces the algorithm's unmodified 32-byte output. For stored
//! password parameters, resource limits, and the shared key schedule, use
//! [`crate::crypto::password_derivation`]. Costs describe one invocation; callers must
//! bound concurrency separately. Execution is synchronous and creates no threads,
//! including when the profile contains multiple Argon2 lanes.

use argon2::{Algorithm, Argon2, AssociatedData, Block, Params, ParamsBuilder, Version};
use zeroize::Zeroizing;

use crate::crypto::Key32;

/// The explicit resource costs of one Argon2id invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Costs {
	/// The memory cost in KiB. Argon2 rounds workspace blocks down to a multiple
	/// of `4 * lanes`; the exact requested cost still affects the output.
	pub memory_kib: u32,
	/// The number of passes over memory.
	pub iterations: u32,
	/// The number of algorithm lanes, which does not select a thread count.
	pub lanes: u32,
}

/// Algorithm-valid costs for a 32-byte Argon2id version 19 output.
///
/// Construction does not allocate the workspace or approve a resource budget.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Profile(Params);

/// Borrowed inputs to the standard algorithm. The caller retains their storage.
pub struct Input<'a> {
	/// Password bytes, without normalization or encoding changes.
	pub password: &'a [u8],
	/// Salt bytes; this implementation requires at least eight bytes.
	pub salt: &'a [u8],
	/// Optional independently held secret material; use an empty slice to omit it.
	pub secret: &'a [u8],
	/// Optional algorithm-associated data, at most 32 bytes; it may be empty.
	pub associated_data: &'a [u8],
}

impl Profile {
	/// Checks the algorithm's cost bounds without allocating memory.
	pub fn new(costs: Costs) -> Result<Self, Error> {
		// RustCrypto 0.5 evaluates eight times the lane count before its upper
		// bound check. Check that bound first so arbitrary input cannot overflow.
		if costs.lanes > Params::MAX_P_COST {
			return Err(Error::InvalidProfile);
		}
		Params::new(costs.memory_kib, costs.iterations, costs.lanes, Some(32))
			.map(Self)
			.map_err(|_| Error::InvalidProfile)
	}

	/// Returns the exact costs that participate in the algorithm's input.
	pub fn costs(&self) -> Costs {
		Costs {
			memory_kib: self.0.m_cost(),
			iterations: self.0.t_cost(),
			lanes: self.0.p_cost(),
		}
	}

	/// Derives a key and clears the allocated workspace before returning.
	///
	/// Approve untrusted costs before calling this operation, or use the bounded
	/// API in [`crate::crypto::password_derivation`]. Input errors precede allocation.
	pub fn derive_key(&self, input: Input<'_>) -> Result<Key32, Error> {
		if input.password.len() > u32::MAX as usize
			|| input.salt.len() < argon2::MIN_SALT_LEN
			|| input.salt.len() > u32::MAX as usize
			|| input.secret.len() > u32::MAX as usize
		{
			return Err(Error::InvalidInput);
		}
		let data = AssociatedData::new(input.associated_data).map_err(|_| Error::InvalidInput)?;
		let params = ParamsBuilder::new()
			.m_cost(self.0.m_cost())
			.t_cost(self.0.t_cost())
			.p_cost(self.0.p_cost())
			.output_len(32)
			.data(data)
			.build()
			.map_err(|_| Error::InvalidInput)?;
		let algorithm =
			Argon2::new_with_secret(input.secret, Algorithm::Argon2id, Version::V0x13, params)
				.map_err(|_| Error::InvalidInput)?;
		let mut memory = Zeroizing::new(Vec::new());
		memory
			.try_reserve_exact(self.0.block_count())
			.map_err(|_| Error::AllocationFailed)?;
		memory.resize(self.0.block_count(), Block::default());
		let mut output = Key32::zeroed();
		algorithm
			.hash_password_into_with_memory(
				input.password,
				input.salt,
				output.as_mut_bytes(),
				memory.as_mut_slice(),
			)
			.map_err(|_| Error::DerivationFailed)?;
		Ok(output)
	}
}

/// An Argon2id parameter, input, allocation, or execution failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
	/// The costs violate the algorithm's bounds.
	InvalidProfile,
	/// An input length violates the implementation's documented bounds.
	InvalidInput,
	/// The workspace could not be allocated.
	AllocationFailed,
	/// The algorithm could not complete the derivation.
	DerivationFailed,
}
impl std::fmt::Display for Error {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(match self {
			Self::InvalidProfile => "invalid Argon2id profile",
			Self::InvalidInput => "invalid Argon2id input",
			Self::AllocationFailed => "Argon2id workspace allocation failed",
			Self::DerivationFailed => "Argon2id derivation failed",
		})
	}
}
impl std::error::Error for Error {}
