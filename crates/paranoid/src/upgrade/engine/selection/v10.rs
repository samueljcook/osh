//! Closed source procedures selected from the actual goal, route and durable progress.

use crate::runtime::admission::bindings::InstallationStorageRoute;
use crate::upgrade::work::v10::source_checkpoint::{PgV10ConversionProcedure, SourceScanProgress};
use crate::upgrade::work::{SourceGoal, UpgradeWork};

use super::{NativeUpgradePrerequisiteError, NativeUpgradeSelection, PgUpgradeStep};

pub(super) fn collect(
	selection: &mut NativeUpgradeSelection,
	work: &UpgradeWork,
	route: InstallationStorageRoute,
) {
	match (work.target().source_goal(), route) {
		(SourceGoal::Unchanged, _)
		| (SourceGoal::WritersRetired, InstallationStorageRoute::SourceRelocation) => {}
		(_, InstallationStorageRoute::NativeImported) => {
			for checkpoint in work.source_checkpoints() {
				selection.prerequisites.push(
					NativeUpgradePrerequisiteError::SourceProgressAfterRetirement {
						procedure: checkpoint.procedure(),
					},
				);
			}
		}
		(_, InstallationStorageRoute::SourceCoexistence) => {
			selection.steps.push(PgUpgradeStep::RetireV10Writers)
		}
		(SourceGoal::DataRetired, InstallationStorageRoute::SourceRelocation) => {
			let mut exhausted = true;
			for procedure in [
				PgV10ConversionProcedure::RawBytesRelocationV1,
				PgV10ConversionProcedure::FleetStateRelocationV1,
				PgV10ConversionProcedure::LeaseAggregateRelocationV1,
				PgV10ConversionProcedure::QueueJobRelocationV1,
				PgV10ConversionProcedure::QueueDeadLetterRelocationV1,
				PgV10ConversionProcedure::QueuePauseRelocationV1,
			] {
				if !work.source_checkpoint(procedure).is_some_and(|checkpoint| {
					matches!(checkpoint.progress(), SourceScanProgress::Scanned { .. })
				}) {
					exhausted = false;
					selection
						.steps
						.push(PgUpgradeStep::MoveV10Records(procedure));
				}
			}
			if exhausted {
				selection.steps.push(PgUpgradeStep::RetireV10Data);
			}
		}
		(_, InstallationStorageRoute::NativeFresh) => {
			unreachable!("complete Work rejects source retirement goals for fresh origin")
		}
	}
}

pub(super) fn goal_satisfied(goal: SourceGoal, route: InstallationStorageRoute) -> bool {
	match goal {
		SourceGoal::Unchanged => true,
		SourceGoal::WritersRetired => matches!(
			route,
			InstallationStorageRoute::SourceRelocation | InstallationStorageRoute::NativeImported
		),
		SourceGoal::DataRetired => route == InstallationStorageRoute::NativeImported,
	}
}
