//! Actual scalar code, configured placement and explicit use intent for one runtime.

use std::sync::Arc;

use crate::scope::declarations::InitialUse;

use crate::fleet::scalar::FleetScalarCode;
use crate::fleet::scalar_codec::ScalarStateError;
use crate::records::postgres::v10::relocating_point::PgRelocatingPointStorage;
use crate::runtime::admission::{NativeAdmissionError, NativeAdmittedContracts};
use crate::runtime::participation::state::ParticipantComponentSupport;
use crate::runtime::participation::state::catalog::ParticipantUseReference;

/// Which Fleet families a runtime uses actively when it starts.
pub struct FleetUseIntent {
	/// Once.
	pub once: InitialUse,
	/// Cache invalidation epochs.
	pub cache_epoch: InitialUse,
	/// Topic sequences and subscription cursors.
	pub topic: InitialUse,
}
pub(super) struct NativeRuntimeFleetScalarFields {
	pub(super) uses: FleetUseIntent,
}
pub(super) struct NativeRuntimeFleetScalarDefinition {
	code: Arc<FleetScalarCode>,
	uses: FleetUseIntent,
}

pub(super) struct NativeRuntimeFleetScalar {
	definition: NativeRuntimeFleetScalarDefinition,
	/// The v10 Fleet state adapter, which is present when the runtime reads and writes v10
	/// Fleet state.
	source: Option<PgRelocatingPointStorage>,
}
impl NativeRuntimeFleetScalarDefinition {
	pub(super) fn new(fields: NativeRuntimeFleetScalarFields) -> Self {
		Self {
			code: Arc::new(FleetScalarCode::new()),
			uses: fields.uses,
		}
	}
	pub(super) fn into_runtime(
		self,
		source: Option<PgRelocatingPointStorage>,
	) -> NativeRuntimeFleetScalar {
		NativeRuntimeFleetScalar {
			definition: self,
			source,
		}
	}
}

impl NativeRuntimeFleetScalar {
	pub(super) fn code(&self) -> &Arc<FleetScalarCode> {
		&self.definition.code
	}
	pub(super) fn source(&self) -> Option<&PgRelocatingPointStorage> {
		self.source.as_ref()
	}
	pub(super) fn register_support_and_uses(
		&self,
		support: &mut Vec<ParticipantComponentSupport>,
		uses: &mut Vec<ParticipantUseReference>,
	) {
		let declared = self.definition.code.support();
		let active = [
			self.definition.uses.once == InitialUse::Active,
			self.definition.uses.cache_epoch == InitialUse::Active,
			self.definition.uses.topic == InitialUse::Active,
			self.source.is_some(),
		];
		for (component, is_active) in declared.into_iter().zip(active) {
			if is_active {
				uses.push(ParticipantUseReference::Component {
					identifier: component.identifier().to_owned(),
				});
			}
			support.push(component);
		}
	}
	pub(super) fn require_active_policies(
		&self,
		contracts: &NativeAdmittedContracts,
	) -> Result<(), NativeRuntimeFleetScalarError> {
		if self.definition.uses.once == InitialUse::Active {
			let declaration = self.definition.code.once.declaration();
			let bound = contracts.bind(&declaration)?;
			self.definition.code.once.bind_schema(&bound)?;
		}
		if self.definition.uses.cache_epoch == InitialUse::Active {
			let declaration = self.definition.code.cache_epoch.declaration();
			let bound = contracts.bind(&declaration)?;
			self.definition.code.cache_epoch.bind_schema(&bound)?;
		}
		if self.definition.uses.topic == InitialUse::Active {
			let declaration = self.definition.code.topic.declaration();
			let bound = contracts.bind(&declaration)?;
			self.definition.code.topic.bind_sequence(&bound)?;
			self.definition.code.topic.bind_cursor(&bound)?;
		}
		// Only an imported installation has a v10 storage route.
		if matches!(
			contracts.storage_route(),
			super::InstallationStorageRoute::SourceCoexistence
				| super::InstallationStorageRoute::SourceRelocation
		) {
			let declaration = self.definition.code.source.declaration();
			let bound = contracts.bind(&declaration)?;
			self.definition.code.source.bind(&bound)?;
		}
		Ok(())
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeRuntimeFleetScalarError {
	#[error("native runtime Fleet scalar active use cannot bind its admitted policy")]
	Admission(#[from] NativeAdmissionError),
	#[error("native runtime Fleet scalar active use cannot bind its state code")]
	State(#[from] ScalarStateError),
}
