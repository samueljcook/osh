//! Fixed synchronous native payload conversion through the actual retained family and schema.

use super::cache::CacheEntryScopeWriteFields;
use super::retained::RetainedPayloadCode;
use super::topic::TopicEventScopeWriteFields;
use super::{PayloadScopeError, PayloadScopeRecordFields};
use crate::fleet::primitive_scope::ManagedPrimitiveRegistration;
use crate::records::{EncodedRecord, RecordIdentity, StoredRecord};
use crate::scope::ScopePolicy;
use crate::scope::payload::managed::{ManagedPayloadError, ManagedPayloadRecord};

pub(crate) struct NativePayloadConversionFields<'a> {
	pub(crate) policy: &'a ScopePolicy,
	pub(crate) identity: &'a RecordIdentity,
	pub(crate) record: &'a StoredRecord,
}
impl ManagedPrimitiveRegistration {
	/// No working application value survives this preparation; original metadata remains exact.
	pub(crate) fn prepare_payload_conversion(
		&self,
		fields: NativePayloadConversionFields<'_>,
	) -> Result<EncodedRecord, NativePayloadConversionError> {
		self.prepare_payload_conversion_inner(&fields)
			.map_err(|error| NativePayloadConversionError {
				source: Box::new(error),
			})
	}
	fn prepare_payload_conversion_inner(
		&self,
		fields: &NativePayloadConversionFields<'_>,
	) -> Result<EncodedRecord, NativePayloadConversionCauseError> {
		let Some(RetainedPayloadCode::Schema(payload)) = self.payload_code() else {
			return Err(NativePayloadConversionCauseError::TypedPayloadCodeRequired);
		};
		match self {
			Self::CacheEntry(code) => {
				let declaration = code.declaration();
				let writer = declaration.bind_writer(declaration.identity(), fields.policy)?;
				let reader = declaration.bind_reader(declaration.identity(), fields.policy)?;
				let state = reader.decode_discovered_record(PayloadScopeRecordFields {
					identity: fields.identity,
					record: fields.record,
				})?;
				let converted = payload.prepare_selected_writer_conversion(
					declaration.identity(),
					fields.policy,
					ManagedPayloadRecord {
						identity: fields.identity,
						payload: state.payload(),
					},
				)?;
				Ok(writer.encode_record(
					CacheEntryScopeWriteFields {
						identity: fields.identity,
						cache_entry_identity: state.identity(),
						epoch: state.epoch(),
						metadata: fields.record.metadata(),
					},
					converted,
				)?)
			}
			Self::TopicEvent(code) => {
				let declaration = code.declaration();
				let writer = declaration.bind_writer(declaration.identity(), fields.policy)?;
				let reader = declaration.bind_reader(declaration.identity(), fields.policy)?;
				let state = reader.decode_discovered_record(PayloadScopeRecordFields {
					identity: fields.identity,
					record: fields.record,
				})?;
				let converted = payload.prepare_selected_writer_conversion(
					declaration.identity(),
					fields.policy,
					ManagedPayloadRecord {
						identity: fields.identity,
						payload: state.payload(),
					},
				)?;
				Ok(writer.encode_record(
					TopicEventScopeWriteFields {
						identity: fields.identity,
						topic_event_identity: state.identity(),
						published_at: state.published_at(),
						metadata: fields.record.metadata(),
					},
					converted,
				)?)
			}
			Self::Semaphore(_) | Self::Throttler(_) => {
				Err(NativePayloadConversionCauseError::TypedPayloadCodeRequired)
			}
		}
	}
}
#[derive(Debug, thiserror::Error)]
pub(crate) enum NativePayloadConversionCauseError {
	#[error("native payload conversion requires structured Cache/Topic code")]
	TypedPayloadCodeRequired,
	#[error("native payload conversion intrinsic envelope or full scoped policy failed")]
	Envelope(#[from] PayloadScopeError),
	#[error("native payload conversion failed through its reader or selected producer")]
	Payload(#[from] ManagedPayloadError),
}
pub(crate) struct NativePayloadConversionError {
	source: Box<NativePayloadConversionCauseError>,
}
impl std::fmt::Debug for NativePayloadConversionError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("NativePayloadConversionError")
			.field("source", &self.source)
			.finish_non_exhaustive()
	}
}
impl std::fmt::Display for NativePayloadConversionError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("actual retained code rejected native payload conversion")
	}
}
impl std::error::Error for NativePayloadConversionError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(self.source.as_ref())
	}
}
