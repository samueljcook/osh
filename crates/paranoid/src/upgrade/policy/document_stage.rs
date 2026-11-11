//! Actual-code document target resolution and pure intermediate writer-policy construction.
//!
//! A stage selects no effective policy and proves no participant, row, or query readiness.
//! An executor must derive it again from its pinned current policy and exact recorded target.

use std::fmt;

use crate::installation::contract::{RepresentationPolicy, SchemaPolicy, StateContractError};
use crate::kv::document::schema::IndexContractIdentity;
use crate::kv::document::schema::managed::{
	ManagedDocumentError, ManagedDocumentRecord, ManagedDocumentRegistration,
	ManagedDocumentRegistry, ManagedDocumentRegistryError,
};
use crate::records::EncodedRecord;
use crate::scope::{DocumentScopePolicy, ScopeError, ScopeIdentity, ScopeOwner, ScopePolicy};

use super::super::work::CompleteUpgradeTarget;
use super::request::matches_document_scope;
use super::scope::CandidateDocumentScopePolicy;

/// A final candidate retains the exact code instance that established its guarantees.
pub(in crate::upgrade) struct RetainedDocumentScopeCandidate {
	registration: ManagedDocumentRegistration,
	candidate: CandidateDocumentScopePolicy,
}

impl RetainedDocumentScopeCandidate {
	pub(crate) fn bind(
		registry: &ManagedDocumentRegistry,
		policy: ScopePolicy,
	) -> Result<Self, BoxedDocumentStageError> {
		let scope = policy.identity().clone();
		let registration = registry
			.lookup(&scope)
			.map_err(|error| BoxedDocumentStageError::new(DocumentStageError::Registry(error)))?;
		let candidate = registration
			.bind_final_candidate(policy)
			.map_err(|error| BoxedDocumentStageError::new(DocumentStageError::Code(error)))?;
		Ok(Self {
			registration: registration.clone(),
			candidate,
		})
	}
	pub(crate) fn policy(&self) -> &ScopePolicy {
		self.candidate.policy()
	}
	pub(crate) fn registration(&self) -> &ManagedDocumentRegistration {
		&self.registration
	}
}

impl fmt::Debug for RetainedDocumentScopeCandidate {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("RetainedDocumentScopeCandidate")
			.finish_non_exhaustive()
	}
}

/// The selected writer maintains the full union; old bodies use retained readers for conversion.
/// This binding does not promise projection-only preparation for every old representation.
pub(in crate::upgrade) struct DocumentWriterStage {
	registration: ManagedDocumentRegistration,
	final_policy: ScopePolicy,
	policy: ScopePolicy,
}

impl DocumentWriterStage {
	pub(crate) fn new(
		target: &RetainedDocumentScopeCandidate,
		current: &ScopePolicy,
	) -> Result<Self, BoxedDocumentStageError> {
		let target_policy = target.policy();
		let scope = target_policy.identity();
		let fail = |error| BoxedDocumentStageError::new(error);
		if current.identity() != scope {
			return Err(fail(DocumentStageError::ScopeMismatch {
				current: current.identity().clone(),
			}));
		}
		let ScopeOwner::Document(current_document) = current.owner() else {
			return Err(fail(DocumentStageError::NonDocumentCurrent));
		};
		// The actual final candidate constructor already rejects every other owner.
		let ScopeOwner::Document(target_document) = target_policy.owner() else {
			unreachable!("actual document final candidate contains a document policy")
		};
		if current_document.schema().identifier() != target_document.schema().identifier() {
			return Err(fail(DocumentStageError::SchemaFamilyMismatch));
		}

		let mut readers = current_document.schema().required_readers().to_vec();
		for target_reader in target_document.schema().required_readers() {
			if let Some(current_reader) = readers
				.iter()
				.find(|reader| reader.version == target_reader.version)
			{
				if current_reader != target_reader {
					return Err(fail(DocumentStageError::ReaderEnvelopeConflict {
						current: *current_reader,
						target: *target_reader,
					}));
				}
			} else {
				readers.push(*target_reader);
			}
		}
		let mut indexes: Vec<_> = current_document.maintenance_indexes().cloned().collect();
		for target_index in target_document.maintenance_indexes() {
			if let Some(current_index) = indexes
				.iter()
				.find(|index| index.identity() == target_index.identity())
			{
				if !current_index.matches_descriptor(target_index) {
					return Err(fail(DocumentStageError::IndexShapeConflict {
						index: target_index.identity().clone(),
					}));
				}
			} else {
				indexes.push(target_index.clone());
			}
		}
		let schema = SchemaPolicy::new(
			target_document.schema().identifier().to_owned(),
			target_document.schema().writer(),
			readers,
		)
		.map_err(|error| fail(DocumentStageError::Contract(error)))?;
		let document = DocumentScopePolicy::new_with_ready_indexes(
			schema,
			indexes,
			current_document.ready_indexes().cloned().collect(),
		)
		.map_err(|error| fail(DocumentStageError::Scope(error)))?;
		let policy = ScopePolicy::new(scope.clone(), ScopeOwner::Document(document));
		target
			.registration
			.check_writer_stage(&policy)
			.map_err(|error| fail(DocumentStageError::Code(error)))?;
		Ok(Self {
			registration: target.registration.clone(),
			final_policy: target_policy.clone(),
			policy,
		})
	}
	pub(crate) fn policy(&self) -> &ScopePolicy {
		&self.policy
	}
	pub(in crate::upgrade) fn final_policy(&self) -> &ScopePolicy {
		&self.final_policy
	}
	/// Compares complete parsed policy meaning without implying installation or readiness.
	pub(crate) fn matches_policy(&self, actual: &ScopePolicy) -> bool {
		matches_document_scope(&self.policy, actual)
	}
	pub(crate) fn registration(&self) -> &ManagedDocumentRegistration {
		&self.registration
	}
	/// Converts through this stage's retained reader and explicit selected writer, preserving
	/// metadata.
	pub(in crate::upgrade) fn prepare_conversion(
		&self,
		record: ManagedDocumentRecord<'_>,
	) -> Result<EncodedRecord, ManagedDocumentError> {
		self.registration
			.prepare_selected_writer_conversion(&self.policy, record)
	}
}

impl fmt::Debug for DocumentWriterStage {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("DocumentWriterStage")
			.finish_non_exhaustive()
	}
}

/// Only document replacements are resolved here. Core/source/slot identity needs separate checks.
/// `None` means that the single scope choice requests no document replacement.
pub(in crate::upgrade) struct ResolvedDocumentTargets {
	targets: Option<Vec<RetainedDocumentScopeCandidate>>,
}

impl ResolvedDocumentTargets {
	pub(crate) fn resolve(
		registry: &ManagedDocumentRegistry,
		target: &CompleteUpgradeTarget,
	) -> Result<Self, BoxedDocumentStageError> {
		let targets = target
			.request()
			.document_scopes()
			.map(|scopes| {
				scopes
					.policies()
					.map(|policy| {
						RetainedDocumentScopeCandidate::bind(
							registry,
							clone_document_policy(policy)?,
						)
					})
					.collect::<Result<Vec<_>, _>>()
			})
			.transpose()?;
		Ok(Self { targets })
	}

	pub(crate) fn targets(&self) -> Option<&[RetainedDocumentScopeCandidate]> {
		self.targets.as_deref()
	}
}

impl fmt::Debug for ResolvedDocumentTargets {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("ResolvedDocumentTargets")
			.field("replacement_count", &self.targets.as_ref().map(Vec::len))
			.finish()
	}
}

fn clone_document_policy(policy: &ScopePolicy) -> Result<ScopePolicy, BoxedDocumentStageError> {
	let ScopeOwner::Document(_) = policy.owner() else {
		return Err(BoxedDocumentStageError::new(
			DocumentStageError::NonDocumentTarget,
		));
	};
	Ok(policy.clone())
}

/// Causes retain typed identifiers; default diagnostics omit record and schema contents.
#[derive(Debug, thiserror::Error)]
pub(in crate::upgrade) enum DocumentStageError {
	#[error("document stage scope differs from current policy")]
	ScopeMismatch { current: ScopeIdentity },
	#[error("document stage current owner is not a document")]
	NonDocumentCurrent,
	#[error("document target owner is not a document")]
	NonDocumentTarget,
	#[error("document stage requires a schema reassignment procedure")]
	SchemaFamilyMismatch,
	#[error("document stage version has conflicting envelopes")]
	ReaderEnvelopeConflict {
		current: RepresentationPolicy,
		target: RepresentationPolicy,
	},
	#[error("document stage index identity has conflicting field definitions")]
	IndexShapeConflict { index: IndexContractIdentity },
	#[error("document target has no code")]
	Registry(#[source] ManagedDocumentRegistryError),
	#[error("document code does not implement the required policy")]
	Code(#[source] ManagedDocumentError),
	#[error("document stage violates a parsed schema contract")]
	Contract(#[source] StateContractError),
	#[error("document stage violates a parsed scope policy")]
	Scope(#[source] ScopeError),
}

#[derive(Debug)]
pub(crate) struct BoxedDocumentStageError {
	source: Box<DocumentStageError>,
}
impl BoxedDocumentStageError {
	fn new(source: DocumentStageError) -> Self {
		Self {
			source: Box::new(source),
		}
	}
}
impl fmt::Display for BoxedDocumentStageError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("actual-code document target or writer stage is unsupported")
	}
}
impl std::error::Error for BoxedDocumentStageError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(self.source.as_ref())
	}
}
