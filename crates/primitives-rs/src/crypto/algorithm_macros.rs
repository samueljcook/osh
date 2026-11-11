// Shared implementations keep the algorithm modules' byte and secret contracts consistent.
macro_rules! fixed_bytes_traits {
	($name:ident, $size:ident) => {
		impl TryFrom<&[u8]> for $name {
			type Error = std::array::TryFromSliceError;
			fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
				Ok(Self::from_array(bytes.try_into()?))
			}
		}
		#[cfg(feature = "serde")]
		impl serde::Serialize for $name {
			fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
				use serde::ser::SerializeTuple;
				let mut tuple = serializer.serialize_tuple($size)?;
				for byte in self.as_bytes() {
					tuple.serialize_element(byte)?;
				}
				tuple.end()
			}
		}
		#[cfg(feature = "serde")]
		impl<'de> serde::Deserialize<'de> for $name {
			fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
				struct Visitor;
				impl<'de> serde::de::Visitor<'de> for Visitor {
					type Value = $name;
					fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
						write!(f, "exactly {} bytes", $size)
					}
					fn visit_seq<A: serde::de::SeqAccess<'de>>(
						self,
						mut sequence: A,
					) -> Result<Self::Value, A::Error> {
						let mut bytes = [0_u8; $size];
						for (index, byte) in bytes.iter_mut().enumerate() {
							*byte = sequence
								.next_element()?
								.ok_or_else(|| serde::de::Error::invalid_length(index, &self))?;
						}
						if sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {
							return Err(serde::de::Error::invalid_length($size + 1, &self));
						}
						Ok($name::from_array(bytes))
					}
				}
				deserializer.deserialize_tuple($size, Visitor)
			}
		}
	};
}

macro_rules! hash_algorithm {
	($backend:ty, $size:literal) => {
		/// The digest size in bytes.
		pub const DIGEST_LEN: usize = $size;
		/// A complete digest from this algorithm.
		#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
		pub struct Digest([u8; DIGEST_LEN]);
		fixed_bytes_traits!(Digest, DIGEST_LEN);
		impl Digest {
			/// Constructs a digest from its complete byte representation.
			pub fn from_array(bytes: [u8; DIGEST_LEN]) -> Self {
				Self(bytes)
			}
			/// Returns the complete digest bytes.
			pub fn as_bytes(&self) -> &[u8; DIGEST_LEN] {
				&self.0
			}
		}
		impl AsRef<[u8]> for Digest {
			fn as_ref(&self) -> &[u8] {
				self.as_bytes()
			}
		}
		/// An incremental hash of a byte stream.
		pub struct Hasher($backend);
		impl Hasher {
			/// Starts an empty byte stream.
			pub fn new() -> Self {
				Self(<$backend as ::sha2::Digest>::new())
			}
			/// Appends bytes without inserting separators or lengths.
			pub fn update(&mut self, bytes: &[u8]) {
				::sha2::Digest::update(&mut self.0, bytes);
			}
			/// Consumes the stream and returns its complete digest.
			pub fn finalize(self) -> Digest {
				Digest(::sha2::Digest::finalize(self.0).into())
			}
			/// Consumes the stream and writes its complete digest directly into caller-owned storage.
			pub fn finalize_into(self, out: &mut [u8; DIGEST_LEN]) {
				::sha2::digest::FixedOutput::finalize_into(self.0, out.into());
			}
		}
		impl Default for Hasher {
			fn default() -> Self {
				Self::new()
			}
		}
		impl std::fmt::Debug for Hasher {
			fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
				f.write_str("Hasher([redacted])")
			}
		}
		/// Hashes the bytes without framing.
		pub fn hash(bytes: &[u8]) -> Digest {
			hash_parts(&[bytes])
		}
		/// Hashes concatenated slices without inserting separators or lengths.
		pub fn hash_parts(parts: &[&[u8]]) -> Digest {
			let mut hasher = Hasher::new();
			for part in parts {
				hasher.update(part);
			}
			hasher.finalize()
		}
	};
}

macro_rules! hmac_algorithm {
	($backend:ty, $size:literal) => {
		/// The authentication tag size in bytes.
		pub const TAG_LEN: usize = $size;
		/// An incremental HMAC with zeroizing keyed state.
		pub struct Authenticator(::hmac::Hmac<$backend>);
		impl Authenticator {
			/// Starts a MAC with the exact supplied key bytes.
			pub fn new(key: &[u8]) -> Self {
				Self(
					<::hmac::Hmac<$backend> as ::hmac::KeyInit>::new_from_slice(key)
						.expect("HMAC accepts any key length"),
				)
			}
			/// Appends bytes without inserting separators or lengths.
			pub fn update(&mut self, bytes: &[u8]) {
				::hmac::Mac::update(&mut self.0, bytes);
			}
			/// Consumes the stream and returns this algorithm's fixed-length tag.
			pub fn finalize(self) -> [u8; TAG_LEN] {
				let output = ::hmac::Mac::finalize(self.0).into_bytes();
				let mut tag = [0; TAG_LEN];
				tag.copy_from_slice(&output[..TAG_LEN]);
				tag
			}
			/// Consumes the stream and verifies an exact-length tag in constant time.
			pub fn verify(
				self,
				tag: &[u8; TAG_LEN],
			) -> Result<(), crate::crypto::VerificationError> {
				::hmac::Mac::verify_truncated_left(self.0, tag)
					.map_err(|_| crate::crypto::VerificationError)
			}
		}
		impl std::fmt::Debug for Authenticator {
			fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
				f.write_str("Authenticator([redacted])")
			}
		}
		/// Computes this algorithm's fixed-length HMAC tag over concatenated slices without framing.
		pub fn authenticate_parts(key: &[u8], parts: &[&[u8]]) -> [u8; TAG_LEN] {
			let mut authenticator = Authenticator::new(key);
			for part in parts {
				authenticator.update(part);
			}
			authenticator.finalize()
		}
	};
}

macro_rules! hkdf_algorithm {
	($backend:ty, $size:literal) => {
		/// The maximum HKDF output size in bytes.
		pub const MAX_OUTPUT_LEN: usize = 255 * $size;
		/// Public extraction and expansion inputs from RFC 5869.
		#[derive(Clone, Copy, Debug)]
		pub struct Context<'a> {
			/// The extraction salt; an empty slice has RFC 5869's empty-salt meaning.
			pub salt: &'a [u8],
			/// The expansion information, concatenated without framing.
			pub info: &'a [&'a [u8]],
		}
		/// Extracts from input key material and expands into the caller's output buffer.
		///
		/// The caller must protect and clear the input and output. An invalid output
		/// length leaves the output unchanged. HKDF does not stretch passwords.
		pub fn derive_into(
			input_key_material: &[u8],
			context: Context<'_>,
			output: &mut [u8],
		) -> Result<(), crate::crypto::OutputLengthError> {
			if output.len() > MAX_OUTPUT_LEN {
				return Err(crate::crypto::OutputLengthError {
					actual: output.len(),
					max: MAX_OUTPUT_LEN,
				});
			}
			use ::hmac::digest::{FixedOutput, KeyInit, Update};
			type Hmac = ::hmac::Hmac<$backend>;

			// RFC 5869 extraction and expansion finalize into protected storage.
			let mut extracted_key = zeroize::Zeroizing::new([0_u8; $size]);
			let mut extract = <Hmac as KeyInit>::new_from_slice(context.salt)
				.expect("HMAC accepts every key length");
			Update::update(&mut extract, input_key_material);
			extract.finalize_into((&mut *extracted_key).into());
			let expand = <Hmac as KeyInit>::new_from_slice(&*extracted_key)
				.expect("HMAC accepts every key length");
			drop(extracted_key);

			let mut previous = zeroize::Zeroizing::new([0_u8; $size]);
			for (index, chunk) in output.chunks_mut($size).enumerate() {
				let mut block = expand.clone();
				if index != 0 {
					Update::update(&mut block, &*previous);
				}
				for info in context.info {
					Update::update(&mut block, info);
				}
				Update::update(&mut block, &[(index + 1) as u8]);
				block.finalize_into((&mut *previous).into());
				chunk.copy_from_slice(&previous[..chunk.len()]);
			}
			Ok(())
		}
	};
}
