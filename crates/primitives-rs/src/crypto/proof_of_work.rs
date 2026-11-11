//! Bounded SHA-256 computational-work search and constant-cost verification.
//!
//! The digest is `SHA256("primitives-rs/sha256-work/v2" || challenge[128] ||
//! nonce:u64be)`, where the challenge is a complete 128-byte digest from the
//! hedged [`hash`] protocol. Every challenge byte affects the
//! work, so the binding between a solution and the challenge's inputs holds while
//! either component of the hedged digest resists second preimages. A solution has
//! at least the required number of leading zero bits. A consuming protocol must
//! authenticate the challenge and difficulty, bind its exact operation and
//! payload, and enforce expiry and replay policy. Work is an admission cost, not
//! authentication or a standalone rate limit.

use crate::crypto::{algorithms::sha256, hash};

const DOMAIN: &[u8] = b"primitives-rs/sha256-work/v2";

/// A fixed challenge and explicit SHA-256 work requirement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Sha256Work {
	/// A complete hedged digest that binds the operation being charged.
	pub challenge: hash::Digest,
	/// The required leading zero bits. Zero explicitly disables computational cost.
	pub required_zero_bits: u8,
}

impl Sha256Work {
	/// Checks one candidate with one hash and no allocation.
	pub fn verify(&self, nonce: u64) -> bool {
		let digest = sha256::hash_parts(&[DOMAIN, self.challenge.as_bytes(), &nonce.to_be_bytes()]);
		let complete_bytes = usize::from(self.required_zero_bits / 8);
		let remaining_bits = self.required_zero_bits % 8;
		let prefix_is_zero = digest.as_bytes()[..complete_bytes]
			.iter()
			.all(|byte| *byte == 0);
		prefix_is_zero
			&& (remaining_bits == 0
				|| digest.as_bytes()[complete_bytes] >> (8 - remaining_bits) == 0)
	}

	/// Searches at most `max_candidates` nonces, allowing cancellation between calls.
	pub fn search(&self, input: SearchInput) -> SearchResult {
		let mut nonce = input.start_nonce;
		for _ in 0..input.max_candidates {
			if self.verify(nonce) {
				return SearchResult::Solved { nonce };
			}
			match nonce.checked_add(1) {
				Some(next) => nonce = next,
				None => return SearchResult::Exhausted,
			}
		}
		SearchResult::Continue { next_nonce: nonce }
	}
}

/// Explicit bounds for one synchronous search call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SearchInput {
	/// The first nonce to check.
	pub start_nonce: u64,
	/// The maximum number of candidates to check before returning.
	pub max_candidates: u32,
}

/// The result of one bounded search call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchResult {
	/// A nonce satisfies the work requirement.
	Solved {
		/// The verified nonce.
		nonce: u64,
	},
	/// The search reached its work limit without a solution.
	Continue {
		/// The first unchecked nonce for a subsequent call.
		next_nonce: u64,
	},
	/// Every nonce from the supplied start through `u64::MAX` failed.
	Exhausted,
}
