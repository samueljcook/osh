//! Published Item key and scope authentication composes with opaque Raw bytes without a Document schema.

use primitives_rs::{
	crypto::{
		encryption::{DecryptError, DecryptInput, EncryptError, EncryptInput, decrypt, encrypt},
		keysets::Keyset,
	},
	secret_bytes::SecretBytes,
};

use crate::kv::{Key, KeyError};
use crate::records::{RecordKey, RecordMetadata, StateTimestampMicros};
use crate::scope::ScopeIdentity;

mod mutation;
pub use mutation::{
	DecryptedRawLiveMutationCurrent, DecryptedRawLiveOrInitMutationResult,
	DecryptedRawMutationCurrent, DecryptedRawMutationResult, EncryptedRawMutation,
};

#[derive(Debug, thiserror::Error)]
pub(in crate::kv) enum PublishedItemKeyError {
	#[error("published Item encryption requires a UTF-8 key")]
	Utf8(#[source] std::str::Utf8Error),
	#[error("published Item encryption requires a canonical complete Key")]
	Key(#[source] KeyError),
}
pub(in crate::kv) fn prepare_published_item_key(
	key: &RecordKey,
) -> Result<Key, PublishedItemKeyError> {
	let text = std::str::from_utf8(key.as_bytes()).map_err(PublishedItemKeyError::Utf8)?;
	Key::from_persisted_text(text.to_owned()).map_err(PublishedItemKeyError::Key)
}

pub(in crate::kv) struct PublishedItemEncryptionFields<'a> {
	pub(in crate::kv) keyset: &'a Keyset,
	pub(in crate::kv) scope: Option<&'a ScopeIdentity>,
	pub(in crate::kv) key: &'a Key,
}
pub(in crate::kv) struct PublishedItemEncryption<'a> {
	keyset: &'a Keyset,
	associated_data: Vec<u8>,
}
impl<'a> PublishedItemEncryption<'a> {
	pub(crate) fn new(fields: PublishedItemEncryptionFields<'a>) -> Self {
		Self {
			keyset: fields.keyset,
			associated_data: item_associated_data(fields.scope, fields.key),
		}
	}
	pub(in crate::kv) fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, EncryptError> {
		encrypt(
			self.keyset,
			EncryptInput {
				plaintext,
				associated_data: &self.associated_data,
			},
		)
	}
	pub(in crate::kv) fn decrypt(&self, envelope: &[u8]) -> Result<SecretBytes, DecryptError> {
		decrypt(
			self.keyset,
			DecryptInput {
				envelope,
				associated_data: &self.associated_data,
			},
		)
	}
}

const ITEM_ASSOCIATED_DATA_HEADER: &[u8] = b"paranoid.kv.raw.v1\0";
const CORE_SCOPE_TAG: u8 = 0x00;
const APP_SCOPE_TAG: u8 = 0x01;

fn append_u32_length_prefixed(out: &mut Vec<u8>, bytes: &[u8]) {
	let len = u32::try_from(bytes.len()).expect("associated data field length fits in u32");
	out.extend_from_slice(&len.to_be_bytes());
	out.extend_from_slice(bytes);
}

fn item_associated_data(scope: Option<&ScopeIdentity>, key: &Key) -> Vec<u8> {
	let key_bytes = key.as_str().as_bytes();
	let scope_encoded_len = match scope {
		None => 1,
		Some(scope) => {
			1 + 4 + scope.storage().as_bytes().len() + 4 + scope.namespace().as_bytes().len()
		}
	};
	let mut associated_data = Vec::with_capacity(
		ITEM_ASSOCIATED_DATA_HEADER.len() + scope_encoded_len + 4 + key_bytes.len(),
	);
	associated_data.extend_from_slice(ITEM_ASSOCIATED_DATA_HEADER);
	match scope {
		None => associated_data.push(CORE_SCOPE_TAG),
		Some(scope) => {
			associated_data.push(APP_SCOPE_TAG);
			append_u32_length_prefixed(&mut associated_data, scope.storage().as_bytes());
			append_u32_length_prefixed(&mut associated_data, scope.namespace().as_bytes());
		}
	}
	append_u32_length_prefixed(&mut associated_data, key_bytes);
	associated_data
}

/// A live record's decrypted value and its metadata.
///
/// Paranoid zeroes the plaintext buffer when it is dropped. Copies that the app makes are not
/// zeroed.
pub struct DecryptedRawValue {
	plaintext: SecretBytes,
	metadata: RecordMetadata,
}
impl DecryptedRawValue {
	pub(crate) fn new(plaintext: SecretBytes, metadata: RecordMetadata) -> Self {
		Self {
			plaintext,
			metadata,
		}
	}
	/// The decrypted value.
	pub fn plaintext(&self) -> &SecretBytes {
		&self.plaintext
	}
	/// The record's expiry and modification time.
	pub fn metadata(&self) -> RecordMetadata {
		self.metadata
	}
	/// The decrypted value and the metadata.
	pub fn into_parts(self) -> (SecretBytes, RecordMetadata) {
		(self.plaintext, self.metadata)
	}
}
impl std::fmt::Debug for DecryptedRawValue {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("DecryptedRawValue").finish_non_exhaustive()
	}
}

/// The result of an encrypted read of one record: its decrypted live value, if any, and the
/// database time of the read.
pub struct DecryptedRawObservation {
	value: Option<DecryptedRawValue>,
	observed_at: StateTimestampMicros,
}
impl DecryptedRawObservation {
	pub(crate) fn new(value: Option<DecryptedRawValue>, observed_at: StateTimestampMicros) -> Self {
		Self { value, observed_at }
	}
	/// The decrypted live value, or `None` when the record is absent or expired.
	pub fn value(&self) -> Option<&DecryptedRawValue> {
		self.value.as_ref()
	}
	/// The database time of the read.
	pub fn observed_at(&self) -> StateTimestampMicros {
		self.observed_at
	}
	/// The value and the database time.
	pub fn into_parts(self) -> (Option<DecryptedRawValue>, StateTimestampMicros) {
		(self.value, self.observed_at)
	}
}
impl std::fmt::Debug for DecryptedRawObservation {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("DecryptedRawObservation")
			.finish_non_exhaustive()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::records::RecordNamespace;
	use crate::scope::{ScopeIdentityFields, StorageIdentity};
	use primitives_rs::crypto::{Key32, keysets::derive_keyset_from_latest_first_keys};

	fn sample_keyset() -> Keyset {
		derive_keyset_from_latest_first_keys([Key32::from_array([7; 32])], "raw-encryption-test")
			.expect("a valid keyset")
	}

	fn scope_with_namespace(bytes: &[u8]) -> ScopeIdentity {
		ScopeIdentity::new(ScopeIdentityFields {
			namespace: RecordNamespace::new(bytes.to_vec()).expect("a valid namespace"),
		})
	}

	#[test]
	fn item_associated_data_encodes_explicit_scope_tag_and_length_prefixes() {
		let key = Key::from_parts(["alpha", "beta"]).expect("a valid key");
		let core_ad = item_associated_data(None, &key);
		let mut expected_core = b"paranoid.kv.raw.v1\0".to_vec();
		expected_core.push(0x00);
		expected_core.extend_from_slice(&(key.as_str().len() as u32).to_be_bytes());
		expected_core.extend_from_slice(key.as_str().as_bytes());
		assert_eq!(core_ad, expected_core);

		let ns_bytes = b"tenant\0a";
		let scope = scope_with_namespace(ns_bytes);
		let scope_ad = item_associated_data(Some(&scope), &key);
		let mut expected_scope = b"paranoid.kv.raw.v1\0".to_vec();
		expected_scope.push(0x01);
		expected_scope.extend_from_slice(&(b"paranoid.app".len() as u32).to_be_bytes());
		expected_scope.extend_from_slice(b"paranoid.app");
		expected_scope.extend_from_slice(&(ns_bytes.len() as u32).to_be_bytes());
		expected_scope.extend_from_slice(ns_bytes);
		expected_scope.extend_from_slice(&(key.as_str().len() as u32).to_be_bytes());
		expected_scope.extend_from_slice(key.as_str().as_bytes());
		assert_eq!(scope_ad, expected_scope);
	}

	#[test]
	fn cross_domain_and_cross_scope_ciphertexts_fail_authentication_with_shared_keyset() {
		let keyset = sample_keyset();
		let key = Key::from_parts(["secret", "record"]).expect("a valid key");
		let scope_a = scope_with_namespace(b"scope\0alpha");
		let scope_b = scope_with_namespace(b"scope\0beta");
		let scope_prefix = scope_with_namespace(b"scope");
		let alt_storage_scope = ScopeIdentity::from_stored(
			StorageIdentity::new(b"paranoid.app\0extra".to_vec()).expect("a valid storage"),
			RecordNamespace::new(b"alpha".to_vec()).expect("a valid namespace"),
		);
		let split_storage_scope = ScopeIdentity::from_stored(
			StorageIdentity::new(b"paranoid.app".to_vec()).expect("a valid storage"),
			RecordNamespace::new(b"\0extraalpha".to_vec()).expect("a valid namespace"),
		);

		let core = PublishedItemEncryption::new(PublishedItemEncryptionFields {
			keyset: &keyset,
			scope: None,
			key: &key,
		});
		let enc_a = PublishedItemEncryption::new(PublishedItemEncryptionFields {
			keyset: &keyset,
			scope: Some(&scope_a),
			key: &key,
		});
		let enc_b = PublishedItemEncryption::new(PublishedItemEncryptionFields {
			keyset: &keyset,
			scope: Some(&scope_b),
			key: &key,
		});
		let enc_prefix = PublishedItemEncryption::new(PublishedItemEncryptionFields {
			keyset: &keyset,
			scope: Some(&scope_prefix),
			key: &key,
		});

		let core_ciphertext = core.encrypt(b"core-payload").expect("encrypt core");
		assert_eq!(
			core.decrypt(&core_ciphertext)
				.expect("decrypt core")
				.expose_secret(),
			b"core-payload"
		);
		assert!(enc_a.decrypt(&core_ciphertext).is_err());

		let scope_a_ciphertext = enc_a.encrypt(b"scope-a-payload").expect("encrypt scope a");
		assert_eq!(
			enc_a
				.decrypt(&scope_a_ciphertext)
				.expect("decrypt scope a")
				.expose_secret(),
			b"scope-a-payload"
		);
		assert!(core.decrypt(&scope_a_ciphertext).is_err());
		assert!(enc_b.decrypt(&scope_a_ciphertext).is_err());
		assert!(enc_prefix.decrypt(&scope_a_ciphertext).is_err());

		let alt_enc = PublishedItemEncryption::new(PublishedItemEncryptionFields {
			keyset: &keyset,
			scope: Some(&alt_storage_scope),
			key: &key,
		});
		let split_enc = PublishedItemEncryption::new(PublishedItemEncryptionFields {
			keyset: &keyset,
			scope: Some(&split_storage_scope),
			key: &key,
		});
		let alt_ciphertext = alt_enc.encrypt(b"boundary").expect("encrypt alt");
		assert!(split_enc.decrypt(&alt_ciphertext).is_err());
	}
}
