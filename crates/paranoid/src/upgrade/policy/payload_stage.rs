//! Actual-code Primitive payload writer stages without effective policy or coverage authority.

use crate::fleet::payload::scope::retained::RetainedPayloadError;
use crate::fleet::primitive_scope::ManagedPrimitiveRegistration;
use crate::installation::contract::{RepresentationPolicy, SchemaPolicy, StateContractError};
use crate::scope::managed::{
	ManagedScopeRegistration, ManagedScopeRegistry, ManagedScopeRegistryError,
};
use crate::scope::{
	PrimitivePayloadPolicy, PrimitiveScopePolicy, PrimitiveScopePolicyFields, ScopeError,
	ScopeOwner, ScopePolicy,
};

pub(in crate::upgrade) struct PrimitivePayloadWriterStageFields<'a> {
	pub(crate) registry: &'a ManagedScopeRegistry,
	pub(crate) current: &'a ScopePolicy,
	pub(crate) target: &'a ScopePolicy,
}
/// The actual owner and intermediate policy must be rebound under exact pinned Work by an executor.
pub(in crate::upgrade) struct PrimitivePayloadWriterStage<'a> {
	registration: &'a ManagedPrimitiveRegistration,
	final_policy: &'a ScopePolicy,
	policy: ScopePolicy,
}
impl<'a> PrimitivePayloadWriterStage<'a> {
	pub(crate) fn new(
		fields: PrimitivePayloadWriterStageFields<'a>,
	) -> Result<Self, BoxedPrimitivePayloadWriterStageError> {
		Self::build(fields).map_err(|error| BoxedPrimitivePayloadWriterStageError {
			source: Box::new(error),
		})
	}
	fn build(
		fields: PrimitivePayloadWriterStageFields<'a>,
	) -> Result<Self, PrimitivePayloadWriterStageError> {
		let PrimitivePayloadWriterStageFields {
			registry,
			current,
			target,
		} = fields;
		if current.identity() != target.identity() {
			return Err(PrimitivePayloadWriterStageError::ScopeMismatch {
				current: current.identity().clone(),
			});
		}
		let ManagedScopeRegistration::Primitive(registration) =
			registry.lookup(target.identity())?
		else {
			return Err(PrimitivePayloadWriterStageError::PayloadFamilyRequired);
		};
		if !matches!(
			registration,
			ManagedPrimitiveRegistration::CacheEntry(_)
				| ManagedPrimitiveRegistration::TopicEvent(_)
		) {
			return Err(PrimitivePayloadWriterStageError::PayloadFamilyRequired);
		}
		let (ScopeOwner::Primitive(installed), ScopeOwner::Primitive(proposed)) =
			(current.owner(), target.owner())
		else {
			return Err(PrimitivePayloadWriterStageError::PayloadFamilyRequired);
		};
		if !installed.intrinsic().matches_policy(proposed.intrinsic()) {
			return Err(PrimitivePayloadWriterStageError::IntrinsicTransitionRequired);
		}
		let (
			PrimitivePayloadPolicy::Schema(installed_schema),
			PrimitivePayloadPolicy::Schema(proposed_schema),
		) = (installed.payload(), proposed.payload())
		else {
			return Err(PrimitivePayloadWriterStageError::SchemaAdoptionRequired);
		};
		if installed_schema.identifier() != proposed_schema.identifier() {
			return Err(PrimitivePayloadWriterStageError::SchemaAdoptionRequired);
		}
		// The actual owner binds the complete final policy independently from the reader union.
		registration.bind_final_candidate(target.clone())?;
		let mut readers = installed_schema.required_readers().to_vec();
		for required in proposed_schema.required_readers() {
			if let Some(retained) = readers
				.iter()
				.find(|retained| retained.version == required.version)
			{
				if retained != required {
					return Err(PrimitivePayloadWriterStageError::ReaderEnvelopeConflict {
						retained: *retained,
						target: *required,
					});
				}
			} else {
				readers.push(*required);
			}
		}
		let schema = SchemaPolicy::new(
			proposed_schema.identifier().to_owned(),
			proposed_schema.writer(),
			readers,
		)?;
		let primitive = PrimitiveScopePolicy::new(PrimitiveScopePolicyFields {
			intrinsic: installed.intrinsic().clone(),
			payload: PrimitivePayloadPolicy::Schema(schema),
		})?;
		let policy = ScopePolicy::new(current.identity().clone(), ScopeOwner::Primitive(primitive));
		registration.bind_final_candidate(policy.clone())?;
		Ok(Self {
			registration,
			final_policy: target,
			policy,
		})
	}
	pub(crate) fn policy(&self) -> &ScopePolicy {
		&self.policy
	}
	pub(in crate::upgrade) fn final_policy(&self) -> &ScopePolicy {
		self.final_policy
	}
	pub(in crate::upgrade) fn prepare_conversion(
		&self,
		fields: crate::fleet::payload::scope::PayloadScopeRecordFields<'_>,
	) -> Result<
		crate::records::EncodedRecord,
		crate::fleet::payload::scope::conversion::NativePayloadConversionError,
	> {
		self.registration.prepare_payload_conversion(
			crate::fleet::payload::scope::conversion::NativePayloadConversionFields {
				policy: &self.policy,
				identity: fields.identity,
				record: fields.record,
			},
		)
	}
	pub(crate) fn matches_policy(&self, actual: &ScopePolicy) -> bool {
		self.policy.matches_policy(actual)
	}
}
impl std::fmt::Debug for PrimitivePayloadWriterStage<'_> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("PrimitivePayloadWriterStage { actual_code_retained: true }")
	}
}
#[derive(Debug, thiserror::Error)]
pub(in crate::upgrade) enum PrimitivePayloadWriterStageError {
	#[error("Primitive payload stage scope differs from the current scope")]
	ScopeMismatch {
		current: crate::scope::ScopeIdentity,
	},
	#[error("Primitive payload stage requires an CacheValue or TopicEvent owner")]
	PayloadFamilyRequired,
	#[error("Primitive payload stage requires a separate intrinsic transition procedure")]
	IntrinsicTransitionRequired,
	#[error("Primitive payload mode or schema family requires a separate adoption procedure")]
	SchemaAdoptionRequired,
	#[error("Primitive payload version has conflicting representation envelopes")]
	ReaderEnvelopeConflict {
		retained: RepresentationPolicy,
		target: RepresentationPolicy,
	},
	#[error("Primitive payload stage cannot resolve scope code")]
	Registry(#[from] ManagedScopeRegistryError),
	#[error("Primitive payload stage cannot bind its family and payload code")]
	Code(#[from] RetainedPayloadError),
	#[error("Primitive payload stage schema policy is invalid")]
	Contract(#[from] StateContractError),
	#[error("Primitive payload stage scope policy is invalid")]
	Scope(#[from] ScopeError),
}

pub(crate) struct BoxedPrimitivePayloadWriterStageError {
	source: Box<PrimitivePayloadWriterStageError>,
}
impl std::fmt::Debug for BoxedPrimitivePayloadWriterStageError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("BoxedPrimitivePayloadWriterStageError")
			.field("source", &self.source)
			.finish_non_exhaustive()
	}
}
impl std::fmt::Display for BoxedPrimitivePayloadWriterStageError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("actual retained code rejected the Primitive payload writer stage")
	}
}
impl std::error::Error for BoxedPrimitivePayloadWriterStageError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(self.source.as_ref())
	}
}
