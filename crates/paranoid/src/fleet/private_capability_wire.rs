//! Zeroizing private text and exact native acquisition-capability wire conversion.

use primitives_rs::encoding::binary_to_text::ByteEncoding;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use zeroize::Zeroizing;

use super::AcquisitionCapability;

pub(super) struct SecretText(pub(super) Zeroizing<String>);

impl<'de> Deserialize<'de> for SecretText {
	fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
		String::deserialize(deserializer).map(|text| Self(Zeroizing::new(text)))
	}
}
impl Serialize for SecretText {
	fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		self.0.as_str().serialize(serializer)
	}
}

pub(super) fn decode_capability(
	text: &str,
) -> Result<AcquisitionCapability, PrivateCapabilityWireError> {
	if text.len() != 64 {
		return Err(PrivateCapabilityWireError::Length);
	}
	let mut bytes = Zeroizing::new([0_u8; 32]);
	ByteEncoding::HexLower
		.decode_into(text, &mut *bytes)
		.map_err(|_| PrivateCapabilityWireError::LowerHex)?;
	Ok(AcquisitionCapability::from_persisted_bytes(*bytes))
}

pub(super) fn encode_capability(capability: &AcquisitionCapability) -> SecretText {
	SecretText(ByteEncoding::HexLower.encode_zeroizing(capability.as_bytes()))
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PrivateCapabilityWireError {
	#[error("private native capability must contain exactly 32 encoded bytes")]
	Length,
	#[error("private native capability must use lowercase hexadecimal")]
	LowerHex,
}
