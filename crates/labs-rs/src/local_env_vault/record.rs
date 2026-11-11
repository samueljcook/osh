//! Version 1 stores an object with `body` and `authentication_tag` fields.
//! The authenticated body has fields in this order: `version`, `vault_id`,
//! `encrypted_master_key`, and `entries`. Its compact Serde JSON representation
//! uses sorted entry names. Names, format metadata, and ciphertext are public;
//! all binary fields use canonical unpadded Base64URL. The outer JSON may use
//! arbitrary whitespace and field ordering, but duplicate fields are rejected.
//!
//! A 16-byte random vault identity binds the password-encrypted key and individual
//! values. One random 32-byte master key supplies a purpose-bound keyset, with
//! separate child keysets for value encryption and complete-record authentication.
//! The record MAC covers the compact body as one framed field. A password change
//! generates a new master key, encrypts every value again under keys derived from
//! it, and replaces the password-encrypted key and MAC. It retains the vault identity.

use super::{
	EnvironmentVariableName, Key32, Keyset, SecretBytes, VaultError, VaultLimits,
	VaultPasswordOptions, validate_value,
};
use primitives_rs::{
	crypto::{
		self,
		encrypted_value::EncryptedValue,
		hash, mac,
		password_derivation::Input,
		password_encrypted_key::{
			PasswordDecryptInput, PasswordEncryptInput, PasswordEncryptedKey,
		},
	},
	encoding::binary_to_text::{ByteEncoding, DecodeOptions},
	random::secure_bytes::generate_random_byte_array,
	serialization::json::{self, JsonFormat, JsonLimits, SerializeOptions},
};
use serde::{
	Deserialize, Deserializer, Serialize,
	de::{MapAccess, Visitor},
};
use std::{collections::BTreeMap, fmt, sync::Arc};
use zeroize::Zeroize;

const FORMAT_VERSION: u32 = 1;
const ENCODING: ByteEncoding = ByteEncoding::Base64UrlUnpadded;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StoredVault {
	pub body: VaultRecordBody,
	authentication_tag: String,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct VaultRecordBody {
	version: u32,
	vault_id: String,
	encrypted_master_key: String,
	#[serde(deserialize_with = "deserialize_unique_entries")]
	pub entries: BTreeMap<String, String>,
}
pub(super) struct VaultKeysets {
	encryption: Keyset,
	authentication: Keyset,
}
/// A record and the keys that verified or produced its authentication tag.
/// Values decrypt only with those keys, and each change produces another
/// authenticated record.
pub(super) struct AuthenticatedVault {
	record: StoredVault,
	keys: Arc<VaultKeysets>,
}
impl VaultKeysets {
	pub fn new(master_key: &Key32) -> Result<Self, VaultError> {
		let mut master_key_copy = *master_key.expose_secret();
		let root_key = Key32::from_array(master_key_copy);
		master_key_copy.zeroize();
		let root = crypto::keysets::derive_keyset_from_latest_first_keys(
			[root_key],
			"labs-rs.local-env-vault.v1",
		)
		.map_err(|_| VaultError::Crypto)?;
		Ok(Self {
			encryption: root
				.derive_child_keyset("environment-values")
				.map_err(|_| VaultError::Crypto)?,
			authentication: root
				.derive_child_keyset("record-authentication")
				.map_err(|_| VaultError::Crypto)?,
		})
	}
}
impl VaultRecordBody {
	/// Creates an empty body with a new vault identity.
	pub fn create(
		master_key: &Key32,
		password: &[u8],
		options: &VaultPasswordOptions,
	) -> Result<Self, VaultError> {
		let id = generate_random_byte_array::<16>().map_err(|_| VaultError::Crypto)?;
		Self::new(ENCODING.encode(&id), master_key, password, options)
	}

	fn new(
		vault_id: String,
		master_key: &Key32,
		password: &[u8],
		options: &VaultPasswordOptions,
	) -> Result<Self, VaultError> {
		let encrypted_master_key = encrypt_master_key(
			master_key,
			password,
			options,
			&key_context(FORMAT_VERSION, &vault_id),
		)?;
		Ok(Self {
			version: FORMAT_VERSION,
			vault_id,
			encrypted_master_key,
			entries: BTreeMap::new(),
		})
	}

	pub fn authenticate(
		self,
		keys: Arc<VaultKeysets>,
		limits: VaultLimits,
	) -> Result<AuthenticatedVault, VaultError> {
		let body = self.body_bytes(limits)?;
		let authentication_tag =
			ENCODING.encode(mac::authenticate_fields(&keys.authentication, &[&body]).as_bytes());
		Ok(AuthenticatedVault {
			record: StoredVault {
				body: self,
				authentication_tag,
			},
			keys,
		})
	}

	fn set_value(
		&mut self,
		keyset: &Keyset,
		name: &EnvironmentVariableName,
		plaintext: &[u8],
	) -> Result<(), VaultError> {
		let encrypted = EncryptedValue::encrypt_bytes(
			keyset,
			crypto::encryption::EncryptInput {
				plaintext,
				associated_data: self.value_context(name).as_ref(),
			},
		)
		.map_err(|_| VaultError::Crypto)?;
		self.entries.insert(
			name.as_str().to_owned(),
			ENCODING.encode(encrypted.as_bytes()),
		);
		Ok(())
	}

	fn body_bytes(&self, limits: VaultLimits) -> Result<Vec<u8>, VaultError> {
		Ok(json::serialize_to_vec(
			self,
			SerializeOptions {
				limits: json_limits(limits),
				format: JsonFormat::Compact,
			},
		)?)
	}
	fn parse_key_record(&self) -> Result<PasswordEncryptedKey, VaultError> {
		let bytes = decode_array::<{ crypto::password_encrypted_key::RECORD_LEN }>(
			&self.encrypted_master_key,
		)?;
		PasswordEncryptedKey::parse(&bytes).map_err(|_| VaultError::InvalidRecord)
	}
	fn key_context(&self) -> hash::Digest {
		key_context(self.version, &self.vault_id)
	}

	fn value_context(&self, name: &EnvironmentVariableName) -> hash::Digest {
		hash::hash_fields(hash::Fields {
			purpose: b"labs-rs/local-env-vault/value/v1",
			fields: &[
				&self.version.to_be_bytes(),
				self.vault_id.as_bytes(),
				name.as_str().as_bytes(),
			],
		})
	}
}

impl StoredVault {
	pub fn check_structure(
		&self,
		options: &VaultPasswordOptions,
		limits: VaultLimits,
	) -> Result<(), VaultError> {
		if self.body.version != FORMAT_VERSION || self.body.entries.len() > limits.max_entry_count {
			return Err(VaultError::InvalidRecord);
		}
		decode_array::<16>(&self.body.vault_id)?;
		decode_array::<{ mac::TAG_LEN }>(&self.authentication_tag)?;
		let key_record = self.body.parse_key_record()?;
		if key_record.requires_additional_secret() {
			return Err(VaultError::InvalidRecord);
		}
		options
			.limits
			.check(key_record.parameters().profile(), 0)
			.map_err(|_| VaultError::PasswordPolicy)?;
		for (name, ciphertext) in &self.body.entries {
			EnvironmentVariableName::new(name.clone()).map_err(|_| VaultError::InvalidRecord)?;
			parse_envelope(ciphertext)?;
		}
		Ok(())
	}
	/// Decrypts the master key with `password` and verifies the record tag with
	/// keys derived from it before any value can be decrypted.
	pub fn unlock(
		self,
		password: &[u8],
		options: &VaultPasswordOptions,
		limits: VaultLimits,
	) -> Result<AuthenticatedVault, VaultError> {
		let master_key = self
			.body
			.parse_key_record()?
			.decrypt(PasswordDecryptInput {
				password: Input {
					password,
					additional_secret: None,
				},
				limits: options.limits,
				context: self.body.key_context().as_ref(),
			})?;
		let keys = VaultKeysets::new(&master_key)?;
		let body = self.body.body_bytes(limits)?;
		let tag = mac::Tag::from_array(decode_array(&self.authentication_tag)?);
		mac::verify_fields(&keys.authentication, &[&body], &tag)
			.map_err(|_| VaultError::AuthenticationFailed)?;
		Ok(AuthenticatedVault {
			record: self,
			keys: Arc::new(keys),
		})
	}
}

impl AuthenticatedVault {
	pub fn record(&self) -> &StoredVault {
		&self.record
	}
	pub fn body(&self) -> &VaultRecordBody {
		&self.record.body
	}
	pub fn decrypt_value(
		&self,
		name: &EnvironmentVariableName,
		limits: VaultLimits,
	) -> Result<SecretBytes, VaultError> {
		let body = self.body();
		let ciphertext = body
			.entries
			.get(name.as_str())
			.ok_or(VaultError::MissingValues)?;
		let value = parse_envelope(ciphertext)?
			.decrypt_bytes(&self.keys.encryption, body.value_context(name).as_ref())
			.map_err(|_| VaultError::AuthenticationFailed)?
			.value;
		validate_value(value.expose_secret(), limits)?;
		Ok(value)
	}
	/// Returns a record that stores `plaintext` under `name` with the same keys.
	pub fn with_value(
		&self,
		name: &EnvironmentVariableName,
		plaintext: &[u8],
		limits: VaultLimits,
	) -> Result<Self, VaultError> {
		let mut body = self.body().clone();
		body.set_value(&self.keys.encryption, name, plaintext)?;
		if body.entries.len() > limits.max_entry_count {
			return Err(VaultError::EntryLimitExceeded);
		}
		body.authenticate(Arc::clone(&self.keys), limits)
	}
	/// Returns a record without the entries for `names`, with the same keys.
	pub fn without_values(
		&self,
		names: &[EnvironmentVariableName],
		limits: VaultLimits,
	) -> Result<Self, VaultError> {
		let mut body = self.body().clone();
		for name in names {
			body.entries.remove(name.as_str());
		}
		body.authenticate(Arc::clone(&self.keys), limits)
	}
	/// Returns a record with the same vault identity and values under a new master
	/// key, which `password` protects with the configured costs. Keys from this
	/// record neither decrypt values in nor authenticate the returned record.
	pub fn with_new_master_key(
		&self,
		password: &[u8],
		options: &VaultPasswordOptions,
		limits: VaultLimits,
	) -> Result<Self, VaultError> {
		let master_key = Key32::generate().map_err(|_| VaultError::Crypto)?;
		let keys = VaultKeysets::new(&master_key)?;
		let current = self.body();
		let mut body =
			VaultRecordBody::new(current.vault_id.clone(), &master_key, password, options)?;
		for name in current.entries.keys() {
			let name = EnvironmentVariableName::new(name.clone())
				.map_err(|_| VaultError::InvalidRecord)?;
			let value = self.decrypt_value(&name, limits)?;
			body.set_value(&keys.encryption, &name, value.expose_secret())?;
		}
		body.authenticate(Arc::new(keys), limits)
	}
}

fn key_context(version: u32, vault_id: &str) -> hash::Digest {
	hash::hash_fields(hash::Fields {
		purpose: b"labs-rs/local-env-vault/master-key/v1",
		fields: &[&version.to_be_bytes(), vault_id.as_bytes()],
	})
}
fn encrypt_master_key(
	key: &Key32,
	password: &[u8],
	options: &VaultPasswordOptions,
	context: &hash::Digest,
) -> Result<String, VaultError> {
	let encrypted = PasswordEncryptedKey::encrypt(
		key,
		PasswordEncryptInput {
			password: Input {
				password,
				additional_secret: None,
			},
			profile: options.argon2id_profile.clone(),
			limits: options.limits,
			context: context.as_ref(),
		},
	)?;
	Ok(ENCODING.encode(encrypted.as_bytes()))
}

pub(super) fn json_limits(limits: VaultLimits) -> JsonLimits {
	JsonLimits {
		max_size: limits.max_file_size,
		max_depth: 4,
	}
}
fn decode_array<const N: usize>(text: &str) -> Result<[u8; N], VaultError> {
	let mut bytes = [0; N];
	if ENCODING
		.decode_into(text, &mut bytes)
		.map_err(|_| VaultError::InvalidRecord)?
		!= N
	{
		return Err(VaultError::InvalidRecord);
	}
	Ok(bytes)
}
fn parse_envelope(text: &str) -> Result<EncryptedValue, VaultError> {
	let bytes = ENCODING
		.decode(
			text,
			DecodeOptions {
				max_decoded_size: crypto::encryption::MAX_ENVELOPE_LEN,
			},
		)
		.map_err(|_| VaultError::InvalidRecord)?;
	EncryptedValue::try_from(bytes).map_err(|_| VaultError::InvalidRecord)
}
fn deserialize_unique_entries<'de, D: Deserializer<'de>>(
	deserializer: D,
) -> Result<BTreeMap<String, String>, D::Error> {
	struct UniqueEntriesVisitor;
	impl<'de> Visitor<'de> for UniqueEntriesVisitor {
		type Value = BTreeMap<String, String>;
		fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
			formatter.write_str("an object with unique variable names")
		}
		fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
			let mut entries = BTreeMap::new();
			while let Some((name, value)) = map.next_entry::<String, String>()? {
				if entries.insert(name, value).is_some() {
					return Err(serde::de::Error::custom("duplicate vault entry"));
				}
			}
			Ok(entries)
		}
	}
	deserializer.deserialize_map(UniqueEntriesVisitor)
}
