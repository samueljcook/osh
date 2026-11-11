//! Complete initial scalar code binding, independent from participant active uses.

use super::{InitialInstallationMode, InstallationControlState, NativeInstallationError};
use crate::fleet::cache::epoch_state::CacheEpochComponent;
use crate::fleet::once::state::OnceCompletionComponent;
use crate::fleet::scalar::FleetScalarCode;
use crate::fleet::topic::state::TopicMetadataComponent;
use crate::fleet::v10::scalar_source_bridge::ScalarFleetSourceBridge;
use crate::installation::contract::ComponentDeclaration;

pub(super) struct InitialScalarDeclarationFields<'a, 'mode> {
	pub(super) code: &'a FleetScalarCode,
	pub(super) mode: InitialInstallationMode<'mode>,
}

/// Actual declarations borrow the same configured code throughout initial validation.
pub(super) struct InitialScalarDeclarations<'a> {
	code: &'a FleetScalarCode,
	once: ComponentDeclaration<'a, OnceCompletionComponent>,
	epoch: ComponentDeclaration<'a, CacheEpochComponent>,
	topic: ComponentDeclaration<'a, TopicMetadataComponent>,
	source: Option<ComponentDeclaration<'a, ScalarFleetSourceBridge>>,
}

impl<'a> InitialScalarDeclarations<'a> {
	pub(super) fn new(fields: InitialScalarDeclarationFields<'a, '_>) -> Self {
		Self {
			code: fields.code,
			once: fields.code.once.declaration(),
			epoch: fields.code.cache_epoch.declaration(),
			topic: fields.code.topic.declaration(),
			source: match fields.mode {
				InitialInstallationMode::NativeFresh => None,
				InitialInstallationMode::ImportedCoexisting { .. } => {
					Some(fields.code.source.declaration())
				}
			},
		}
	}

	pub(super) fn extend_required(&self, required: &mut Vec<&'static str>) {
		required.extend([
			self.once.identifier(),
			self.epoch.identifier(),
			self.topic.identifier(),
		]);
		if let Some(source) = &self.source {
			required.push(source.identifier());
		}
	}

	/// The caller has checked the entire required component set before binding it.
	pub(super) fn bind(
		&self,
		state: &InstallationControlState,
	) -> Result<(), NativeInstallationError> {
		let policy = |identifier| {
			state
				.component(identifier)
				.expect("every initial scalar component was checked in the required set")
		};
		let once = self.once.bind(policy(self.once.identifier()))?;
		self.code.once.bind_schema(&once)?;
		let epoch = self.epoch.bind(policy(self.epoch.identifier()))?;
		self.code.cache_epoch.bind_schema(&epoch)?;
		let topic = self.topic.bind(policy(self.topic.identifier()))?;
		self.code.topic.bind_sequence(&topic)?;
		self.code.topic.bind_cursor(&topic)?;
		if let Some(source) = &self.source {
			let bound = source.bind(policy(source.identifier()))?;
			self.code.source.bind(&bound)?;
		}
		Ok(())
	}
}
