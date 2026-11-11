//! Actual native backend codec and durable physical registration comparison data.
//!
//! Parsing and encoding establish a format, not installed policy, catalog correctness,
//! projection coverage, or query readiness. Installed updates require the owning controller.

use primitives_rs::crypto::algorithms::blake3;
use std::{borrow::Cow, fmt, num::NonZeroU32};

use primitives_rs::serialization::json::{
	JsonDecodeFailureSummary, JsonFailureCategory, deserialize_required_option,
};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::installation::contract::{
	BoundStateSchema, ContractStateReadError, RepresentationPolicy, SchemaPolicy,
	StateContractError, StateStorageEnvelope,
};
use crate::installation::v10::{
	PgV10SourceDescriptor, PgV10SourceDescriptorWire, V10SourceBindingError,
};
use crate::kv::document::postgres::index::PgDocumentIndexDescriptor;
use crate::records::state_schema::{
	StateRepresentationFormat, StateSchema, StateSchemaError, StateVersion,
};

use super::fleet_scalar::{
	FleetScalarPlacement, FleetScalarPlacementError, FleetScalarPlacementWire,
};
use super::{ImportedPhase, NativeMappingOrigin, PHYSICAL_LAYOUT_VERSION};

mod document_indexes;
pub(crate) use document_indexes::DocumentIndexCodecError;
use document_indexes::{DocumentIndexWire, check_unique_indexes};

const BACKEND_SCHEMA: &str = "paranoid.pg-backend-registration";
const BACKEND_REPRESENTATION_VERSION: u32 = 1;

/// One actual immutable codec registry; clones of storage bindings share its exact instance.
pub(crate) struct PgBackendRegistrationSchema {
	schema: StateSchema<PgBackendRegistration, PgBackendRegistrationCodecError>,
}

impl PgBackendRegistrationSchema {
	pub(crate) fn new() -> Self {
		Self {
			schema: StateSchema::new(crate::records::state_schema::StateSchemaFields {
				identifier: BACKEND_SCHEMA,
				versions: vec![StateVersion {
					version: BACKEND_REPRESENTATION_VERSION,
					format: StateRepresentationFormat::Json,
					decoder: decode_backend,
					encoder: Some(encode_backend),
				}],
			})
			.expect("backend representation declaration is distinct and positive"),
		}
	}

	pub(crate) fn schema(
		&self,
	) -> &StateSchema<PgBackendRegistration, PgBackendRegistrationCodecError> {
		&self.schema
	}

	/// Fixed initial backend codec policy; physical layout and ledger contents remain separate.
	pub(crate) fn initial_schema_policy(&self) -> Result<SchemaPolicy, StateContractError> {
		let representation = RepresentationPolicy {
			version: BACKEND_REPRESENTATION_VERSION,
			envelope: StateStorageEnvelope::NativeJson,
		};
		SchemaPolicy::new(
			self.schema.identifier().to_owned(),
			representation,
			vec![representation],
		)
	}

	pub(crate) fn require_bound_writer(
		&self,
		bound: &BoundStateSchema<'_, PgBackendRegistration, PgBackendRegistrationCodecError>,
	) -> Result<(), PgBackendRegistrationBindingError> {
		self.require_schema_binding(bound)?;
		if bound.policy().writer().envelope != StateStorageEnvelope::NativeJson {
			return Err(PgBackendRegistrationBindingError::NativeWriterRequired);
		}
		Ok(())
	}

	fn require_schema_binding(
		&self,
		bound: &BoundStateSchema<'_, PgBackendRegistration, PgBackendRegistrationCodecError>,
	) -> Result<(), PgBackendRegistrationBindingError> {
		if !bound.matches_schema_declaration(self.schema()) {
			return Err(PgBackendRegistrationBindingError::DifferentSchemaBinding);
		}
		Ok(())
	}

	pub(crate) fn decode_bound_document(
		&self,
		value: &RawValue,
		bound: &BoundStateSchema<'_, PgBackendRegistration, PgBackendRegistrationCodecError>,
	) -> Result<PgBackendRegistration, PgBackendRegistrationBindingError> {
		self.require_schema_binding(bound)?;
		Ok(bound.decode_document(value.get().as_bytes())?.1)
	}

	pub(crate) fn encode_bound_document(
		&self,
		state: &PgBackendRegistration,
		bound: &BoundStateSchema<'_, PgBackendRegistration, PgBackendRegistrationCodecError>,
	) -> Result<Box<RawValue>, PgBackendRegistrationBindingError> {
		self.require_bound_writer(bound)?;
		let bytes = bound.encode(state)?;
		serde_json::from_slice(&bytes)
			.map_err(json_error)
			.map_err(StateSchemaError::Codec)
			.map_err(Into::into)
	}

	/// Format inspection does not bind the stored envelope to an installed reader policy.
	pub(crate) fn decode_document(
		&self,
		value: &RawValue,
	) -> Result<PgBackendRegistration, StateSchemaError<PgBackendRegistrationCodecError>> {
		self.schema
			.decode_document(value.get().as_bytes())
			.map(|(_, state)| state)
	}
}

/// Complete parsed backend state. Mutable index registrations are separate from the
/// immutable storage-map fingerprint and physical container layout version.
pub(crate) struct PgBackendRegistration {
	layout_version: NonZeroU32,
	pub(super) fingerprint: blake3::Hash,
	pub(super) origin: BackendOrigin,
	pub(super) fleet_scalar: Option<FleetScalarPlacement>,
	document_indexes: Vec<PgDocumentIndexDescriptor>,
}

/// Exact ledger classification is comparison data, not physical or mutation authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DocumentIndexLedgerSlot {
	Absent,
	RecordedMatching { entry_index: usize },
}

impl PgBackendRegistration {
	pub(super) fn initial(
		fingerprint: blake3::Hash,
		origin: BackendOrigin,
		fleet_scalar: Option<FleetScalarPlacement>,
	) -> Self {
		Self {
			layout_version: NonZeroU32::new(PHYSICAL_LAYOUT_VERSION)
				.expect("native physical layout version is positive"),
			fingerprint,
			origin,
			fleet_scalar,
			document_indexes: Vec::new(),
		}
	}

	pub(crate) fn require_document_index_slot(
		&self,
		expected: &PgDocumentIndexDescriptor,
	) -> Result<DocumentIndexLedgerSlot, DocumentIndexCodecError> {
		for (entry_index, recorded) in self.document_indexes.iter().enumerate() {
			if recorded.scope() == expected.scope()
				&& recorded.index().identity() == expected.index().identity()
			{
				return if recorded.matches(expected) {
					Ok(DocumentIndexLedgerSlot::RecordedMatching { entry_index })
				} else {
					Err(DocumentIndexCodecError::ConflictingLogical { entry_index })
				};
			}
			if recorded.table().schema() == expected.table().schema()
				&& recorded.index_name() == expected.index_name()
			{
				return Err(DocumentIndexCodecError::ConflictingPhysicalName { entry_index });
			}
		}
		Ok(DocumentIndexLedgerSlot::Absent)
	}

	/// Appends one exact entry without rebuilding unrelated parsed backend fields.
	pub(crate) fn with_document_index(
		mut self,
		descriptor: PgDocumentIndexDescriptor,
	) -> Result<Self, DocumentIndexCodecError> {
		if let DocumentIndexLedgerSlot::RecordedMatching { entry_index } =
			self.require_document_index_slot(&descriptor)?
		{
			return Err(DocumentIndexCodecError::DuplicateLogical { entry_index });
		}
		self.document_indexes.push(descriptor);
		check_unique_indexes(&self.document_indexes)?;
		Ok(self)
	}

	pub(crate) fn document_indexes(&self) -> &[PgDocumentIndexDescriptor] {
		&self.document_indexes
	}
}

impl fmt::Debug for PgBackendRegistration {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("PgBackendRegistration")
			.field("layout_version", &self.layout_version)
			.field("document_index_count", &self.document_indexes.len())
			.finish_non_exhaustive()
	}
}

pub(super) enum BackendOrigin {
	NativeFresh,
	ImportedV10 {
		descriptor: Box<PgV10SourceDescriptor>,
		phase: ImportedPhase,
	},
}

impl BackendOrigin {
	pub(super) fn mapping_origin(&self) -> NativeMappingOrigin {
		match self {
			Self::NativeFresh => NativeMappingOrigin::NativeFresh,
			Self::ImportedV10 { .. } => NativeMappingOrigin::ImportedV10,
		}
	}
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum OriginWire {
	NativeFresh,
	ImportedV10(ImportedOriginWire),
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ImportedOriginWire {
	source_binding_fingerprint: String,
	source_descriptor: PgV10SourceDescriptorWire,
	source_phase: ImportedPhase,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BackendWire<'a> {
	layout_version: u32,
	origin: OriginWire,
	binding_fingerprint: Cow<'a, str>,
	#[serde(deserialize_with = "deserialize_required_option")]
	fleet_scalar: Option<FleetScalarPlacementWire<'a>>,
	document_indexes: Vec<DocumentIndexWire<'a>>,
}

fn decode_backend(bytes: &[u8]) -> Result<PgBackendRegistration, PgBackendRegistrationCodecError> {
	let wire: BackendWire<'_> = serde_json::from_slice(bytes).map_err(json_error)?;
	let layout_version = NonZeroU32::new(wire.layout_version)
		.ok_or(PgBackendRegistrationCodecError::ZeroLayoutVersion)?;
	if layout_version.get() != PHYSICAL_LAYOUT_VERSION {
		return Err(PgBackendRegistrationCodecError::UnsupportedLayoutVersion {
			version: layout_version.get(),
		});
	}
	let fingerprint = parse_fingerprint(&wire.binding_fingerprint)?;
	let fleet_scalar = wire
		.fleet_scalar
		.map(FleetScalarPlacementWire::into_placement)
		.transpose()?;
	let origin = match wire.origin {
		OriginWire::NativeFresh => BackendOrigin::NativeFresh,
		OriginWire::ImportedV10(source) => {
			let source_fingerprint = parse_fingerprint(&source.source_binding_fingerprint)?;
			let descriptor = PgV10SourceDescriptor::parse(source.source_descriptor)?;
			if descriptor.fingerprint() != source_fingerprint {
				return Err(PgBackendRegistrationCodecError::SourceBindingFingerprintMismatch);
			}
			BackendOrigin::ImportedV10 {
				descriptor: Box::new(descriptor),
				phase: source.source_phase,
			}
		}
	};
	let document_indexes = wire
		.document_indexes
		.into_iter()
		.enumerate()
		.map(|(entry_index, entry)| {
			entry.into_descriptor().map_err(|error| {
				PgBackendRegistrationCodecError::DocumentIndex {
					entry_index,
					error: Box::new(error),
				}
			})
		})
		.collect::<Result<Vec<_>, _>>()?;
	check_unique_indexes(&document_indexes)
		.map_err(PgBackendRegistrationCodecError::IndexLedger)?;
	Ok(PgBackendRegistration {
		layout_version,
		fingerprint,
		origin,
		fleet_scalar,
		document_indexes,
	})
}

fn encode_backend(
	state: &PgBackendRegistration,
) -> Result<Vec<u8>, PgBackendRegistrationCodecError> {
	check_unique_indexes(&state.document_indexes)
		.map_err(PgBackendRegistrationCodecError::IndexLedger)?;
	let origin = match &state.origin {
		BackendOrigin::NativeFresh => OriginWire::NativeFresh,
		BackendOrigin::ImportedV10 { descriptor, phase } => {
			OriginWire::ImportedV10(ImportedOriginWire {
				source_binding_fingerprint: descriptor.fingerprint().to_hex().to_string(),
				source_descriptor: descriptor.to_wire(),
				source_phase: *phase,
			})
		}
	};
	serde_json::to_vec(&BackendWire {
		layout_version: state.layout_version.get(),
		origin,
		binding_fingerprint: Cow::Owned(state.fingerprint.to_hex().to_string()),
		fleet_scalar: state
			.fleet_scalar
			.as_ref()
			.map(FleetScalarPlacementWire::from_placement),
		document_indexes: state
			.document_indexes
			.iter()
			.map(DocumentIndexWire::from_descriptor)
			.collect(),
	})
	.map_err(json_error)
}

fn parse_fingerprint(encoded: &str) -> Result<blake3::Hash, PgBackendRegistrationCodecError> {
	let fingerprint = blake3::Hash::from_hex(encoded)
		.map_err(|_| PgBackendRegistrationCodecError::InvalidFingerprint)?;
	if fingerprint.to_hex().as_str() != encoded {
		return Err(PgBackendRegistrationCodecError::InvalidFingerprint);
	}
	Ok(fingerprint)
}

fn json_error(error: serde_json::Error) -> PgBackendRegistrationCodecError {
	let summary = JsonDecodeFailureSummary::from_serde_error(&error);
	PgBackendRegistrationCodecError::Json {
		category: summary.category(),
		line: summary.line(),
		column: summary.column(),
	}
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PgBackendRegistrationCodecError {
	#[error("backend Fleet scalar placement is invalid")]
	FleetScalarPlacement(#[from] FleetScalarPlacementError),
	#[error("backend physical layout version must be positive")]
	ZeroLayoutVersion,
	#[error("backend physical layout version {version} is unsupported")]
	UnsupportedLayoutVersion { version: u32 },
	#[error("backend binding fingerprint is not canonical")]
	InvalidFingerprint,
	#[error("backend source fingerprint disagrees with its descriptor")]
	SourceBindingFingerprintMismatch,
	#[error("backend source descriptor is invalid")]
	SourceDescriptor(#[from] V10SourceBindingError),
	#[error("backend document index entry {entry_index} is invalid")]
	DocumentIndex {
		entry_index: usize,
		#[source]
		error: Box<DocumentIndexCodecError>,
	},
	#[error("backend document index ledger is invalid")]
	IndexLedger(#[source] DocumentIndexCodecError),
	#[error("backend JSON is invalid: {category:?} at line {line}, column {column}")]
	Json {
		category: JsonFailureCategory,
		line: usize,
		column: usize,
	},
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PgBackendRegistrationBindingError {
	#[error("backend registration uses another schema instance")]
	DifferentSchemaBinding,
	#[error("backend registration selected writer requires native JSON")]
	NativeWriterRequired,
	#[error("backend registration reader rejected the stored document")]
	Read(#[from] ContractStateReadError<PgBackendRegistrationCodecError>),
	#[error("backend registration selected writer rejected the value")]
	Representation(#[from] StateSchemaError<PgBackendRegistrationCodecError>),
}
