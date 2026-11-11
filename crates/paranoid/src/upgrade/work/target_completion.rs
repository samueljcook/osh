//! Exact complete-target Work preparation, separate from the executor's phase and inspection proof.

use crate::installation::contract::ComponentPolicy;
use crate::installation::control_state::InstallationControlState;
use crate::scope::{ScopeError, ScopeIdentity, ScopePolicySet};
use crate::upgrade::policy::InstallationUpgradeCandidate;

use super::v10::source_checkpoint::PgV10ConversionProcedure;
use super::{
	PgUpgradeStatus, UpgradeSourceOrigin, UpgradeWork, UpgradeWorkCheckpointContext,
	UpgradeWorkError, UpgradeWorkGenerationChange,
};

pub(in crate::upgrade) struct TargetCompletionWorkFields<'a> {
	pub(crate) control: &'a InstallationControlState,
	pub(crate) candidate: &'a InstallationUpgradeCandidate,
	pub(crate) origin: UpgradeSourceOrigin,
	pub(crate) expected: &'a UpgradeWorkCheckpointContext,
	pub(crate) components: &'a [ComponentPolicy],
	pub(crate) scopes: &'a ScopePolicySet,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum TargetCompletionError {
	#[error("complete target requires Queue job conversion progress consumption")]
	QueueJobProgress,
	#[error("complete target prospective component is repeated")]
	DuplicateComponent { component_identifier: String },
	#[error("complete target prospective component is not requested")]
	UnexpectedComponent { component_identifier: String },
	#[error("complete target requested component is absent")]
	MissingComponent { component_identifier: String },
	#[error("complete target prospective component differs from its full requested policy")]
	ComponentMismatch { component_identifier: String },
	#[error("complete target explicit scope overlay is invalid")]
	ScopeOverlay(#[source] Box<ScopeError>),
	#[error("complete target prospective scope differs from its full requested policy")]
	ScopeMismatch { scope: ScopeIdentity },
	#[error("complete target prospective scope is not part of the requested overlay")]
	UnexpectedScope { scope: ScopeIdentity },
	#[error("complete target requires source progress consumption")]
	SourceProgress { procedure: PgV10ConversionProcedure },
	#[error("complete target requires document conversion progress consumption")]
	DocumentProgress { scope: ScopeIdentity },
	#[error("complete target requires document verification progress consumption")]
	VerificationProgress { scope: ScopeIdentity },
	#[error("complete target requires payload conversion progress consumption")]
	PayloadProgress { scope: ScopeIdentity },
}

impl From<TargetCompletionError> for UpgradeWorkError {
	fn from(source: TargetCompletionError) -> Self {
		Self::TargetCompletion(Box::new(source))
	}
}

impl UpgradeWork {
	/// The executor must prove supported transitions, actual goal/route and a complete inspection.
	/// It must persist these exact prospective policies and Work in the same effective write.
	/// Every rejection precedes mutation; progress is consumed only by its named completion.
	pub(in crate::upgrade) fn complete_target(
		&mut self,
		fields: TargetCompletionWorkFields<'_>,
	) -> Result<UpgradeWorkGenerationChange, UpgradeWorkError> {
		self.require_checkpoint_context(
			fields.control,
			fields.candidate,
			fields.origin,
			fields.expected,
		)?;
		if matches!(
			self.status(),
			PgUpgradeStatus::Completed | PgUpgradeStatus::Cancelled
		) {
			return Err(UpgradeWorkError::TerminalWork);
		}
		self.require_source_checkpoint_invariants(
			fields.control.component_policies(),
			fields.control.scope_policies(),
		)?;
		self.require_document_checkpoint_invariants(fields.control.scope_policies())?;
		self.require_document_verification_invariants(fields.control.scope_policies())?;
		self.require_payload_checkpoint_invariants(fields.control.scope_policies())?;
		self.require_consumed_target_progress()?;
		self.require_complete_target_policies(&fields)?;
		self.require_source_checkpoint_invariants(fields.components, fields.scopes)?;
		self.require_document_checkpoint_invariants(fields.scopes)?;
		self.require_document_verification_invariants(fields.scopes)?;
		self.require_payload_checkpoint_invariants(fields.scopes)?;
		let components_equal = fields.components.len() == fields.control.component_policies().len()
			&& fields.components.iter().all(|policy| {
				fields
					.control
					.component(policy.component_identifier())
					.is_some_and(|current| current.matches_policy(policy))
			});
		let next = if components_equal
			&& fields
				.scopes
				.matches_policies(fields.control.scope_policies())
		{
			self.owned_generation()
		} else {
			self.next_build_generation()?
		};
		let result = UpgradeWorkGenerationChange {
			expected: self.owned_generation(),
			next,
		};
		// `require_consumed_target_progress` checked that the progress is empty.
		self.take_unfinished_progress()?;
		self.state = super::UpgradeWorkState::Completed {
			owned_generation: next,
		};
		Ok(result)
	}

	/// Reports the exact remaining procedure/scope without changing any checkpoint.
	pub(in crate::upgrade) fn require_consumed_target_progress(
		&self,
	) -> Result<(), TargetCompletionError> {
		if self.queue_job_checkpoint().is_some() {
			return Err(TargetCompletionError::QueueJobProgress);
		}
		if let Some(checkpoint) = self.source_checkpoints().first() {
			return Err(TargetCompletionError::SourceProgress {
				procedure: checkpoint.procedure(),
			});
		}
		if let Some(checkpoint) = self.document_conversion_checkpoints().first() {
			return Err(TargetCompletionError::DocumentProgress {
				scope: checkpoint.invariant().identity().clone(),
			});
		}
		if let Some(checkpoint) = self.document_verification_checkpoints().first() {
			return Err(TargetCompletionError::VerificationProgress {
				scope: checkpoint.invariant().identity().clone(),
			});
		}
		if let Some(checkpoint) = self.payload_checkpoints().first() {
			return Err(TargetCompletionError::PayloadProgress {
				scope: checkpoint.invariant().identity().clone(),
			});
		}
		Ok(())
	}

	fn require_complete_target_policies(
		&self,
		fields: &TargetCompletionWorkFields<'_>,
	) -> Result<(), TargetCompletionError> {
		let target = self.target.request();
		for (index, policy) in fields.components.iter().enumerate() {
			if fields.components[..index]
				.iter()
				.any(|prior| prior.component_identifier() == policy.component_identifier())
			{
				return Err(TargetCompletionError::DuplicateComponent {
					component_identifier: policy.component_identifier().to_owned(),
				});
			}
			let expected = target
				.policies()
				.iter()
				.find(|expected| expected.component_identifier() == policy.component_identifier())
				.ok_or_else(|| TargetCompletionError::UnexpectedComponent {
					component_identifier: policy.component_identifier().to_owned(),
				})?;
			if !expected.matches_policy(policy) {
				return Err(TargetCompletionError::ComponentMismatch {
					component_identifier: policy.component_identifier().to_owned(),
				});
			}
		}
		for policy in target.policies() {
			if !fields.components.iter().any(|prospective| {
				prospective.component_identifier() == policy.component_identifier()
			}) {
				return Err(TargetCompletionError::MissingComponent {
					component_identifier: policy.component_identifier().to_owned(),
				});
			}
		}
		let overlaid = target
			.scope_replacements()
			.map(|replacements| {
				fields
					.control
					.scope_policies()
					.with_replacements(replacements)
			})
			.transpose()
			.map_err(|error| TargetCompletionError::ScopeOverlay(Box::new(error)))?;
		let expected = match overlaid.as_ref() {
			Some(scopes) => scopes,
			None => fields.control.scope_policies(),
		};
		for policy in expected.policies() {
			if !fields
				.scopes
				.lookup(policy.identity())
				.is_ok_and(|prospective| prospective.matches_policy(policy))
			{
				return Err(TargetCompletionError::ScopeMismatch {
					scope: policy.identity().clone(),
				});
			}
		}
		for policy in fields.scopes.policies() {
			if expected.lookup(policy.identity()).is_err() {
				return Err(TargetCompletionError::UnexpectedScope {
					scope: policy.identity().clone(),
				});
			}
		}
		Ok(())
	}
}
