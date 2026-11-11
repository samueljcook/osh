//! Participant pages and fresh canonical expiry fencing inside the owning exclusive interval.

use std::{num::NonZeroU32, sync::Arc};

use crate::db::WriteTx;
use crate::installation::contract::ComponentDeclaration;
use crate::installation::control_state::{InstallationControlComponent, InstallationControlState};
use crate::records::{NamespacePageRequest, PointRecordLock, StateTimestampMicros};
use crate::runtime::participation::state::{
	ParticipantComponent, ParticipantSchema, ParticipantState, ParticipationSemantics,
};
use crate::upgrade::participants::ParticipantInspectionFields;
use crate::upgrade::policy::assessment::ParticipantPolicyAssessment;
use crate::upgrade::policy::installed::{
	InstalledParticipantPolicyAssessment, InstalledParticipantPolicyAssessmentFields,
};
use crate::upgrade::policy::{
	InstallationUpgradeCandidate, InstallationUpgradeParticipantAssessmentFields,
	InstallationUpgradeParticipantDisposition, UpgradeCompatibilityError,
};

use super::super::{
	NativeUpgradeParticipantsError, ParticipantInspectionBudget, ParticipantInspectionCoverage,
	ParticipantInspectionLimit, ParticipantInspectionSummary, ParticipantUpgradeDeferral,
	PgNativeUpgradeParticipants,
};
use super::PgParticipantUpgradeInterval;

enum PolicyAssessment<'a, 'policy> {
	FinalTarget {
		current: InstalledParticipantPolicyAssessment<'a>,
		candidate: &'a InstallationUpgradeCandidate,
		control: &'a InstallationControlState,
		participant: &'a ParticipantSchema,
		control_component: &'a InstallationControlComponent,
		administration: &'a ComponentDeclaration<'policy, ParticipationSemantics>,
	},
	CurrentAndExplicit {
		current: InstalledParticipantPolicyAssessment<'a>,
		proposed: &'a ParticipantPolicyAssessment<'policy>,
	},
}
impl PolicyAssessment<'_, '_> {
	fn assess(
		&self,
		state: &ParticipantState,
		observed_at: StateTimestampMicros,
	) -> Result<InstallationUpgradeParticipantDisposition, UpgradeCompatibilityError> {
		match self {
			Self::FinalTarget {
				current,
				candidate,
				control,
				administration,
				participant,
				control_component,
			} => match current.assess(state, observed_at)? {
				InstallationUpgradeParticipantDisposition::Compatible => candidate
					.assess_participant(InstallationUpgradeParticipantAssessmentFields {
						participant: state,
						control,
						administration,
						participant_schema: participant,
						control_component,
						observed_at,
					}),
				disposition => Ok(disposition),
			},

			Self::CurrentAndExplicit { current, proposed } => {
				match current.assess(state, observed_at)? {
					InstallationUpgradeParticipantDisposition::Compatible => {
						proposed.assess(state, observed_at)
					}
					disposition => Ok(disposition),
				}
			}
		}
	}
}

pub(super) async fn inspect(
	interval: &mut PgParticipantUpgradeInterval<'_, '_>,
	budget: ParticipantInspectionBudget,
) -> Result<ParticipantInspectionSummary, NativeUpgradeParticipantsError> {
	let control = &interval.control;
	let declaration = interval.participants.config.administration.declaration();
	let current =
		InstalledParticipantPolicyAssessment::new(InstalledParticipantPolicyAssessmentFields {
			control,
			administration: &declaration,
			participant: interval
				.participants
				.config
				.administration
				.participant_schema(),
			control_component: interval
				.participants
				.config
				.administration
				.control_component(),
		})?;
	let assessment = PolicyAssessment::FinalTarget {
		current,
		candidate: interval.candidate,
		control,
		participant: interval
			.participants
			.config
			.administration
			.participant_schema(),
		administration: &declaration,
		control_component: interval
			.participants
			.config
			.administration
			.control_component(),
	};
	inspect_with_assessment(
		interval.participants,
		interval.tx,
		control,
		assessment,
		budget,
	)
	.await
}

/// One physical inspection checks current compatibility before considering proposed policy.
pub(in crate::upgrade) async fn inspect_current_and_proposed_policy(
	tx: &mut WriteTx<'_>,
	fields: ParticipantInspectionFields<'_, '_>,
) -> Result<ParticipantInspectionSummary, NativeUpgradeParticipantsError> {
	let declaration = fields.inspector.config.administration.declaration();
	let current =
		InstalledParticipantPolicyAssessment::new(InstalledParticipantPolicyAssessmentFields {
			control: fields.current,
			administration: &declaration,
			participant: fields.inspector.config.administration.participant_schema(),
			control_component: fields.inspector.config.administration.control_component(),
		})?;
	inspect_with_assessment(
		fields.inspector,
		tx,
		fields.current,
		PolicyAssessment::CurrentAndExplicit {
			current,
			proposed: fields.proposed,
		},
		fields.budget,
	)
	.await
}

async fn inspect_with_assessment(
	participants: &PgNativeUpgradeParticipants,
	tx: &mut WriteTx<'_>,
	control: &InstallationControlState,
	assessment: PolicyAssessment<'_, '_>,
	budget: ParticipantInspectionBudget,
) -> Result<ParticipantInspectionSummary, NativeUpgradeParticipantsError> {
	let declaration = participants.config.administration.declaration();
	let administrative_policy = control
		.component(declaration.identifier())
		.ok_or(NativeUpgradeParticipantsError::AdministrativePolicyAbsent)?;
	let administrative_binding = declaration.bind(administrative_policy)?;
	participants
		.config
		.administration
		.require_bound_policy(&administrative_binding)?;
	let bound_participant = administrative_binding.bind_schema(
		participants
			.config
			.administration
			.participant_schema()
			.schema(),
	)?;
	let mut result = ParticipantInspectionSummary {
		coverage: ParticipantInspectionCoverage::Incomplete {
			exhausted: ParticipantInspectionLimit::Rows,
		},
		inspected_row_count: 0,
		page_query_count: 0,
		expired_fenced_count: 0,
		deferred: Vec::new(),
	};
	let mut after_key = None;
	loop {
		let remaining = budget.max_rows.get() - result.inspected_row_count;
		if remaining == 0 {
			result.coverage = ParticipantInspectionCoverage::Incomplete {
				exhausted: ParticipantInspectionLimit::Rows,
			};
			return Ok(result);
		}
		if result.page_query_count == budget.max_page_queries.get() {
			result.coverage = ParticipantInspectionCoverage::Incomplete {
				exhausted: ParticipantInspectionLimit::PageQueries,
			};
			return Ok(result);
		}
		let page_size = NonZeroU32::new(remaining.min(budget.page_size.get()))
			.expect("positive remaining row budget and page size");
		let request = NamespacePageRequest::new(
			participants.config.participant_namespace.clone(),
			after_key,
			page_size,
		);
		let page = participants
			.config
			.participant_storage
			.read_page(tx, &request)
			.await?;
		result.page_query_count += 1;
		for entry in page.entries() {
			let component = ParticipantComponent::from_record_identity_with_schema(
				control.installation_id(),
				entry.identity().clone(),
				Arc::clone(participants.config.administration.participant_schema()),
			)?;
			let participant = component.decode_bound_stored_record(
				entry.identity(),
				entry.record(),
				&bound_participant,
			)?;
			let disposition = assessment.assess(&participant, page.observed_at())?;
			// Re-reading creates a fresh private working value, without copying the stored body.
			drop(participant);
			match disposition {
				InstallationUpgradeParticipantDisposition::Compatible => {}
				InstallationUpgradeParticipantDisposition::UnsupportedCore => {
					result
						.deferred
						.push(ParticipantUpgradeDeferral::LiveUnsupportedCore);
				}
				InstallationUpgradeParticipantDisposition::UnsupportedScope => {
					result
						.deferred
						.push(ParticipantUpgradeDeferral::LiveUnsupportedScope);
				}
				InstallationUpgradeParticipantDisposition::RequiresDurableFencing => {
					let point = participants
						.config
						.participant_storage
						.lock(
							tx,
							component.identity(),
							participants.config.conflict_attempts,
						)
						.await?;
					let Some(participant) = component.decode_bound_record(
						component.identity(),
						point.snapshot(),
						&bound_participant,
					)?
					else {
						point.keep().await?;
						return Err(NativeUpgradeParticipantsError::ParticipantDisappeared);
					};
					match assessment.assess(&participant, point.snapshot().observed_at())? {
						InstallationUpgradeParticipantDisposition::RequiresDurableFencing => {
							point.delete().await?;
							result.expired_fenced_count += 1;
						}
						InstallationUpgradeParticipantDisposition::Compatible => {
							point.keep().await?;
						}
						InstallationUpgradeParticipantDisposition::UnsupportedScope => {
							point.keep().await?;
							result
								.deferred
								.push(ParticipantUpgradeDeferral::LiveUnsupportedScope);
						}
						InstallationUpgradeParticipantDisposition::UnsupportedCore => {
							point.keep().await?;
							result
								.deferred
								.push(ParticipantUpgradeDeferral::LiveUnsupportedCore);
						}
					}
				}
			}
			result.inspected_row_count += 1;
		}
		if page.entries().len() < page_size.get() as usize {
			result.coverage = ParticipantInspectionCoverage::Complete;
			return Ok(result);
		}
		after_key = page
			.entries()
			.last()
			.map(|entry| entry.identity().key().clone());
	}
}
