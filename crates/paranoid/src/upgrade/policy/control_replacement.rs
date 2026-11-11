//! A draft of a replacement control state, built from the installed state.

use crate::installation::control_state::{
	InstallationControlError, InstallationControlFields, InstallationControlState,
};

/// The installed control state's fields, which an upgrade step changes before it records the
/// replacement state. The draft supplies no admission, generation, inspection, or transaction
/// authority.
pub(in crate::upgrade) struct EffectiveControlReplacement {
	fields: InstallationControlFields,
}

impl EffectiveControlReplacement {
	pub(crate) fn new(installed: InstallationControlState) -> Self {
		Self {
			fields: installed.into_fields(),
		}
	}

	pub(in crate::upgrade) fn fields_mut(&mut self) -> &mut InstallationControlFields {
		&mut self.fields
	}

	/// Checks the changed fields and builds the replacement state.
	pub(crate) fn finish(
		self,
	) -> Result<InstallationControlState, EffectiveControlReplacementError> {
		InstallationControlState::new(self.fields)
			.map_err(|error| EffectiveControlReplacementError::State(Box::new(error)))
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum EffectiveControlReplacementError {
	#[error("replacement control state cannot be constructed")]
	State(#[source] Box<InstallationControlError>),
}
