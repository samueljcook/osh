//! Published v10 Fleet scalar import, separate from native scalar schemas.
//!
//! The adapter supplies the actual classified lookup identity and coherent source record.
//! Bytes alone prove neither identity nor lock ownership. Import creates no grant or write.

use crate::installation::contract::{
	BoundComponentContract, ComponentDeclaration, ComponentDeclarationFields, ComponentPolicy,
	ComponentPolicyFields, ComponentSettings, ComponentSettingsDeclaration,
	ComponentSettingsRequirement, SemanticContractDef,
};
use crate::records::{
	EncodedRecord, IndexFields, RecordMetadata, SourcePointRecord, StateTimestampMicros,
};

use super::source_state_keys::{FleetSourceFamily, FleetSourceKey, FleetSourceKeyKind};
use super::state_formats::{
	decode_source_cache_epoch, decode_source_once_completion, decode_source_subscription_cursor,
	decode_source_topic_sequence,
};
use crate::fleet::HolderId;
use crate::fleet::once::state::{NativeOnceCompletion, NativeOnceCompletionFields};
use crate::fleet::scalar_codec::{ScalarStateError, invalid_value, require_permanent};

const COMPONENT: &str = "paranoid.fleet.scalar-source.v10";
const SEMANTICS: &str = "paranoid.fleet.scalar-source.v10.coexist-and-import.v1";

pub(crate) struct ScalarFleetSourceBridge;

impl ScalarFleetSourceBridge {
	pub(crate) fn new() -> Self {
		Self
	}
	pub(crate) fn component_identifier() -> &'static str {
		COMPONENT
	}
	/// Constructs and binds this owner's complete fixed initial policy without value IO.
	pub(crate) fn initial_policy(&self) -> Result<ComponentPolicy, ScalarStateError> {
		let policy = ComponentPolicy::new(ComponentPolicyFields {
			component_identifier: COMPONENT.to_owned(),
			semantic_contract: SEMANTICS.to_owned(),
			schemas: Vec::new(),
			settings: ComponentSettings::Parameterless,
		})?;
		let declaration = self.declaration();
		let binding = declaration.bind(&policy)?;
		self.bind(&binding)?;
		Ok(policy)
	}
	pub(crate) fn declaration(&self) -> ComponentDeclaration<'_, Self> {
		ComponentDeclaration::new(ComponentDeclarationFields {
			identifier: Self::component_identifier(),
			fixed_schemas: Vec::new(),
			settings: ComponentSettingsDeclaration::Parameterless,
			semantics: vec![SemanticContractDef {
				identifier: SEMANTICS,
				transitions: self,
				settings: ComponentSettingsRequirement::Parameterless,
			}],
		})
		.expect("scalar source import declares its actual fixed parser owner")
	}
	pub(crate) fn bind<'a>(
		&'a self,
		contract: &'a BoundComponentContract<'_, '_, Self>,
	) -> Result<BoundScalarFleetSourceBridge<'a>, ScalarStateError> {
		if !std::ptr::eq(self, contract.transitions())
			|| contract.policy().component_identifier() != COMPONENT
			|| contract.policy().semantic_contract() != SEMANTICS
		{
			return Err(ScalarStateError::SourceBindingMismatch);
		}
		Ok(BoundScalarFleetSourceBridge { _bridge: self })
	}
}

pub(crate) struct BoundScalarFleetSourceBridge<'a> {
	_bridge: &'a ScalarFleetSourceBridge,
}
pub(crate) struct ScalarFleetSourceRecord<'a> {
	pub(crate) key: &'a FleetSourceKey,
	pub(crate) record: &'a SourcePointRecord,
}
/// Borrowed bytes and metadata from an explicitly selected coherent source observation.
pub(crate) struct ScalarFleetSourceBody<'a> {
	pub(crate) key: &'a FleetSourceKey,
	pub(crate) body: &'a [u8],
	pub(crate) metadata: RecordMetadata,
}
/// Tentative typed state and original metadata; no native writer or commit proof is carried.
pub(crate) enum ScalarFleetSourceImport {
	OnceCompletion {
		value: NativeOnceCompletion,
		metadata: RecordMetadata,
	},
	CacheEpoch {
		value: i64,
		metadata: RecordMetadata,
	},
	TopicSequence {
		value: i64,
		metadata: RecordMetadata,
	},
	SubscriptionCursor {
		value: i64,
		metadata: RecordMetadata,
	},
}
impl std::fmt::Debug for ScalarFleetSourceImport {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let family = match self {
			Self::OnceCompletion { .. } => FleetSourceFamily::OnceCompletion,
			Self::CacheEpoch { .. } => FleetSourceFamily::CacheEpoch,
			Self::TopicSequence { .. } => FleetSourceFamily::TopicSequence,
			Self::SubscriptionCursor { .. } => FleetSourceFamily::SubscriptionCursor,
		};
		f.debug_struct("ScalarFleetSourceImport")
			.field("family", &family)
			.finish_non_exhaustive()
	}
}
impl BoundScalarFleetSourceBridge<'_> {
	/// The published source writer remains Postcard while v10 writers can still participate.
	pub(crate) fn encode_published_cache_epoch(
		&self,
		value: i64,
		metadata: RecordMetadata,
	) -> Result<EncodedRecord, ScalarStateError> {
		require_permanent(metadata)?;
		let body = postcard::to_allocvec(&value).map_err(|_| {
			ScalarStateError::Source(invalid_value("published cache epoch encoding failed"))
		})?;
		EncodedRecord::derive(body, metadata, |bytes, actual_metadata| {
			require_permanent(actual_metadata)?;
			decode_source_cache_epoch(bytes).map_err(ScalarStateError::Source)?;
			Ok(IndexFields::new())
		})
	}
	/// Selects the actual family reader from the exact parsed key, without probing native JSON.
	pub(crate) fn import(
		&self,
		fields: ScalarFleetSourceRecord<'_>,
	) -> Result<ScalarFleetSourceImport, ScalarFleetSourceImportError> {
		self.import_body(ScalarFleetSourceBody {
			key: fields.key,
			body: fields.record.body(),
			metadata: fields.record.metadata(),
		})
	}
	pub(crate) fn import_body(
		&self,
		fields: ScalarFleetSourceBody<'_>,
	) -> Result<ScalarFleetSourceImport, ScalarFleetSourceImportError> {
		if !matches!(
			fields.key.kind(),
			FleetSourceKeyKind::OnceCompletion(_)
				| FleetSourceKeyKind::CacheEpoch(_)
				| FleetSourceKeyKind::TopicSequence(_)
				| FleetSourceKeyKind::SubscriptionCursor { .. }
		) {
			return Err(ScalarFleetSourceImportError::UnsupportedFamily {
				family: fields.key.kind().family(),
			});
		}
		let metadata = fields.metadata;
		require_permanent(metadata)?;
		let body = fields.body;
		Ok(match fields.key.kind() {
			FleetSourceKeyKind::OnceCompletion(_) => {
				let source =
					decode_source_once_completion(body).map_err(ScalarStateError::Source)?;
				let value = NativeOnceCompletion::new(NativeOnceCompletionFields {
					finished_at: StateTimestampMicros::new(source.finished_at_unix_micros),
					holder: HolderId::new(&source.holder_id).map_err(|_| {
						ScalarStateError::Source(invalid_value(
							"source Once completion holder is invalid",
						))
					})?,
					fencing_token: source.fencing_token,
				})
				.map_err(ScalarStateError::Source)?;
				ScalarFleetSourceImport::OnceCompletion { value, metadata }
			}
			FleetSourceKeyKind::CacheEpoch(_) => ScalarFleetSourceImport::CacheEpoch {
				value: decode_source_cache_epoch(body).map_err(ScalarStateError::Source)?,
				metadata,
			},
			FleetSourceKeyKind::TopicSequence(_) => ScalarFleetSourceImport::TopicSequence {
				value: decode_source_topic_sequence(body).map_err(ScalarStateError::Source)?,
				metadata,
			},
			FleetSourceKeyKind::SubscriptionCursor { .. } => {
				ScalarFleetSourceImport::SubscriptionCursor {
					value: decode_source_subscription_cursor(body)
						.map_err(ScalarStateError::Source)?,
					metadata,
				}
			}
			other => {
				return Err(ScalarFleetSourceImportError::UnsupportedFamily {
					family: other.family(),
				});
			}
		})
	}
}
#[derive(Debug, thiserror::Error)]
pub(crate) enum ScalarFleetSourceImportError {
	#[error("source Fleet scalar family is not implemented by this import declaration: {family:?}")]
	UnsupportedFamily { family: FleetSourceFamily },
	#[error("source Fleet scalar cannot be imported")]
	State(#[from] ScalarStateError),
}
