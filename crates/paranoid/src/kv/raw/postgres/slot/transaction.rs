//! The actual mutation token remains owned until known completion or unresolved failure.

use super::super::StateMutationCompletion;
use super::super::attempt_boundary::{
	GroupAttemptBoundaryPhase, GroupAttemptError, execute_boundary,
};
use super::SlotOwner;

pub(super) enum SlotMutation {
	Direct(StateMutationCompletion),
	Attempt(StateMutationCompletion),
	Unresolved,
	Finished,
}
impl SlotOwner<'_, '_> {
	pub(super) async fn begin_attempt(&mut self) -> Result<(), GroupAttemptError> {
		let SlotMutation::Direct(completion) =
			std::mem::replace(&mut self.mutation, SlotMutation::Unresolved)
		else {
			unreachable!("attempt creation requires a direct mutation owner")
		};
		execute_boundary(self.tx, GroupAttemptBoundaryPhase::Create).await?;
		self.mutation = SlotMutation::Attempt(completion);
		Ok(())
	}
	pub(super) async fn restore_attempt(&mut self) -> Result<(), GroupAttemptError> {
		let SlotMutation::Attempt(completion) =
			std::mem::replace(&mut self.mutation, SlotMutation::Unresolved)
		else {
			unreachable!("restoration requires an actual attempt owner")
		};
		execute_boundary(self.tx, GroupAttemptBoundaryPhase::RestoreAndRelease).await?;
		self.mutation = SlotMutation::Direct(completion);
		Ok(())
	}
	pub(super) async fn release(&mut self) -> Result<(), GroupAttemptError> {
		match std::mem::replace(&mut self.mutation, SlotMutation::Unresolved) {
			SlotMutation::Direct(completion) => self.mutation = SlotMutation::Direct(completion),
			SlotMutation::Attempt(completion) => {
				execute_boundary(self.tx, GroupAttemptBoundaryPhase::Release).await?;
				self.mutation = SlotMutation::Direct(completion);
			}
			SlotMutation::Unresolved | SlotMutation::Finished => {
				unreachable!("release requires a retained mutation owner")
			}
		}
		Ok(())
	}
	pub(super) fn complete_mutation(&mut self) {
		let SlotMutation::Direct(mut completion) =
			std::mem::replace(&mut self.mutation, SlotMutation::Unresolved)
		else {
			unreachable!("completion requires a direct or known-ended mutation owner")
		};
		completion.complete();
		self.mutation = SlotMutation::Finished;
	}
}
