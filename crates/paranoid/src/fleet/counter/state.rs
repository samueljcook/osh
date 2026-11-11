//! Counter schemas and transitions independent of storage mechanisms.

use crate::fleet::scalar_codec::ValueError;
use crate::fleet::scalar_codec::{decode_integer, encode_integer};
use crate::fleet::v10::state_formats::decode_source_counter;
use crate::installation::contract::{
	BoundComponentContract, BoundStateSchema, ComponentDeclaration, ComponentDeclarationFields,
	ComponentPolicy, ComponentPolicyFields, ComponentSettings, ComponentSettingsDeclaration,
	ComponentSettingsRequirement, ContractStateReadError, RepresentationPolicy, SchemaPolicy,
	SemanticContractDef, StateContractError, StateStorageEnvelope,
};
use crate::records::state_schema::{
	StateRepresentationFormat, StateSchema, StateSchemaError, StateVersion,
};
use crate::records::{
	EncodedRecord, IndexFields, PointRecordLock, PointRecordRead, RecordMetadata,
	RelocatingPointRecordLock, RelocationRead, RelocationRecord, SourcePointRecord,
};

const COUNTER_SCHEMA: &str = "paranoid.counter";
const COUNTER_COMPONENT: &str = "paranoid.counter";
const COUNTER_SEMANTICS: &str = "paranoid.counter.v1";
const SOURCE_COUNTER_REPRESENTATION_VERSION: u32 = 1;
const JSON_COUNTER_REPRESENTATION_VERSION: u32 = 2;

/// A counter operation or its rejection cleanup failed.
#[derive(Debug)]
pub(crate) enum CounterOperationError<E> {
	Representation(CounterRepresentationError),
	ArithmeticOverflow,
	Storage(E),
	CleanupFailed {
		operation_error: Box<CounterOperationError<E>>,
		cleanup_error: E,
	},
}

impl<E> std::fmt::Display for CounterOperationError<E> {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter.write_str(match self {
			Self::Representation(_) => "counter state could not be interpreted or encoded",
			Self::ArithmeticOverflow => "counter arithmetic overflowed the signed 64-bit range",
			Self::Storage(_) => "counter storage operation failed",
			Self::CleanupFailed { .. } => {
				"counter operation failed and its absent reservation could not be removed"
			}
		})
	}
}

impl<E: std::error::Error + 'static> std::error::Error for CounterOperationError<E> {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::Representation(error) => Some(error),
			Self::Storage(error) => Some(error),
			Self::CleanupFailed {
				operation_error, ..
			} => Some(operation_error.as_ref()),
			Self::ArithmeticOverflow => None,
		}
	}
}

/// Reader policy, selected writer, and native physical mapping failures.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CounterRepresentationError {
	#[error("counter representation is not readable under the bound policy")]
	Read(#[from] ContractStateReadError<ValueError>),
	#[error("counter could not encode the selected representation")]
	Encode(#[from] StateSchemaError<ValueError>),
	#[error("native counter storage requires a native JSON writer")]
	NativeWriterRequired,
	#[error("published counter source storage requires its declared source-byte writer")]
	SourceWriterRequired,
}

/// The code-owned counter schema and point-operation transitions.
pub(crate) struct CounterComponent {
	schema: StateSchema<i64, ValueError>,
}

/// The two initial storage routes require different actual Counter writers.
pub(crate) enum CounterInitialOrigin {
	NativeFresh,
	ImportedCoexisting,
}

pub(crate) struct CounterInitialPolicyFields {
	pub(crate) origin: CounterInitialOrigin,
}

/// The Counter storage format that an upgrade targets: the v10 format, the JSON format with
/// the v10 format still readable, or the JSON format alone.
///
/// The choice does not decide whether v10 deployments can still write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CounterUpgradeSelection {
	/// Counters keep the v10 byte format while v10 deployments still write.
	PublishedCoexistence,
	/// Counters are written and read as JSON only.
	NativeOnly,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum CounterInitialPolicyError {
	#[error("initial Counter policy does not bind its declaration")]
	Contract(#[from] StateContractError),
	#[error("initial Counter policy does not select the required storage writer")]
	Representation(#[from] CounterRepresentationError),
}

/// Counter-specific binding created from this component's actual registered schema instance.
pub(crate) struct CounterSchemaBinding<'a> {
	schema: BoundStateSchema<'a, i64, ValueError>,
}

pub(crate) struct CounterReaderRetirementFields<'a> {
	pub(crate) component: &'a CounterComponent,
	pub(crate) current: &'a ComponentPolicy,
	pub(crate) target: &'a ComponentPolicy,
}

/// Exact code-bound reader-only narrowing; the executor must prove physical source retirement.
pub(crate) struct CounterReaderRetirement<'a> {
	policy: &'a ComponentPolicy,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum CounterReaderRetirementError {
	#[error("Counter reader retirement does not bind the component declaration")]
	Contract(#[from] StateContractError),
	#[error("Counter reader retirement requires the native writer")]
	Representation(#[from] CounterRepresentationError),
	#[error("Counter reader retirement cannot change its semantic contract")]
	SemanticChanged,
	#[error("Counter reader retirement cannot change its settings")]
	SettingsChanged,
	#[error("Counter reader retirement cannot change its selected writer")]
	WriterChanged,
	#[error("Counter reader retirement cannot add required reader {representation:?}")]
	ReaderAdded {
		representation: RepresentationPolicy,
	},
	#[error("Counter reader retirement cannot remove native reader {representation:?}")]
	NativeReaderRemoved {
		representation: RepresentationPolicy,
	},
}

impl<'a> CounterReaderRetirement<'a> {
	pub(crate) fn new(
		fields: CounterReaderRetirementFields<'a>,
	) -> Result<Self, CounterReaderRetirementError> {
		let declaration = fields.component.declaration();
		let current = declaration.bind(fields.current)?;
		let target = declaration.bind(fields.target)?;
		let current = fields.component.bind_schema(&current)?;
		let target = fields.component.bind_schema(&target)?;
		current.require_native_writer()?;
		target.require_native_writer()?;
		if fields.current.semantic_contract() != fields.target.semantic_contract() {
			return Err(CounterReaderRetirementError::SemanticChanged);
		}
		if !fields
			.current
			.settings()
			.matches_settings(fields.target.settings())
		{
			return Err(CounterReaderRetirementError::SettingsChanged);
		}
		let current = current.schema.policy();
		let target = target.schema.policy();
		if current.writer() != target.writer() {
			return Err(CounterReaderRetirementError::WriterChanged);
		}
		for representation in target.required_readers() {
			if !current.required_readers().contains(representation) {
				return Err(CounterReaderRetirementError::ReaderAdded {
					representation: *representation,
				});
			}
		}
		for representation in current.required_readers() {
			if !target.required_readers().contains(representation)
				&& representation.envelope != StateStorageEnvelope::SourceBytes
			{
				return Err(CounterReaderRetirementError::NativeReaderRemoved {
					representation: *representation,
				});
			}
		}
		Ok(Self {
			policy: fields.target,
		})
	}
	pub(crate) fn policy(&self) -> &ComponentPolicy {
		self.policy
	}
}

impl CounterSchemaBinding<'_> {
	/// Maintenance checks complete native bodies even when their metadata is expired.
	pub(crate) fn require_native_record(
		&self,
		record: &crate::records::StoredRecord,
	) -> Result<(), ContractStateReadError<ValueError>> {
		self.schema.decode_document(record.body()).map(|_| ())
	}
	pub(crate) fn require_source_writer(&self) -> Result<(), CounterRepresentationError> {
		let writer = self.schema.policy().writer();
		if writer.envelope != StateStorageEnvelope::SourceBytes
			|| writer.version != SOURCE_COUNTER_REPRESENTATION_VERSION
		{
			Err(CounterRepresentationError::SourceWriterRequired)
		} else {
			Ok(())
		}
	}

	pub(crate) fn require_native_writer(&self) -> Result<(), CounterRepresentationError> {
		if self.schema.policy().writer().envelope != StateStorageEnvelope::NativeJson {
			Err(CounterRepresentationError::NativeWriterRequired)
		} else {
			Ok(())
		}
	}
}

impl CounterComponent {
	pub(crate) fn new() -> Self {
		let schema = StateSchema::new(crate::records::state_schema::StateSchemaFields {
			identifier: COUNTER_SCHEMA,
			versions: vec![
				StateVersion {
					version: SOURCE_COUNTER_REPRESENTATION_VERSION,
					format: StateRepresentationFormat::Bytes,
					decoder: decode_source_counter,
					encoder: Some(encode_source_counter),
				},
				StateVersion {
					version: JSON_COUNTER_REPRESENTATION_VERSION,
					format: StateRepresentationFormat::Json,
					decoder: decode_json_counter,
					encoder: Some(encode_json_counter),
				},
			],
		})
		.expect("the counter declares distinct positive representation versions");
		Self { schema }
	}

	/// Derives support from this actual registry and this transition implementation.
	pub(crate) fn declaration(&self) -> ComponentDeclaration<'_, Self> {
		ComponentDeclaration::new(ComponentDeclarationFields {
			identifier: COUNTER_COMPONENT,
			fixed_schemas: vec![self.schema.capabilities()],
			settings: ComponentSettingsDeclaration::Parameterless,
			semantics: vec![SemanticContractDef {
				identifier: COUNTER_SEMANTICS,
				transitions: self,
				settings: ComponentSettingsRequirement::Parameterless,
			}],
		})
		.expect("counter declares one schema and one semantic implementation")
	}

	pub(crate) fn schema(&self) -> &StateSchema<i64, ValueError> {
		&self.schema
	}

	/// Fixed initial policy for the explicit route; source retirement later changes its writer.
	pub(crate) fn initial_policy(
		&self,
		fields: CounterInitialPolicyFields,
	) -> Result<ComponentPolicy, CounterInitialPolicyError> {
		self.upgrade_policy(match fields.origin {
			CounterInitialOrigin::NativeFresh => CounterUpgradeSelection::NativeOnly,
			CounterInitialOrigin::ImportedCoexisting => {
				CounterUpgradeSelection::PublishedCoexistence
			}
		})
	}

	/// Derives selected metadata through this component's actual representation owners.
	pub(crate) fn upgrade_policy(
		&self,
		selection: CounterUpgradeSelection,
	) -> Result<ComponentPolicy, CounterInitialPolicyError> {
		let native = RepresentationPolicy {
			version: JSON_COUNTER_REPRESENTATION_VERSION,
			envelope: StateStorageEnvelope::NativeJson,
		};
		let published = RepresentationPolicy {
			version: SOURCE_COUNTER_REPRESENTATION_VERSION,
			envelope: StateStorageEnvelope::SourceBytes,
		};
		let (writer, readers) = match selection {
			CounterUpgradeSelection::PublishedCoexistence => (published, vec![published]),

			CounterUpgradeSelection::NativeOnly => (native, vec![native]),
		};
		let policy = ComponentPolicy::new(ComponentPolicyFields {
			component_identifier: COUNTER_COMPONENT.to_owned(),
			semantic_contract: COUNTER_SEMANTICS.to_owned(),
			schemas: vec![SchemaPolicy::new(
				self.schema.identifier().to_owned(),
				writer,
				readers,
			)?],
			settings: ComponentSettings::Parameterless,
		})?;
		let declaration = self.declaration();
		let bound = declaration.bind(&policy)?;
		let schema = self.bind_schema(&bound)?;
		match selection {
			CounterUpgradeSelection::NativeOnly => schema.require_native_writer()?,
			CounterUpgradeSelection::PublishedCoexistence => schema.require_source_writer()?,
		}
		Ok(policy)
	}

	pub(crate) fn bind_schema<'a>(
		&'a self,
		component: &'a BoundComponentContract<'_, '_, Self>,
	) -> Result<CounterSchemaBinding<'a>, StateContractError> {
		component
			.bind_schema(&self.schema)
			.map(|schema| CounterSchemaBinding { schema })
	}

	/// Returns zero for an absent or expired value; a live malformed value is an error.
	pub(crate) fn decode_value(
		&self,
		snapshot: &PointRecordRead,
		schema: &CounterSchemaBinding<'_>,
	) -> Result<i64, ContractStateReadError<ValueError>> {
		match snapshot.live_record() {
			Some(record) => schema
				.schema
				.decode_document(record.body())
				.map(|(_, value)| value),
			None => Ok(0),
		}
	}

	/// Reads the explicitly identified published source row without probing a JSON parser.
	pub(crate) fn decode_source_value(
		&self,
		snapshot: &PointRecordRead,
		schema: &CounterSchemaBinding<'_>,
	) -> Result<i64, ContractStateReadError<ValueError>> {
		match snapshot.live_record() {
			Some(record) => schema
				.schema
				.decode_source(SOURCE_COUNTER_REPRESENTATION_VERSION, record.body()),
			None => Ok(0),
		}
	}

	/// Applies the existing counter semantics while published writers still use source rows.
	pub(crate) async fn add_source<L: PointRecordLock>(
		&self,
		lock: L,
		delta: i64,
		schema: &CounterSchemaBinding<'_>,
	) -> Result<i64, CounterOperationError<L::Error>> {
		let prepared = self.prepare_add(
			self.decode_source_value(lock.snapshot(), schema),
			delta,
			schema,
			RecordMetadata {
				expires_at: None,
				updated_at: lock.snapshot().observed_at(),
			},
			CounterPointRepresentation::PublishedSource,
		);
		match prepared {
			Ok((value, record)) => {
				lock.write(record)
					.await
					.map_err(CounterOperationError::Storage)?;
				Ok(value)
			}
			Err(error) => Err(clean_after_rejection(lock, error).await),
		}
	}

	/// Replaces a source value through the exact bound source writer and preserves source layout.
	pub(crate) async fn set_source_value<L: PointRecordLock>(
		&self,
		lock: L,
		value: i64,
		schema: &CounterSchemaBinding<'_>,
	) -> Result<(), CounterOperationError<L::Error>> {
		let encoded = self.encode_point_record(
			value,
			schema,
			RecordMetadata {
				expires_at: None,
				updated_at: lock.snapshot().observed_at(),
			},
			CounterPointRepresentation::PublishedSource,
		);
		match encoded {
			Ok(record) => lock
				.write(record)
				.await
				.map_err(CounterOperationError::Storage),
			Err(error) => {
				Err(clean_after_rejection(lock, CounterOperationError::Representation(error)).await)
			}
		}
	}

	/// Selects the declared source parser or native document parser from physical origin.
	pub(crate) fn decode_relocated_value(
		&self,
		snapshot: &RelocationRead,
		schema: &CounterSchemaBinding<'_>,
	) -> Result<i64, ContractStateReadError<ValueError>> {
		match snapshot.live_record() {
			Some(RelocationRecord::Source(record)) => schema
				.schema
				.decode_source(SOURCE_COUNTER_REPRESENTATION_VERSION, record.body()),
			Some(RelocationRecord::Native(record)) => schema
				.schema
				.decode_document(record.body())
				.map(|(_, value)| value),
			Some(RelocationRecord::Absent) | None => Ok(0),
		}
	}

	/// Adds under mutation authority and the exact admitted reader/writer policy.
	pub(crate) async fn add<L: PointRecordLock>(
		&self,
		lock: L,
		delta: i64,
		schema: &CounterSchemaBinding<'_>,
	) -> Result<i64, CounterOperationError<L::Error>> {
		let prepared = self.prepare_add(
			self.decode_value(lock.snapshot(), schema),
			delta,
			schema,
			RecordMetadata {
				expires_at: None,
				updated_at: lock.snapshot().observed_at(),
			},
			CounterPointRepresentation::NativeDocument,
		);
		match prepared {
			Ok((value, record)) => {
				lock.write(record)
					.await
					.map_err(CounterOperationError::Storage)?;
				Ok(value)
			}
			Err(error) => Err(clean_after_rejection(lock, error).await),
		}
	}

	/// Adds and relocates atomically after the installation has excluded source writers.
	pub(crate) async fn add_relocated<L: RelocatingPointRecordLock>(
		&self,
		lock: L,
		delta: i64,
		schema: &CounterSchemaBinding<'_>,
	) -> Result<i64, CounterOperationError<L::Error>> {
		let prepared = self.prepare_add(
			self.decode_relocated_value(lock.snapshot(), schema),
			delta,
			schema,
			RecordMetadata {
				expires_at: None,
				updated_at: lock.snapshot().observed_at(),
			},
			CounterPointRepresentation::NativeDocument,
		);
		match prepared {
			Ok((value, record)) => {
				lock.write(record)
					.await
					.map_err(CounterOperationError::Storage)?;
				Ok(value)
			}
			Err(error) => Err(rejection_error(error, lock.keep().await)),
		}
	}

	/// Replaces and relocates an exact value without interpreting the prior body.
	pub(crate) async fn set_relocated_value<L: RelocatingPointRecordLock>(
		&self,
		lock: L,
		value: i64,
		schema: &CounterSchemaBinding<'_>,
	) -> Result<(), CounterOperationError<L::Error>> {
		let encoded = self.encode_record_with_metadata(
			value,
			schema,
			RecordMetadata {
				expires_at: None,
				updated_at: lock.snapshot().observed_at(),
			},
		);
		match encoded {
			Ok(record) => lock
				.write(record)
				.await
				.map_err(CounterOperationError::Storage),
			Err(error) => Err(rejection_error(
				CounterOperationError::Representation(error),
				lock.keep().await,
			)),
		}
	}

	fn prepare_add<E>(
		&self,
		current: Result<i64, ContractStateReadError<ValueError>>,
		delta: i64,
		schema: &CounterSchemaBinding<'_>,
		metadata: RecordMetadata,
		representation: CounterPointRepresentation,
	) -> Result<(i64, EncodedRecord), CounterOperationError<E>> {
		let value = current
			.map_err(|error| CounterOperationError::Representation(error.into()))?
			.checked_add(delta)
			.ok_or(CounterOperationError::ArithmeticOverflow)?;
		self.encode_point_record(value, schema, metadata, representation)
			.map(|record| (value, record))
			.map_err(CounterOperationError::Representation)
	}

	/// Stores an exact value under mutation authority and the exact admitted writer.
	pub(crate) async fn set_value<L: PointRecordLock>(
		&self,
		lock: L,
		value: i64,
		schema: &CounterSchemaBinding<'_>,
	) -> Result<(), CounterOperationError<L::Error>> {
		match self.encode_record(lock.snapshot(), value, schema) {
			Ok(record) => lock
				.write(record)
				.await
				.map_err(CounterOperationError::Storage),
			Err(error) => {
				Err(clean_after_rejection(lock, CounterOperationError::Representation(error)).await)
			}
		}
	}

	/// Converts a source value while retaining its absolute expiry and modification time.
	pub(crate) fn convert_source_record(
		&self,
		record: &SourcePointRecord,
		schema: &CounterSchemaBinding<'_>,
	) -> Result<EncodedRecord, CounterRepresentationError> {
		let value = schema
			.schema
			.decode_source(SOURCE_COUNTER_REPRESENTATION_VERSION, record.body())?;
		self.encode_record_with_metadata(value, schema, record.metadata())
	}

	fn encode_record(
		&self,
		snapshot: &PointRecordRead,
		value: i64,
		schema: &CounterSchemaBinding<'_>,
	) -> Result<EncodedRecord, CounterRepresentationError> {
		self.encode_record_with_metadata(
			value,
			schema,
			RecordMetadata {
				expires_at: None,
				updated_at: snapshot.observed_at(),
			},
		)
	}

	fn encode_record_with_metadata(
		&self,
		value: i64,
		schema: &CounterSchemaBinding<'_>,
		metadata: RecordMetadata,
	) -> Result<EncodedRecord, CounterRepresentationError> {
		self.encode_point_record(
			value,
			schema,
			metadata,
			CounterPointRepresentation::NativeDocument,
		)
	}

	fn encode_point_record(
		&self,
		value: i64,
		schema: &CounterSchemaBinding<'_>,
		metadata: RecordMetadata,
		representation: CounterPointRepresentation,
	) -> Result<EncodedRecord, CounterRepresentationError> {
		match representation {
			CounterPointRepresentation::PublishedSource => schema.require_source_writer()?,
			CounterPointRepresentation::NativeDocument => schema.require_native_writer()?,
		}
		let body = schema.schema.encode(&value)?;
		EncodedRecord::derive(body, metadata, |_, _| Ok(IndexFields::new()))
	}
}

enum CounterPointRepresentation {
	PublishedSource,
	NativeDocument,
}

async fn clean_after_rejection<L: PointRecordLock>(
	lock: L,
	operation_error: CounterOperationError<L::Error>,
) -> CounterOperationError<L::Error> {
	rejection_error(operation_error, lock.keep().await)
}

fn rejection_error<E>(
	operation_error: CounterOperationError<E>,
	cleanup: Result<(), E>,
) -> CounterOperationError<E> {
	match cleanup {
		Ok(()) => operation_error,
		Err(cleanup_error) => CounterOperationError::CleanupFailed {
			operation_error: Box::new(operation_error),
			cleanup_error,
		},
	}
}

fn decode_json_counter(bytes: &[u8]) -> Result<i64, ValueError> {
	decode_integer(bytes)
}

fn encode_json_counter(value: &i64) -> Result<Vec<u8>, ValueError> {
	encode_integer(value)
}

fn encode_source_counter(value: &i64) -> Result<Vec<u8>, ValueError> {
	postcard::to_allocvec(value).map_err(|_| ValueError::Representation {
		reason: "counter value could not be encoded in the source representation".to_owned(),
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_json_counter_accepts_only_canonical_decimal_text() {
		for value in [0, -5, 7, i64::MIN, i64::MAX] {
			let encoded = encode_json_counter(&value).expect("encode a counter");
			assert_eq!(
				decode_json_counter(&encoded).expect("decode a counter"),
				value
			);
		}
		for text in ["+5", "007", "-0", " 5", "5 ", ""] {
			let body = serde_json::to_vec(&serde_json::json!({ "value": text })).expect("a body");
			assert!(
				decode_json_counter(&body).is_err(),
				"{text:?} is not canonical counter text"
			);
		}
	}
}
