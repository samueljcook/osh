//! Purpose-bound rotating keys from one shared hedged derivation protocol.
//!
//! Each master key derives one purpose key. Encryption, MACs, and child keysets
//! derive separate operation keys from that purpose key. The application stores
//! its master keys newest first and retains older keys while values need them.

use std::fmt;

use crate::crypto::Key32;
pub use crate::crypto::error::KeysetError;
use crate::crypto::key_derivation;

/// Maximum length, in bytes, of a purpose string bound to a keyset.
pub const MAX_PURPOSE_LEN: usize = 255;

/// Maximum number of latest-first rotation keys accepted in one keyset.
pub const MAX_KEYSET_KEYS: usize = 16;

/// The matching key's position in the supplied latest-first keyset.
///
/// This position is not a persistent key identifier. It changes when the application
/// changes the keyset. A value that matches an older key can be replaced under the
/// newest key after successful authentication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyMatch {
	pub(crate) index: usize,
}

impl KeyMatch {
	/// Returns the zero-based position in the supplied latest-first keyset.
	pub fn index(self) -> usize {
		self.index
	}

	/// Returns whether the value authenticated under the newest key.
	pub fn is_latest(self) -> bool {
		self.index == 0
	}
}

/// A latest-first set of derived working keys for one purpose.
///
/// Encryption uses the newest key, and decryption tries every key from the newest to the
/// oldest. A value that was encrypted under a key that the keyset no longer holds fails to
/// decrypt, so the application must keep an older key in its keyset for as long as it retains
/// any value that was encrypted under that key. The application can remove a key only after it
/// has re-encrypted or deleted every value that depends on it.
///
/// A keyset also binds its keys to its purpose string, and an envelope does not store the
/// purpose. The application must decrypt a value with a keyset that has the purpose of the
/// keyset that encrypted it because another purpose derives other keys and decryption fails.
pub struct Keyset {
	purpose: Purpose,
	latest_first_keys: Vec<Key32>,
}

/// Derives a latest-first keyset from latest-first `Key32` values and a purpose string.
pub fn derive_keyset_from_latest_first_keys<I>(
	latest_first_keys: I,
	purpose: &str,
) -> Result<Keyset, KeysetError>
where
	I: IntoIterator<Item = Key32>,
{
	let mut input_keys = Vec::new();
	input_keys
		.try_reserve_exact(MAX_KEYSET_KEYS)
		.map_err(|_| KeysetError::AllocationFailed)?;
	for key in latest_first_keys {
		if input_keys.len() == MAX_KEYSET_KEYS {
			return Err(KeysetError::TooManyKeys {
				max: MAX_KEYSET_KEYS,
			});
		}
		input_keys.push(key);
	}
	validate_latest_first_keys(&input_keys)?;
	let purpose = Purpose::new(purpose)?;
	let mut derived_keys = Vec::new();
	derived_keys
		.try_reserve_exact(input_keys.len())
		.map_err(|_| KeysetError::AllocationFailed)?;
	for key in &input_keys {
		derived_keys.push(derive_purpose_key(key, purpose.as_str()));
	}
	Ok(Keyset {
		purpose,
		latest_first_keys: derived_keys,
	})
}

impl Keyset {
	/// Returns the purpose string used to derive this keyset.
	pub fn purpose(&self) -> &str {
		self.purpose.as_str()
	}

	/// Returns the number of latest-first keys in this keyset.
	pub fn key_count(&self) -> usize {
		self.latest_first_keys.len()
	}

	/// Derives a latest-first child keyset for a narrower purpose.
	pub fn derive_child_keyset(&self, purpose: &str) -> Result<Keyset, KeysetError> {
		let child_purpose = self.purpose.for_child(purpose)?;
		let mut child_keys = Vec::new();
		child_keys
			.try_reserve_exact(self.latest_first_keys.len())
			.map_err(|_| KeysetError::AllocationFailed)?;
		for key in &self.latest_first_keys {
			child_keys.push(key_derivation::derive_key(
				key,
				key_derivation::Context {
					salt: b"",
					purpose: b"primitives-rs/keyset/v1/child",
					fields: &[self.purpose.as_str().as_bytes(), purpose.as_bytes()],
				},
			));
		}
		Ok(Keyset {
			purpose: child_purpose,
			latest_first_keys: child_keys,
		})
	}

	/// Returns the newest rotation key in this keyset.
	#[cfg(not(feature = "__internal"))]
	pub(crate) fn latest_key(&self) -> &Key32 {
		self.latest_key_inner()
	}

	/// Returns the newest rotation key in this keyset.
	#[cfg(feature = "__internal")]
	#[doc(hidden)]
	pub fn latest_key(&self) -> &Key32 {
		self.latest_key_inner()
	}

	fn latest_key_inner(&self) -> &Key32 {
		&self.latest_first_keys[0]
	}

	pub(crate) fn latest_first_keys(&self) -> &[Key32] {
		&self.latest_first_keys
	}
}

impl fmt::Debug for Keyset {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("Keyset")
			.field("purpose", &self.purpose)
			.field("key_count", &self.latest_first_keys.len())
			.finish()
	}
}

#[derive(Clone, Eq, PartialEq)]
struct Purpose(String);

impl Purpose {
	fn new(purpose: &str) -> Result<Self, KeysetError> {
		validate_purpose(purpose)?;
		let mut text = String::new();
		text.try_reserve_exact(purpose.len())
			.map_err(|_| KeysetError::AllocationFailed)?;
		text.push_str(purpose);
		Ok(Self(text))
	}

	fn as_str(&self) -> &str {
		&self.0
	}

	fn for_child(&self, child_purpose: &str) -> Result<Self, KeysetError> {
		validate_purpose(child_purpose)?;
		let combined_len = self.0.len() + 1 + child_purpose.len();
		if combined_len > MAX_PURPOSE_LEN {
			return Err(KeysetError::PurposeTooLong {
				actual: combined_len,
				max: MAX_PURPOSE_LEN,
			});
		}

		let mut combined = String::new();
		combined
			.try_reserve_exact(combined_len)
			.map_err(|_| KeysetError::AllocationFailed)?;
		combined.push_str(&self.0);
		combined.push('/');
		combined.push_str(child_purpose);
		Ok(Self(combined))
	}
}

impl fmt::Debug for Purpose {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_tuple("Purpose").field(&self.0).finish()
	}
}

fn validate_latest_first_keys(latest_first_keys: &[Key32]) -> Result<(), KeysetError> {
	if latest_first_keys.is_empty() {
		return Err(KeysetError::EmptyKeyset);
	}
	for index in 0..latest_first_keys.len() {
		if latest_first_keys[..index].contains(&latest_first_keys[index]) {
			return Err(KeysetError::DuplicateKey { index });
		}
	}
	Ok(())
}

fn derive_purpose_key(input_key: &Key32, purpose: &str) -> Key32 {
	key_derivation::derive_key(
		input_key,
		key_derivation::Context {
			salt: b"",
			purpose: b"primitives-rs/keyset/v1/purpose",
			fields: &[purpose.as_bytes()],
		},
	)
}

fn validate_purpose(purpose: &str) -> Result<(), KeysetError> {
	if purpose.is_empty() {
		return Err(KeysetError::EmptyPurpose);
	}
	if purpose.len() > MAX_PURPOSE_LEN {
		return Err(KeysetError::PurposeTooLong {
			actual: purpose.len(),
			max: MAX_PURPOSE_LEN,
		});
	}
	for (index, byte) in purpose.as_bytes().iter().copied().enumerate() {
		if byte == b'/' || !(0x21..=0x7e).contains(&byte) {
			return Err(KeysetError::InvalidPurposeByte { index, byte });
		}
	}
	Ok(())
}
