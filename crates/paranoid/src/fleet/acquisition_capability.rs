use primitives_rs::secret_bytes::SecretToken32;
use subtle::ConstantTimeEq;

use super::{RandomnessError, fmt};

const FLEET_ACQUISITION_CAPABILITY_LEN: usize = 32;

/// Unforgeable identity for one Fleet acquisition generation.
///
/// Holder IDs are public labels and may be reused. This capability is the
/// authority-bearing material that prevents a stale handle from acting on a
/// later acquisition with the same holder and slot.
#[derive(Clone)]
pub struct AcquisitionCapability(SecretToken32);

impl PartialEq for AcquisitionCapability {
	fn eq(&self, other: &Self) -> bool {
		self.0.ct_eq(&other.0).into()
	}
}

impl Eq for AcquisitionCapability {}

impl AcquisitionCapability {
	pub fn generate() -> Result<Self, RandomnessError> {
		let token = SecretToken32::generate().map_err(RandomnessError::AcquisitionCapability)?;
		Ok(Self(token))
	}

	pub(super) fn as_bytes(&self) -> &[u8; FLEET_ACQUISITION_CAPABILITY_LEN] {
		self.0.expose_secret()
	}

	/// Reconstructs private persisted state without generating or delivering claim authority.
	pub(super) fn from_persisted_bytes(bytes: [u8; 32]) -> Self {
		Self(SecretToken32::from_array(bytes))
	}
}

impl fmt::Debug for AcquisitionCapability {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("AcquisitionCapability")
			.field("len", &FLEET_ACQUISITION_CAPABILITY_LEN)
			.finish()
	}
}
