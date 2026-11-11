use super::{
	ClaimDurationError, CoordinationKeyError, HolderIdError, HolderPrefixError,
	LEASE_KEY_SEPARATOR, LEASE_TOKEN_LEN, MAX_HOLDER_ID_LEN, MAX_LEASE_KEY_LEN, MIN_CLAIM_DURATION,
	PersistedLeaseError, RandomnessError, StdDuration,
};
use primitives_rs::crypto::algorithms::blake3;
use primitives_rs::encoding::binary_to_text::ByteEncoding;
use primitives_rs::id::time_sortable as id;
use primitives_rs::secret_bytes::SecretToken32;
use primitives_rs::time::duration_to_ceil_micros_i64;
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

/// Validated persisted coordination key.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LeaseKey(pub(super) String);

/// Diagnostic label on a coordination claim, Semaphore slot or Throttler permit.
///
/// [`HolderPrefix::generate_holder_id`] generates a label for one acquisition from an explicit
/// prefix and a new sortable identifier. Labels and snapshots grant no authority; private
/// acquisition capabilities authorize state changes.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HolderId(pub(super) String);

/// Validated name of the process that acquires Fleet claims.
///
/// Every holder identifier that a Fleet store generates starts with its prefix. The key of a
/// claim says what is held, so the prefix says which process holds it, such as an instance
/// name or a pod name.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HolderPrefix(String);

/// Positive lease ownership generation.
///
/// A claim of a key that follows an expired or released claim of the same key gets a larger
/// token than every earlier claim of that key, and a renewal keeps the token of its claim.
/// Paranoid cannot stop a holder from continuing its work after its claim expires. An external
/// resource that must reject such a holder must receive the token with every request, must
/// remember the largest token that it has seen for the key, and must reject a request that
/// carries a smaller token. The claims of a lease, a mutex, a once task, a cron, and a cache
/// computation expose this token. A semaphore slot and a throttler slot have none.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FencingToken(pub(super) i64);

#[derive(Clone, Debug)]
pub(crate) struct Token(SecretToken32);

/// Validated coordination claim duration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClaimDuration {
	positive_duration: StdDuration,
	positive_micros: i64,
}

/// Current coordination claim returned by a successful claim or renewal.
#[derive(Debug, Eq, PartialEq)]
pub struct Claim {
	pub(super) key: LeaseKey,
	pub(super) holder_id: HolderId,
	pub(super) fencing_token: FencingToken,
	pub(super) lease_token: Token,
	pub(super) expires_at_unix_micros: i64,
}

/// Non-secret view of the current live holder for a coordination key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HolderSnapshot {
	pub(super) key: LeaseKey,
	pub(super) holder_id: HolderId,
	pub(super) fencing_token: FencingToken,
	pub(super) expires_at_unix_micros: i64,
}

impl LeaseKey {
	/// Validates and joins key parts into a persisted coordination key.
	pub fn from_parts<S, I>(parts: I) -> Result<Self, CoordinationKeyError>
	where
		S: AsRef<str>,
		I: IntoIterator<Item = S>,
	{
		Ok(Self(build_key_from_parts(parts)?))
	}

	/// Returns the persisted key text.
	pub fn as_str(&self) -> &str {
		&self.0
	}

	/// Builds the lease key of an identity that Paranoid derives, such as a Cache entry or a
	/// Once. The identity keeps the key of `parts`, its present key, whenever that key is at most
	/// `MAX_LEASE_KEY_LEN` bytes long because deployments of different versions must use one key
	/// for one identity. An identity with a longer present key gets the key of `digest_label` and
	/// the lowercase hex BLAKE3 digest of the bytes that `framed_identity_fn` returns.
	pub(crate) fn from_parts_or_digest<S, I, B>(
		parts: I,
		digest_label: &'static str,
		framed_identity_fn: impl FnOnce() -> B,
	) -> Result<Self, CoordinationKeyError>
	where
		S: AsRef<str>,
		I: IntoIterator<Item = S>,
		B: AsRef<[u8]>,
	{
		match Self::from_parts(parts) {
			Err(CoordinationKeyError::TooLong { .. }) => {
				let digest = blake3::hash(framed_identity_fn().as_ref());
				Self::from_parts([
					digest_label,
					ByteEncoding::HexLower.encode(digest.as_bytes()).as_str(),
				])
			}
			present => present,
		}
	}
}

impl HolderId {
	/// Validates and copies a coordination holder identifier.
	pub(crate) fn new(input: impl AsRef<str>) -> Result<Self, HolderIdError> {
		validate_holder_id(input.as_ref())?;
		Ok(Self(input.as_ref().to_owned()))
	}

	/// Validates and copies holder text that Fleet did not generate. Tests use it to exercise
	/// claims that share a label.
	#[cfg(feature = "__internal")]
	#[doc(hidden)]
	pub fn from_text_for_tests(input: impl AsRef<str>) -> Result<Self, HolderIdError> {
		Self::new(input)
	}

	/// Returns the holder identifier text.
	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl HolderPrefix {
	/// Validates and copies the name of the process.
	///
	/// The name must be non-empty, must not contain null bytes, and must leave room for the
	/// separator and a sortable identifier within a holder ID's limit of 512 bytes.
	pub fn new(input: impl AsRef<str>) -> Result<Self, HolderPrefixError> {
		let input = input.as_ref();
		if input.is_empty() {
			return Err(HolderPrefixError::Empty);
		}
		if input.as_bytes().contains(&0) {
			return Err(HolderPrefixError::ContainsNullByte);
		}
		if input.len() > MAX_HOLDER_PREFIX_LEN {
			return Err(HolderPrefixError::TooLong {
				actual: input.len(),
				max: MAX_HOLDER_PREFIX_LEN,
			});
		}
		Ok(Self(input.to_owned()))
	}

	/// Returns the prefix text.
	pub fn as_str(&self) -> &str {
		&self.0
	}

	/// Generates a holder identifier for one acquisition: the prefix, a separator, and a new
	/// sortable identifier.
	pub fn generate_holder_id(&self) -> Result<HolderId, RandomnessError> {
		let id = id::SortableId::generate().map_err(RandomnessError::HolderId)?;
		let holder_id = format!("{}{HOLDER_SEPARATOR}{}", self.0, id.to_text());
		Ok(HolderId::new(holder_id)
			.expect("a valid prefix, a separator, and a sortable identifier form a valid holder"))
	}
}

/// Separates a holder prefix from the sortable identifier of one acquisition.
const HOLDER_SEPARATOR: &str = ":";

/// Maximum accepted holder prefix length, in bytes.
pub const MAX_HOLDER_PREFIX_LEN: usize =
	MAX_HOLDER_ID_LEN - HOLDER_SEPARATOR.len() - id::SORTABLE_ID_TEXT_LEN;

impl FencingToken {
	/// Returns the positive fencing token as a signed Postgres integer.
	pub fn as_i64(self) -> i64 {
		self.0
	}

	pub(crate) fn from_i64(value: i64) -> Result<Self, PersistedLeaseError> {
		if value <= 0 {
			return Err(PersistedLeaseError::FencingToken { value });
		}
		Ok(Self(value))
	}
}

impl Token {
	pub(super) fn generate() -> Result<Self, RandomnessError> {
		SecretToken32::generate()
			.map(Self)
			.map_err(RandomnessError::LeaseToken)
	}

	pub(super) fn from_bytes(bytes: [u8; LEASE_TOKEN_LEN]) -> Self {
		Self(SecretToken32::from_array(bytes))
	}

	pub(super) fn from_persisted_bytes(bytes: Vec<u8>) -> Result<Self, PersistedLeaseError> {
		let bytes = Zeroizing::new(bytes);
		SecretToken32::from_slice(&bytes)
			.map(Self)
			.map_err(|_| PersistedLeaseError::TokenLength {
				actual: bytes.len(),
				expected: LEASE_TOKEN_LEN,
			})
	}

	pub(super) fn as_bytes(&self) -> &[u8; LEASE_TOKEN_LEN] {
		self.0.expose_secret()
	}
}

impl PartialEq for Token {
	fn eq(&self, other: &Self) -> bool {
		self.0.ct_eq(&other.0).into()
	}
}

impl Eq for Token {}

impl ClaimDuration {
	/// Validates a positive coordination claim duration.
	pub fn new(duration: StdDuration) -> Result<Self, ClaimDurationError> {
		if duration.is_zero() {
			return Err(ClaimDurationError::Zero);
		}
		if duration < MIN_CLAIM_DURATION {
			return Err(ClaimDurationError::below_minimum());
		}
		let positive_micros = duration_to_rounded_micros(duration)?;
		Ok(Self {
			positive_duration: duration,
			positive_micros,
		})
	}

	/// Returns the validated positive coordination claim duration.
	pub fn as_duration(self) -> StdDuration {
		self.positive_duration
	}

	pub(super) fn positive_micros(self) -> i64 {
		self.positive_micros
	}
}

impl Claim {
	/// Returns the coordination key.
	pub fn key(&self) -> &LeaseKey {
		&self.key
	}

	/// Returns the holder identifier.
	pub fn holder_id(&self) -> &HolderId {
		&self.holder_id
	}

	/// Returns this claim's fencing token.
	pub fn fencing_token(&self) -> FencingToken {
		self.fencing_token
	}

	/// Returns the claim expiration timestamp as Unix microseconds.
	pub fn expires_at_unix_micros(&self) -> i64 {
		self.expires_at_unix_micros
	}
}

impl HolderSnapshot {
	/// Returns the coordination key.
	pub fn key(&self) -> &LeaseKey {
		&self.key
	}

	/// Returns the holder identifier.
	pub fn holder_id(&self) -> &HolderId {
		&self.holder_id
	}

	/// Returns the holder's fencing token.
	pub fn fencing_token(&self) -> FencingToken {
		self.fencing_token
	}

	/// Returns the holder snapshot expiration timestamp as Unix microseconds.
	pub fn expires_at_unix_micros(&self) -> i64 {
		self.expires_at_unix_micros
	}
}

fn build_key_from_parts<S, I>(parts: I) -> Result<String, CoordinationKeyError>
where
	S: AsRef<str>,
	I: IntoIterator<Item = S>,
{
	let mut parts = parts.into_iter();
	let first = parts.next().ok_or(CoordinationKeyError::Empty)?;
	let first = first.as_ref();
	validate_key_part(first)?;
	let mut key = first.to_owned();

	for part in parts {
		let part = part.as_ref();
		validate_key_part(part)?;
		key.push_str(LEASE_KEY_SEPARATOR);
		key.push_str(part);
	}

	key.push_str(LEASE_KEY_SEPARATOR);
	validate_key_length(&key)?;
	Ok(key)
}

fn validate_key_part(part: &str) -> Result<(), CoordinationKeyError> {
	if part.is_empty() {
		return Err(CoordinationKeyError::EmptyPart);
	}
	if part.as_bytes().contains(&b':') {
		return Err(CoordinationKeyError::PartContainsSeparatorByte);
	}
	if part.as_bytes().contains(&0) {
		return Err(CoordinationKeyError::PartContainsNullByte);
	}
	Ok(())
}

fn validate_key_length(key: &str) -> Result<(), CoordinationKeyError> {
	let actual = key.len();
	if actual > MAX_LEASE_KEY_LEN {
		return Err(CoordinationKeyError::too_long(actual));
	}
	Ok(())
}

fn validate_holder_id(holder_id: &str) -> Result<(), HolderIdError> {
	if holder_id.is_empty() {
		return Err(HolderIdError::Empty);
	}
	if holder_id.as_bytes().contains(&0) {
		return Err(HolderIdError::ContainsNullByte);
	}
	let actual = holder_id.len();
	if actual > MAX_HOLDER_ID_LEN {
		return Err(HolderIdError::too_long(actual));
	}
	Ok(())
}

pub(super) fn duration_to_rounded_micros(duration: StdDuration) -> Result<i64, ClaimDurationError> {
	duration_to_ceil_micros_i64(duration).map_err(|_| ClaimDurationError::TooLarge)
}
