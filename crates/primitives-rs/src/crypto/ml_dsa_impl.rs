macro_rules! define_ml_dsa {
	($params:ty, $public_len:expr, $signature_len:expr) => {
		use crate::crypto::{
			Key32, SignatureError, SignatureMsg,
			signature_support::{FixedRandomness, check_context},
		};
		use ml_dsa::{KeyExport, signature::Keypair};
		use zeroize::Zeroizing;
		/// The FIPS 204 public-key length in bytes.
		pub const PUBLIC_KEY_LEN: usize = $public_len;
		/// The FIPS 204 signature length in bytes.
		pub const SIGNATURE_LEN: usize = $signature_len;
		/// A signing key with zeroizing secret storage and redacted diagnostics.
		pub struct SigningKey(ml_dsa::SigningKey<$params>);
		impl SigningKey {
			/// Derives a key from the exact 32-byte FIPS 204 seed.
			pub fn from_seed(seed: &Key32) -> Self {
				let bytes = Zeroizing::new(ml_dsa::Seed::from(*seed.expose_secret()));
				Self(ml_dsa::SigningKey::from_seed(&bytes))
			}
			/// Returns this signing key's public verification key.
			pub fn verifying_key(&self) -> VerifyingKey {
				VerifyingKey(self.0.verifying_key())
			}
			/// Signs with fresh platform randomness and an explicit FIPS context.
			pub fn sign(&self, input: SignatureMsg<'_>) -> Result<Signature, SignatureError> {
				check_context(input)?;
				let mut randomizer = Zeroizing::new([0; 32]);
				crate::random::secure_bytes::fill_random_bytes(randomizer.as_mut())
					.map_err(SignatureError::Random)?;
				self.0
					.expanded_key()
					.sign_randomized(
						input.msg,
						input.context,
						&mut FixedRandomness(randomizer.as_ref()),
					)
					.map(Signature)
					.map_err(|_| SignatureError::InvalidInput)
			}
		}
		impl std::fmt::Debug for SigningKey {
			fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
				f.write_str("SigningKey([REDACTED])")
			}
		}
		/// A parsed FIPS 204 public verification key.
		#[derive(Clone, Debug, PartialEq)]
		pub struct VerifyingKey(ml_dsa::VerifyingKey<$params>);
		impl Eq for VerifyingKey {}
		impl VerifyingKey {
			/// Parses an exact-length FIPS 204 public key.
			pub fn parse(bytes: &[u8]) -> Result<Self, SignatureError> {
				let encoded = ml_dsa::EncodedVerifyingKey::<$params>::try_from(bytes)
					.map_err(|_| SignatureError::InvalidInput)?;
				Ok(Self(ml_dsa::VerifyingKey::decode(&encoded)))
			}
			/// Encodes this public key using FIPS 204.
			pub fn to_bytes(&self) -> [u8; PUBLIC_KEY_LEN] {
				self.0
					.to_bytes()
					.as_slice()
					.try_into()
					.expect("ML-DSA public-key size")
			}
			/// Verifies the message, context, and signature together.
			pub fn verify(
				&self,
				input: SignatureMsg<'_>,
				signature: &Signature,
			) -> Result<(), SignatureError> {
				check_context(input)?;
				if self
					.0
					.verify_with_context(input.msg, input.context, &signature.0)
				{
					Ok(())
				} else {
					Err(SignatureError::InvalidSignature)
				}
			}
		}
		/// A structurally parsed FIPS 204 detached signature.
		#[derive(Clone, Debug, PartialEq)]
		pub struct Signature(ml_dsa::Signature<$params>);
		impl Eq for Signature {}
		impl Signature {
			/// Rejects malformed length and noncanonical FIPS 204 encodings.
			pub fn parse(bytes: &[u8]) -> Result<Self, SignatureError> {
				let encoded = ml_dsa::EncodedSignature::<$params>::try_from(bytes)
					.map_err(|_| SignatureError::InvalidInput)?;
				ml_dsa::Signature::decode(&encoded)
					.map(Self)
					.ok_or(SignatureError::InvalidInput)
			}
			/// Encodes this signature using FIPS 204.
			pub fn to_bytes(&self) -> [u8; SIGNATURE_LEN] {
				self.0
					.encode()
					.as_slice()
					.try_into()
					.expect("ML-DSA signature size")
			}
		}
	};
}
pub(crate) use define_ml_dsa;
