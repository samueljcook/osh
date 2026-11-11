//! One retained task continues one explicit request through actual registered operations.

use primitives_rs::time::Deadline;
use std::{num::NonZeroU32, sync::Arc};
use tokio::runtime::Handle;

use crate::runtime::local_admission::RuntimeAdmissionTracker;
use crate::runtime::local_admission::upgrade_work::{
	RuntimeUpgradeWorkIdentity, RuntimeUpgradeWorkPermit,
};
use crate::runtime::targets::NativeRuntimeTargetCode;
use crate::upgrade::requests::work::creation::UpgradeCreationIntent;
use crate::upgrade::requests::work::creation_execution::NativeUpgradeCreateState;
use crate::upgrade::work::UpgradeWork;

use super::NativeUpgradeExecutionServices;
use super::execution::limits::NativeUpgradeExecutionLimits;
use super::selection::PgUpgradeStep;

mod continuation;
pub(crate) mod error;
pub(crate) mod observer;
pub(crate) mod options;
mod state;

use error::NativeUpgradeDriverError;
use observer::{
	DriverShared, NativeUpgradeDriverObserver, NativeUpgradeDriverSnapshot, PgUpgradePhase,
};
use options::NativeUpgradeDriverOptions;
use state::{DriverCreation, DriverObservation, DriverRecovery, DriverTarget, DriverTransaction};

pub(crate) enum NativeUpgradeDriverRequest {
	Create(Arc<UpgradeCreationIntent>),
	Resume {
		identity: RuntimeUpgradeWorkIdentity,
		code: Arc<NativeRuntimeTargetCode>,
	},
	Inspect {
		identity: RuntimeUpgradeWorkIdentity,
		code: Arc<NativeRuntimeTargetCode>,
	},
	Cancel {
		identity: RuntimeUpgradeWorkIdentity,
		code: Arc<NativeRuntimeTargetCode>,
	},
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum DriverPurpose {
	Advance,
	Inspect,
	Cancel,
}

pub(crate) struct NativeUpgradeDriverStartFields {
	pub(crate) services: Arc<NativeUpgradeExecutionServices>,
	pub(crate) request: NativeUpgradeDriverRequest,
	pub(crate) runtime: Handle,
	pub(crate) options: NativeUpgradeDriverOptions,
	pub(crate) execution_limits: NativeUpgradeExecutionLimits,
}

/// Registers and spawns in the actual runtime registry; ordinary startup invokes no upgrade.
pub(crate) fn start_upgrade_driver(
	fields: NativeUpgradeDriverStartFields,
) -> Result<NativeUpgradeDriverObserver, NativeUpgradeDriverError> {
	let (identity, creation, code, purpose) = match fields.request {
		NativeUpgradeDriverRequest::Create(intent) => {
			let code = Arc::clone(intent.code());
			(
				intent.identity(),
				DriverCreation::Direct(NativeUpgradeCreateState::new(intent)),
				code,
				DriverPurpose::Advance,
			)
		}
		NativeUpgradeDriverRequest::Resume { identity, code } => (
			identity,
			DriverCreation::NotCreating,
			code,
			DriverPurpose::Advance,
		),
		NativeUpgradeDriverRequest::Inspect { identity, code } => (
			identity,
			DriverCreation::NotCreating,
			code,
			DriverPurpose::Inspect,
		),
		NativeUpgradeDriverRequest::Cancel { identity, code } => (
			identity,
			DriverCreation::NotCreating,
			code,
			DriverPurpose::Cancel,
		),
	};
	if !Arc::ptr_eq(
		code.installation(),
		fields.services.admission.installation_binding(),
	) {
		return Err(
			crate::runtime::admission::upgrade::NativeUpgradeAdmissionError::BindingMismatch.into(),
		);
	}
	let owner = fields.services.admission.register_upgrade_work(identity)?;
	let runtime = owner.admission_tracker();
	let shared = DriverShared::new(NativeUpgradeDriverSnapshot {
		phase: PgUpgradePhase::Starting,
		work: None,
		step: None,
		prerequisites: Arc::new(Vec::new()),
		ending: None,
		failure: None,
	});
	let observer = NativeUpgradeDriverObserver {
		shared: Arc::clone(&shared),
	};
	owner.spawn_owned_task(&fields.runtime, move |owner| async move {
		Driver {
			owner,
			runtime,
			shared,
			services: fields.services,
			code,
			identity,
			purpose,
			options: fields.options,
			limit: fields.execution_limits.limit(),
			execution_limits: fields.execution_limits,
			creation,
			observation: DriverObservation::new(),
			target: DriverTarget::Unbound,
			transaction: DriverTransaction::Idle,
			recovery: None,
			failure: None,
			transient_failures: 0,
			steps_in_pass: 0,
			last_step: None,
			deferred: Vec::new(),
			last_rejected: None,
		}
		.run()
		.await;
	})?;
	Ok(observer)
}

struct DeferredStep {
	step: PgUpgradeStep,
	until: Deadline,
}

struct RejectedContext {
	step: PgUpgradeStep,
	work: Arc<UpgradeWork>,
}

struct Driver {
	owner: RuntimeUpgradeWorkPermit,
	runtime: RuntimeAdmissionTracker,
	shared: Arc<DriverShared>,
	services: Arc<NativeUpgradeExecutionServices>,
	code: Arc<NativeRuntimeTargetCode>,
	identity: RuntimeUpgradeWorkIdentity,
	purpose: DriverPurpose,
	options: NativeUpgradeDriverOptions,
	execution_limits: NativeUpgradeExecutionLimits,
	/// The page size of the next document, payload, or v10 conversion step. After a step ends
	/// because one of its waits elapsed, the page size halves so that a slow database still
	/// converts smaller pages. After a step succeeds, the page size doubles, up to the
	/// configured budget.
	limit: NonZeroU32,
	creation: DriverCreation,
	observation: DriverObservation,
	target: DriverTarget,
	transaction: DriverTransaction,
	recovery: Option<DriverRecovery>,
	failure: Option<Arc<NativeUpgradeDriverError>>,
	transient_failures: u32,
	steps_in_pass: u32,
	last_step: Option<PgUpgradeStep>,
	deferred: Vec<DeferredStep>,
	last_rejected: Option<RejectedContext>,
}
