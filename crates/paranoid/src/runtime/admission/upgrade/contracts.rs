//! Fixed maintenance contexts borrow held current control and own their authenticated participant.

use std::sync::Arc;

use crate::scope::{PrimitivePayloadPolicy, ScopeError, ScopeOwner, ScopePolicy};

use crate::fleet::lease::v10::source_store::PgLeaseSourceStorage;
use crate::fleet::v10::source_state_keys::FleetSourceKey;
use crate::installation::contract::{BoundComponentContract, ComponentDeclaration};
use crate::installation::control_state::InstallationControlState;
use crate::installation::control_storage::ControlLockProof;
use crate::installation::creation::PgNativeQueueStorage;
use crate::installation::v10::PgV10SourceDescriptor;
use crate::kv::Key;
use crate::records::RecordNamespace;
use crate::records::postgres::v10::relocating_point::PgRelocatingPointStorage;
use crate::records::postgres::v10::relocating_point::RelocatingPointBinding;
use crate::runtime::admission::bindings::{
	InstallationStorageRoute, PgInstallationAdmissionBinding,
};
use crate::runtime::admission::queue::NativeQueueSourceBindingError;
use crate::runtime::admission::{NativeAdmissionError, bind_supported_component};
use crate::runtime::participation::state::ParticipantState;
use crate::upgrade::policy::InstallationUpgradeCandidate;
use crate::upgrade::work::{PgUpgradeStatus, SourceGoal, UpgradeSourceOrigin, UpgradeWork};

use super::super::WriteTx;
use super::{NativeUpgradeAdmissionError, NativeUpgradeInvocation};

/// The fixed procedure binds advertised code support without altering active uses.
/// Construction is restricted to the exact enrolled invocation's fixed factory.
pub(crate) struct NativeUpgradeContracts<'current> {
	control: &'current InstallationControlState,
	participant: Arc<ParticipantState>,
	installation: Arc<PgInstallationAdmissionBinding>,
	route: InstallationStorageRoute,
}

/// One native scope operation's authenticated advertised support under held current control.
/// This context activates no ordinary role and supplies no transaction or participant secret.
pub(crate) struct NativeScopeMaintenanceContracts<'current> {
	scope: &'current ScopePolicy,
	participant: Arc<ParticipantState>,
}

/// Document storage is independent of source imports; schema-owned Fleet payloads are not.
pub(crate) fn require_scope_maintenance_route(
	route: InstallationStorageRoute,
	scope: &ScopePolicy,
) -> Result<(), NativeUpgradeAdmissionError> {
	match scope.owner() {
		ScopeOwner::Document(_) => Ok(()),
		ScopeOwner::Primitive(primitive)
			if matches!(primitive.payload(), PrimitivePayloadPolicy::Schema(_)) =>
		{
			if matches!(
				route,
				InstallationStorageRoute::NativeFresh | InstallationStorageRoute::NativeImported
			) {
				Ok(())
			} else {
				Err(NativeAdmissionError::StorageRouteUnsupported { route }.into())
			}
		}
		_ => Err(NativeAdmissionError::from(ScopeError::WrongOwner).into()),
	}
}

impl NativeScopeMaintenanceContracts<'_> {
	pub(crate) fn require_payload_support(&self) -> Result<(), NativeUpgradeAdmissionError> {
		let current = self.scope;
		if !matches!(current.owner(), ScopeOwner::Primitive(primitive)
			if matches!(primitive.payload(), PrimitivePayloadPolicy::Schema(_)))
		{
			return Err(NativeAdmissionError::from(ScopeError::WrongOwner).into());
		}
		self.participant
			.capabilities()
			.advertised()
			.primitive_scopes()
			.lookup(current.identity(), crate::scope::PrimitiveRole::Writer)
			.map_err(NativeAdmissionError::from)?
			.check_policy(crate::scope::PrimitiveRole::Writer, current)
			.map_err(NativeAdmissionError::from)?;
		for role in [
			crate::scope::payload::PayloadRole::Reader,
			crate::scope::payload::PayloadRole::Producer,
		] {
			self.participant
				.capabilities()
				.advertised()
				.payload_roles()
				.lookup(current.identity(), role)
				.map_err(NativeAdmissionError::from)?
				.check_policy(current)
				.map_err(NativeAdmissionError::from)?;
		}
		Ok(())
	}
	pub(crate) fn require_document_support(&self) -> Result<(), NativeUpgradeAdmissionError> {
		let current = self.scope;
		if !matches!(current.owner(), ScopeOwner::Document(_)) {
			return Err(NativeAdmissionError::from(ScopeError::WrongOwner).into());
		}
		self.participant
			.capabilities()
			.advertised()
			.document_roles()
			.check_selected_writer_verification(current)
			.map_err(NativeAdmissionError::from)?;
		Ok(())
	}
}

impl NativeUpgradeContracts<'_> {
	pub(crate) fn current_control(&self) -> &InstallationControlState {
		self.control
	}

	pub(crate) fn installation_binding(&self) -> &Arc<PgInstallationAdmissionBinding> {
		&self.installation
	}

	pub(crate) fn storage_route(&self) -> InstallationStorageRoute {
		self.route
	}

	pub(crate) fn bind<'a, 'd, Transitions>(
		&'a self,
		declaration: &'a ComponentDeclaration<'d, Transitions>,
	) -> Result<BoundComponentContract<'a, 'd, Transitions>, NativeAdmissionError> {
		let policy = self
			.control
			.component(declaration.identifier())
			.ok_or(NativeAdmissionError::ComponentNotInstalled)?;
		let support = self
			.participant
			.capabilities()
			.advertised()
			.component(declaration.identifier())
			.ok_or(NativeAdmissionError::ComponentNotAdvertised)?;
		bind_supported_component(policy, support, declaration)
	}

	pub(crate) fn require_lease_source_storage(
		&self,
		source_storage: &PgLeaseSourceStorage,
		namespace: &RecordNamespace,
	) -> Result<(), NativeAdmissionError> {
		self.installation
			.verify_lease_source_storage(source_storage, namespace)
			.map_err(Into::into)
	}

	pub(crate) fn require_raw_source_storage(
		&self,
		source_storage: &PgRelocatingPointStorage,
		namespace: &RecordNamespace,
	) -> Result<(), NativeAdmissionError> {
		self.installation
			.verify_raw_source_storage(source_storage, namespace)
			.map_err(Into::into)
	}
	pub(crate) fn require_counter_source_storage(
		&self,
		source_storage: &PgRelocatingPointStorage,
		binding: &RelocatingPointBinding,
	) -> Result<(), NativeAdmissionError> {
		self.installation
			.verify_counter_source_storage(source_storage, binding)
			.map_err(Into::into)
	}
	pub(crate) fn scalar_source_binding(
		&self,
		key: &Key,
	) -> Result<(FleetSourceKey, RelocatingPointBinding), NativeAdmissionError> {
		self.installation
			.scalar_source_binding(key)
			.map_err(Into::into)
	}
	pub(crate) fn require_scalar_source_storage(
		&self,
		source_storage: &PgRelocatingPointStorage,
		binding: &RelocatingPointBinding,
	) -> Result<FleetSourceKey, NativeAdmissionError> {
		self.installation
			.verify_fleet_scalar_source_storage(source_storage, binding)
			.map_err(Into::into)
	}

	pub(crate) fn require_queue_source_storage(
		&self,
		descriptor: &PgV10SourceDescriptor,
		storage: &PgNativeQueueStorage,
	) -> Result<(), NativeQueueSourceBindingError> {
		self.installation
			.verify_queue_source_storage(descriptor, storage)?;
		Ok(())
	}
}

/// Proof-carrying control snapshot and storage route supplied by a live [`PgControlRecordLock`].
pub(crate) struct HeldUpgradeControl<'lock, 'current> {
	_proof: ControlLockProof<'lock>,
	current: &'current InstallationControlState,
	route: InstallationStorageRoute,
}

impl<'lock, 'current> HeldUpgradeControl<'lock, 'current> {
	pub(crate) fn new(
		proof: ControlLockProof<'lock>,
		current: &'current InstallationControlState,
		route: InstallationStorageRoute,
	) -> Self {
		Self {
			_proof: proof,
			current,
			route,
		}
	}
}

impl NativeUpgradeInvocation<'_> {
	/// Constructs fixed native scope maintenance after exact current context and one private
	/// authentication. The progress guard supplies its observed route; actual installation origin
	/// must agree.
	pub(crate) async fn native_scope_maintenance_contracts<'current>(
		&self,
		tx: &mut WriteTx<'_>,
		held_control: HeldUpgradeControl<'_, 'current>,
		final_candidate: &InstallationUpgradeCandidate,
		actual_stage: &ScopePolicy,
	) -> Result<NativeScopeMaintenanceContracts<'current>, NativeUpgradeAdmissionError> {
		let current = held_control.current;
		let route = held_control.route;
		self.enrollment.require_transaction(tx)?;
		let origin = match (
			self.enrollment
				.admission
				.config
				.installation_binding
				.as_ref(),
			route,
		) {
			(
				PgInstallationAdmissionBinding::Fresh { .. },
				InstallationStorageRoute::NativeFresh,
			) => UpgradeSourceOrigin::Fresh,
			(
				PgInstallationAdmissionBinding::Imported { .. },
				InstallationStorageRoute::SourceCoexistence
				| InstallationStorageRoute::SourceRelocation
				| InstallationStorageRoute::NativeImported,
			) => UpgradeSourceOrigin::Imported,
			_ => return Err(NativeAdmissionError::StorageRouteUnsupported { route }.into()),
		};
		let scope = current
			.scope_policies()
			.lookup(actual_stage.identity())
			.map_err(NativeAdmissionError::from)?;
		if !scope.matches_policy(actual_stage) {
			return Err(NativeAdmissionError::from(ScopeError::ScopeMismatch).into());
		}
		require_scope_maintenance_route(route, scope)?;
		let work = UpgradeWork::decode_json(
			current
				.pending_upgrade()
				.ok_or(NativeUpgradeAdmissionError::WorkAbsent)?,
		)?;
		work.require_checkpoint_context(
			current,
			final_candidate,
			origin,
			&work.checkpoint_context(),
		)?;
		if !work.target().matches_target(self.target()) {
			return Err(NativeUpgradeAdmissionError::WorkTargetMismatch);
		}
		if matches!(
			work.status(),
			PgUpgradeStatus::Completed | PgUpgradeStatus::Cancelled
		) {
			return Err(crate::upgrade::work::UpgradeWorkError::TerminalWork.into());
		}
		let participant = self
			.enrollment
			.authenticate_current_participant(tx, current)
			.await?;
		Ok(NativeScopeMaintenanceContracts { scope, participant })
	}
	/// The fixed progress child supplies its actual held current control, decoded route and Tx.
	/// Full Work context and private authentication use no additional source/control acquisition.
	pub(crate) async fn source_relocation_contracts<'current>(
		&self,
		tx: &mut WriteTx<'_>,
		held_control: HeldUpgradeControl<'_, 'current>,
		final_candidate: &InstallationUpgradeCandidate,
	) -> Result<NativeUpgradeContracts<'current>, NativeUpgradeAdmissionError> {
		let current = held_control.current;
		let route = held_control.route;
		self.enrollment.require_transaction(tx)?;
		let work = UpgradeWork::decode_json(
			current
				.pending_upgrade()
				.ok_or(NativeUpgradeAdmissionError::WorkAbsent)?,
		)?;
		work.require_checkpoint_context(
			current,
			final_candidate,
			UpgradeSourceOrigin::Imported,
			&work.checkpoint_context(),
		)?;
		if !work.target().matches_target(self.target()) {
			return Err(NativeUpgradeAdmissionError::WorkTargetMismatch);
		}
		if work.target().source_goal() != SourceGoal::DataRetired {
			return Err(NativeUpgradeAdmissionError::DataRetirementGoalRequired);
		}
		if matches!(
			work.status(),
			PgUpgradeStatus::Completed | PgUpgradeStatus::Cancelled
		) {
			return Err(crate::upgrade::work::UpgradeWorkError::TerminalWork.into());
		}
		if route != InstallationStorageRoute::SourceRelocation
			|| !matches!(
				self.enrollment
					.admission
					.config
					.installation_binding
					.as_ref(),
				PgInstallationAdmissionBinding::Imported { .. }
			) {
			return Err(NativeAdmissionError::StorageRouteUnsupported { route }.into());
		}
		let participant = self
			.enrollment
			.authenticate_current_participant(tx, current)
			.await?;
		Ok(NativeUpgradeContracts {
			control: current,
			participant,
			installation: Arc::clone(&self.enrollment.admission.config.installation_binding),
			route,
		})
	}
}
