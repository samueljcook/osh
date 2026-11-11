use crate::crypto::RandomError;

/// A message and its explicit application context.
#[derive(Clone, Copy, Debug)]
pub struct SignatureMsg<'a> {
	/// The context bytes; algorithm-specific APIs accept at most 255 bytes.
	pub context: &'a [u8],
	/// The message bytes.
	pub msg: &'a [u8],
}

/// A signature input, verification, or entropy failure.
#[derive(Debug)]
pub enum SignatureError {
	/// The encoded key, signature, or context has an invalid shape or version.
	InvalidInput,
	/// The signature does not verify under the supplied key and message.
	InvalidSignature,
	/// The platform's secure random source failed.
	Random(RandomError),
}
impl std::fmt::Display for SignatureError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(match self {
			Self::InvalidInput => "invalid signature input",
			Self::InvalidSignature => "signature verification failed",
			Self::Random(_) => "signature random source failed",
		})
	}
}
impl std::error::Error for SignatureError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::Random(error) => Some(error),
			_ => None,
		}
	}
}

#[cfg(feature = "signatures")]
pub(crate) fn check_context(input: SignatureMsg<'_>) -> Result<(), SignatureError> {
	if input.context.len() > 255 {
		Err(SignatureError::InvalidInput)
	} else {
		Ok(())
	}
}

// The supported backend RNG constructors consume exactly the supplied seed or
// randomizer bytes. Exhaustion indicates a backend-contract violation. This
// adapter performs no expansion and retains no copy of the secret material.
#[cfg(feature = "signatures")]
pub(crate) struct FixedRandomness<'a>(pub &'a [u8]);
#[cfg(feature = "signatures")]
impl rand_core::TryRng for FixedRandomness<'_> {
	type Error = std::convert::Infallible;
	fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
		let mut out = [0; 4];
		self.try_fill_bytes(&mut out)?;
		Ok(u32::from_le_bytes(out))
	}
	fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
		let mut out = [0; 8];
		self.try_fill_bytes(&mut out)?;
		Ok(u64::from_le_bytes(out))
	}
	fn try_fill_bytes(&mut self, out: &mut [u8]) -> Result<(), Self::Error> {
		let (bytes, rest) = self.0.split_at(out.len());
		out.copy_from_slice(bytes);
		self.0 = rest;
		Ok(())
	}
}
#[cfg(feature = "signatures")]
impl rand_core::TryCryptoRng for FixedRandomness<'_> {}
