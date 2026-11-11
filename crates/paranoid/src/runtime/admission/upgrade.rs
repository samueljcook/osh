//! Private fixed upgrade transaction enrollment in the actual participation runtime.

use crate::db::OwnedTransactionRegistration;
use crate::db::transaction_ending::{TransactionEndingReceipt, TransactionEndingSnapshot};
use crate::runtime::local_admission::upgrade_work::{
	RuntimeUpgradeWorkIdentity, RuntimeUpgradeWorkPermit,
};
use crate::upgrade::work::{CompleteUpgradeTarget, UpgradeWorkError};

use super::{
	Arc, DatabaseError, InstallationControlState, NativeAdmissionError, ParticipantState,
	PgControlStorage, PgInstallationAdmissionBinding, PgNativeAdministration, PgNativeAdmission,
	RuntimeAdmissionError, Tx, WriteTx,
};

pub(crate) mod contracts;
pub(crate) use contracts::HeldUpgradeControl;
pub(crate) mod ending;

pub(crate) struct NativeUpgradeEnrollmentFields<'a> {
	pub(crate) admission: &'a PgNativeAdmission,
	pub(crate) owner: &'a RuntimeUpgradeWorkPermit,
	pub(crate) identity: RuntimeUpgradeWorkIdentity,
	pub(crate) ending: &'a mut Option<TransactionEndingReceipt>,
}

/// One fixed step retains exact private local ownership and actual ending observations.
/// Its identities and receipts provide no durable control or participant authority.
pub(crate) struct NativeUpgradeEnrollment<'a> {
	admission: &'a PgNativeAdmission,
	identity: RuntimeUpgradeWorkIdentity,
	ending: &'a mut Option<TransactionEndingReceipt>,
	state: UpgradeEnrollmentState,
}

enum UpgradeEnrollmentState {
	AwaitingDispatch(OwnedTransactionRegistration),
	Enrolled(TransactionEndingReceipt),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeUpgradeAdmissionError {
	#[error("fixed upgrade control has no complete Work")]
	WorkAbsent,
	#[error("fixed upgrade Work differs from its invocation target")]
	WorkTargetMismatch,
	#[error("fixed source conversion requires the explicit DataRetired goal")]
	DataRetirementGoalRequired,
	#[error("fixed upgrade full current Work context is invalid")]
	Work(#[from] UpgradeWorkError),
	#[error("upgrade identity belongs to another registered installation")]
	InstallationMismatch,
	#[error("upgrade operation uses different runtime bindings")]
	BindingMismatch,
	#[error("upgrade previous transaction ending remains open or pending")]
	PreviousEndingPending,
	#[error("upgrade continuation has already enrolled its transaction")]
	AlreadyEnrolled,
	#[error("upgrade authentication requires its enrolled transaction")]
	TransactionMismatch,
	#[error("upgrade local continuation admission failed")]
	Local(#[from] RuntimeAdmissionError),
	#[error("upgrade private participant admission failed")]
	Admission(#[from] NativeAdmissionError),
}

impl NativeUpgradeAdmissionError {
	pub(crate) fn database_error(&self) -> Option<&DatabaseError> {
		match self {
			Self::Admission(error) => error.database_error(),
			_ => None,
		}
	}
	pub(crate) fn involves_database_failure(&self) -> bool {
		match self {
			Self::Admission(error) => error.involves_database_failure(),
			_ => false,
		}
	}
	pub(crate) fn commit_outcome_is_unknown(&self) -> bool {
		match self {
			Self::Admission(error) => error.commit_outcome_is_unknown(),
			_ => false,
		}
	}
}

impl PgNativeAdmission {
	/// Registers a known request identity without manufacturing a target base generation.
	pub(crate) fn register_upgrade_work(
		&self,
		identity: RuntimeUpgradeWorkIdentity,
	) -> Result<RuntimeUpgradeWorkPermit, NativeUpgradeAdmissionError> {
		if identity.installation_id != self.config.registration.authority().installation_id() {
			return Err(NativeUpgradeAdmissionError::InstallationMismatch);
		}
		Ok(self
			.config
			.local_admission
			.try_register_upgrade_work(identity)?)
	}
}

impl<'a> NativeUpgradeEnrollment<'a> {
	pub(crate) fn new(
		fields: NativeUpgradeEnrollmentFields<'a>,
	) -> Result<Self, NativeUpgradeAdmissionError> {
		if fields.identity.installation_id
			!= fields
				.admission
				.config
				.registration
				.authority()
				.installation_id()
		{
			return Err(NativeUpgradeAdmissionError::InstallationMismatch);
		}
		if let Some(origin) = fields.ending.as_ref()
			&& !matches!(origin.snapshot(), TransactionEndingSnapshot::Terminal(_))
		{
			return Err(NativeUpgradeAdmissionError::PreviousEndingPending);
		}
		let permit = fields
			.admission
			.config
			.local_admission
			.try_admit_upgrade_continuation(fields.owner, fields.identity)?;
		Ok(Self {
			admission: fields.admission,
			identity: fields.identity,
			ending: fields.ending,
			state: UpgradeEnrollmentState::AwaitingDispatch(OwnedTransactionRegistration::new(
				permit,
			)),
		})
	}

	pub(crate) fn identity(&self) -> RuntimeUpgradeWorkIdentity {
		self.identity
	}

	pub(crate) fn require_bindings(
		&self,
		installation: &Arc<PgInstallationAdmissionBinding>,
		storage: &PgControlStorage,
		administration: &Arc<PgNativeAdministration>,
	) -> Result<(), NativeUpgradeAdmissionError> {
		if !Arc::ptr_eq(installation, &self.admission.config.installation_binding)
			|| !Arc::ptr_eq(administration, &self.admission.config.administration)
			|| !storage.matches_storage(&self.admission.config.control_storage)
		{
			return Err(NativeUpgradeAdmissionError::BindingMismatch);
		}
		Ok(())
	}

	/// Publishes this ending before the owned runner awaits acquisition or BEGIN.
	pub(crate) fn prepare_transaction(
		&mut self,
	) -> Result<OwnedTransactionRegistration, NativeUpgradeAdmissionError> {
		let receipt = match &self.state {
			UpgradeEnrollmentState::AwaitingDispatch(registration) => registration.receipt(),
			UpgradeEnrollmentState::Enrolled(_) => {
				return Err(NativeUpgradeAdmissionError::AlreadyEnrolled);
			}
		};
		let UpgradeEnrollmentState::AwaitingDispatch(registration) = std::mem::replace(
			&mut self.state,
			UpgradeEnrollmentState::Enrolled(receipt.clone()),
		) else {
			unreachable!("only an awaiting enrollment can dispatch its registration")
		};
		*self.ending = Some(receipt.clone());
		Ok(registration)
	}

	pub(crate) fn require_transaction(
		&self,
		tx: &mut Tx<'_>,
	) -> Result<(), NativeUpgradeAdmissionError> {
		if !matches!(&self.state, UpgradeEnrollmentState::Enrolled(receipt)
			if receipt.shares_ending(&tx.ending_receipt()))
		{
			return Err(NativeUpgradeAdmissionError::TransactionMismatch);
		}
		Ok(())
	}

	/// Only fixed guard methods supply their verified current control after exact request checks.
	/// This authenticates under retained pins and performs no additional control acquisition.
	pub(crate) async fn authenticate_current(
		&self,
		tx: &mut Tx<'_>,
		current: &InstallationControlState,
	) -> Result<(), NativeUpgradeAdmissionError> {
		self.authenticate_current_participant(tx, current).await?;
		Ok(())
	}

	async fn authenticate_current_participant(
		&self,
		tx: &mut Tx<'_>,
		current: &InstallationControlState,
	) -> Result<Arc<ParticipantState>, NativeUpgradeAdmissionError> {
		self.require_transaction(tx)?;
		if current.installation_id() != self.identity.installation_id {
			return Err(NativeUpgradeAdmissionError::InstallationMismatch);
		}
		let declaration = self.admission.config.administration.declaration();
		let policy = current
			.component(declaration.identifier())
			.ok_or(NativeAdmissionError::ComponentNotInstalled)?;
		let administration = declaration
			.bind(policy)
			.map_err(NativeAdmissionError::Contract)?;
		self.admission
			.config
			.administration
			.require_bound_policy(&administration)
			.map_err(NativeAdmissionError::Administration)?;
		Ok(self
			.admission
			.authenticate_pinned_participant(tx, current, &administration)
			.await?)
	}
}

pub(crate) struct NativeUpgradeInvocationFields<'a> {
	pub(crate) admission: &'a PgNativeAdmission,
	pub(crate) owner: &'a RuntimeUpgradeWorkPermit,
	pub(crate) target: &'a CompleteUpgradeTarget,
	pub(crate) ending: &'a mut Option<TransactionEndingReceipt>,
}

/// A fixed effective step retains its complete target and the same transaction enrollment.
pub(crate) struct NativeUpgradeInvocation<'a> {
	enrollment: NativeUpgradeEnrollment<'a>,
	target: &'a CompleteUpgradeTarget,
}

impl<'a> NativeUpgradeInvocation<'a> {
	pub(crate) fn new(
		fields: NativeUpgradeInvocationFields<'a>,
	) -> Result<Self, NativeUpgradeAdmissionError> {
		let identity = fields.target.request().identity();
		let enrollment = NativeUpgradeEnrollment::new(NativeUpgradeEnrollmentFields {
			admission: fields.admission,
			owner: fields.owner,
			identity: RuntimeUpgradeWorkIdentity {
				installation_id: identity.installation_id,
				request_id: identity.request_id,
			},
			ending: fields.ending,
		})?;
		Ok(Self {
			enrollment,
			target: fields.target,
		})
	}
	pub(crate) fn target(&self) -> &CompleteUpgradeTarget {
		self.target
	}
	pub(crate) fn require_bindings(
		&self,
		installation: &Arc<PgInstallationAdmissionBinding>,
		storage: &PgControlStorage,
		administration: &Arc<PgNativeAdministration>,
	) -> Result<(), NativeUpgradeAdmissionError> {
		self.enrollment
			.require_bindings(installation, storage, administration)
	}
	pub(crate) fn prepare_transaction(
		&mut self,
	) -> Result<OwnedTransactionRegistration, NativeUpgradeAdmissionError> {
		self.enrollment.prepare_transaction()
	}
	pub(crate) fn require_transaction(
		&self,
		tx: &mut WriteTx<'_>,
	) -> Result<(), NativeUpgradeAdmissionError> {
		self.enrollment.require_transaction(tx)
	}
	pub(crate) async fn authenticate_current(
		&self,
		tx: &mut WriteTx<'_>,
		current: &InstallationControlState,
	) -> Result<(), NativeUpgradeAdmissionError> {
		self.enrollment.authenticate_current(tx, current).await
	}
}
