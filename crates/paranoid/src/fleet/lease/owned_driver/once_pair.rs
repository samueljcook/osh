//! Actual pure Once pair preparation, with interpretation fixed by each sealed consumer.

use super::Claim;
use crate::fleet::lease::state::{
	LeaseRepresentationError, LeaseSchemaBinding, NativeLeaseClaimReleaseFields,
	PreparedNativeLeaseRelease,
};
use crate::fleet::once::engine::PgNativeOnce;
use crate::fleet::once::state::{
	NativeOnceCompletion, NativeOnceCompletionFields, OnceCompletionMutation,
};
use crate::fleet::scalar_codec::{ScalarSchemaBinding, ScalarStateError};
use crate::records::postgres::record_set::{
	PgPlacedRecordIdentity, PgRecordSetLock, RecordSetRejectionError,
};
use crate::records::{RecordMetadata, RecordMutation, StateTimestampMicros, StoredRecord};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NativeOnceCompletionSnapshot {
	pub(crate) finished_at: StateTimestampMicros,
	pub(crate) holder: super::super::HolderId,
	pub(crate) fencing_token: i64,
}
impl NativeOnceCompletionSnapshot {
	pub(super) fn from_completion(completion: &NativeOnceCompletion) -> Self {
		Self {
			finished_at: completion.finished_at(),
			holder: completion.holder().clone(),
			fencing_token: completion.fencing_token(),
		}
	}
}
#[derive(Debug, thiserror::Error)]
pub(crate) enum OncePairPreparationError {
	#[error("Once pair Lease history or private release preparation failed")]
	Lease(#[source] LeaseRepresentationError),
	#[error("Once pair marker is unreadable or cannot be encoded")]
	Completion(#[source] ScalarStateError),
	#[error("Once pair marker metadata is outside its domain")]
	Construction(#[source] crate::fleet::scalar_codec::ValueError),
	#[error("Once completion has unexpected physical index fields")]
	UnexpectedIndexes,
	#[error("Once pair addresses an unheld physical identity")]
	Identity(#[source] RecordSetRejectionError),
}
pub(super) struct OnceCompletionReadFields<'a, 'schema> {
	pub(super) schema: &'a ScalarSchemaBinding<'schema, NativeOnceCompletion>,
	pub(super) record: Option<&'a StoredRecord>,
}
pub(super) fn read_once_completion(
	fields: OnceCompletionReadFields<'_, '_>,
) -> Result<Option<NativeOnceCompletion>, OncePairPreparationError> {
	fields
		.record
		.map(|record| {
			if !record.index_fields().is_empty() {
				return Err(OncePairPreparationError::UnexpectedIndexes);
			}
			fields
				.schema
				.decode_record(record)
				.map_err(OncePairPreparationError::Completion)
		})
		.transpose()
}
pub(super) struct OncePairPrepareFields<'a, 'once, 'lease, 'group, 'tx> {
	pub(super) once: &'a PgNativeOnce,
	pub(super) once_schema: &'a ScalarSchemaBinding<'once, NativeOnceCompletion>,
	pub(super) lease_schema: &'a LeaseSchemaBinding<'lease>,
	pub(super) authority: &'a Claim,
	pub(super) group: &'a PgRecordSetLock<'group, 'tx>,
	pub(super) lease_identity: &'a PgPlacedRecordIdentity,
	pub(super) completion_identity: &'a PgPlacedRecordIdentity,
}
pub(super) enum PreparedOncePair {
	NoMatch,
	ExistingCompletion {
		completion: NativeOnceCompletionSnapshot,
		lease: RecordMutation,
	},
	NewCompletion {
		completion: NativeOnceCompletionSnapshot,
		lease: RecordMutation,
		completion_mutation: RecordMutation,
	},
}
pub(super) fn prepare_once_pair(
	fields: OncePairPrepareFields<'_, '_, '_, '_, '_>,
) -> Result<PreparedOncePair, OncePairPreparationError> {
	let snapshot = fields
		.group
		.snapshot(fields.lease_identity)
		.map_err(OncePairPreparationError::Identity)?;
	let lease = match fields
		.lease_schema
		.prepare_native_claim_release(NativeLeaseClaimReleaseFields {
			snapshot,
			authority: fields.authority,
		})
		.map_err(OncePairPreparationError::Lease)?
	{
		PreparedNativeLeaseRelease::NoMatch => return Ok(PreparedOncePair::NoMatch),
		PreparedNativeLeaseRelease::Release(record) => RecordMutation::Write(record),
	};
	let completion = fields
		.group
		.snapshot(fields.completion_identity)
		.map_err(OncePairPreparationError::Identity)?;
	let current = read_once_completion(OnceCompletionReadFields {
		schema: fields.once_schema,
		record: completion.record(),
	})?;
	let next = NativeOnceCompletion::new(NativeOnceCompletionFields {
		finished_at: fields.group.observed_at(),
		holder: fields.authority.holder_id().clone(),
		fencing_token: fields.authority.fencing_token().as_i64(),
	})
	.map_err(OncePairPreparationError::Construction)?;
	match fields.once.component().mark_done(current.as_ref(), next) {
		OnceCompletionMutation::KeepExisting => Ok(PreparedOncePair::ExistingCompletion {
			completion: NativeOnceCompletionSnapshot::from_completion(
				current
					.as_ref()
					.expect("mark_done preserves an actual marker"),
			),
			lease,
		}),
		OnceCompletionMutation::Write(completion) => {
			let snapshot = NativeOnceCompletionSnapshot::from_completion(&completion);
			let encoded = fields
				.once_schema
				.encode_record(
					&completion,
					RecordMetadata {
						expires_at: None,
						updated_at: fields.group.observed_at(),
					},
				)
				.map_err(OncePairPreparationError::Completion)?;
			Ok(PreparedOncePair::NewCompletion {
				completion: snapshot,
				lease,
				completion_mutation: RecordMutation::Write(encoded),
			})
		}
	}
}
