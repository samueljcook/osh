//! An upgrade moves an installation to the component and scope policies that a runtime's
//! declarations require. The retirement of v10 moves the data of an imported v10 installation
//! into Paranoid's storage and ends v10's use of the installation.
//!
//! [`PgRuntime::start_upgrade`](crate::runtime::PgRuntime::start_upgrade) starts an upgrade, and
//! [`PgRuntime::retire_v10`](crate::runtime::PgRuntime::retire_v10) starts the retirement of v10.
//! [`PgRuntime::resume_upgrade`](crate::runtime::PgRuntime::resume_upgrade),
//! [`PgRuntime::inspect_upgrade`](crate::runtime::PgRuntime::inspect_upgrade), and
//! [`PgRuntime::cancel_upgrade`](crate::runtime::PgRuntime::cancel_upgrade) continue, read, or
//! cancel the recorded upgrade. Each method returns a [`PgUpgradeController`] of a task that the
//! runtime runs. `resume_upgrade` and `cancel_upgrade` return `None` when the installation has
//! no unfinished upgrade, and `inspect_upgrade` returns `None` when the installation records no
//! upgrade. The installation records the upgrade, so a runtime in any process can continue it
//! after a restart. A step that installs new versions waits until every running deployment can
//! use them.
//!
//! [`PgUpgradeSnapshot`] reports the task's phase, the upgrade's recorded status, the task's
//! last step, the conditions that block the next step, the end of its last transaction, and its
//! last failure. [`PgUpgradeStartError`] reports a failure to start a task, and
//! [`PgUpgradeWaitError`] reports a wait that ended without the requested result.

pub(crate) mod engine;
pub(crate) mod handle;
pub(crate) mod observation;
pub(crate) mod participants;
pub(crate) mod policy;
pub(crate) mod progress;
pub(crate) mod requests;
mod wait;
pub(crate) mod work;

pub use engine::driver::observer::PgUpgradePhase;
pub use engine::selection::PgUpgradeStep;
pub use handle::error::{
	PgUpgradeFailure, PgUpgradeFailureKind, PgUpgradeStartError, PgUpgradeStartErrorKind,
	PgUpgradeTargetSelectionErrorKind,
};
pub use handle::{
	PgUpgradeController, PgUpgradeEndingKind, PgUpgradeEndingSnapshot, PgUpgradePrerequisite,
	PgUpgradePrerequisiteKind, PgUpgradeSnapshot, PgUpgradeWaitError, PgUpgradeWaitErrorKind,
};
pub use work::{PgUpgradeStatus, v10::source_checkpoint::PgV10ConversionProcedure};
