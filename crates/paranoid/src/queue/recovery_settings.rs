//! Code-owned installed Queue recovery settings with exact positive microsecond meaning.

use super::record_transitions::{
	QueueDeadLetterPolicy, QueueRecoveryPolicy, QueueRecoveryPolicyError,
};
use crate::installation::contract::{
	ComponentSettings, ComponentSettingsBindingError, ComponentSettingsSchema,
	RepresentationPolicy, SchemaPolicy, StateStorageEnvelope,
};
use crate::records::state_schema::{StateRepresentationFormat, StateSchema, StateVersion};
use serde::{Deserialize, Serialize};

const SCHEMA: &str = "paranoid.queue.recovery-settings";
const REPRESENTATION_VERSION: u32 = 1;

/// The actual registry supplies the declaration, selected writer and sealed validator.
pub(in crate::queue) fn recovery_settings_schema()
-> ComponentSettingsSchema<QueueRecoveryPolicy, QueueRecoverySettingsError> {
	ComponentSettingsSchema::new(
		StateSchema::new(crate::records::state_schema::StateSchemaFields {
			identifier: SCHEMA,
			versions: vec![StateVersion {
				version: REPRESENTATION_VERSION,
				format: StateRepresentationFormat::Json,
				decoder: decode_settings,
				encoder: Some(encode_settings),
			}],
		})
		.expect("Queue recovery settings register one actual positive JSON representation"),
	)
}

pub(super) struct QueueInitialRecoverySettingsFields<'a> {
	pub(super) schema: &'a ComponentSettingsSchema<QueueRecoveryPolicy, QueueRecoverySettingsError>,
	pub(super) recovery: &'a QueueRecoveryPolicy,
}

/// Encodes explicit parsed recovery choices through the actual retained settings registry.
pub(super) fn initial_recovery_settings(
	fields: QueueInitialRecoverySettingsFields<'_>,
) -> Result<ComponentSettings, ComponentSettingsBindingError> {
	let writer = RepresentationPolicy {
		version: REPRESENTATION_VERSION,
		envelope: StateStorageEnvelope::NativeJson,
	};
	let policy = SchemaPolicy::new(
		fields.schema.schema().identifier().to_owned(),
		writer,
		vec![writer],
	)
	.map_err(ComponentSettingsBindingError::Policy)?;
	fields.schema.encode_settings(policy, fields.recovery)
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SettingsWire {
	stale_threshold_micros: String,
	dead_letters: DeadLetterWire,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum DeadLetterWire {
	KeepFailed,
	MoveToDeadLetters,
}

fn decode_settings(bytes: &[u8]) -> Result<QueueRecoveryPolicy, QueueRecoverySettingsError> {
	let wire: SettingsWire = serde_json::from_slice(bytes).map_err(json_error)?;
	let threshold: i64 =
		primitives_rs::encoding::decimal::parse_canonical_decimal(&wire.stale_threshold_micros)
			.map_err(|_| QueueRecoverySettingsError::InvalidThreshold)?;
	if threshold <= 0 {
		return Err(QueueRecoverySettingsError::InvalidThreshold);
	}
	let dead_letters = match wire.dead_letters {
		DeadLetterWire::KeepFailed => QueueDeadLetterPolicy::KeepFailed,
		DeadLetterWire::MoveToDeadLetters => QueueDeadLetterPolicy::MoveToDeadLetters,
	};
	QueueRecoveryPolicy::from_positive_micros(threshold, dead_letters)
		.map_err(QueueRecoverySettingsError::Policy)
}

fn encode_settings(policy: &QueueRecoveryPolicy) -> Result<Vec<u8>, QueueRecoverySettingsError> {
	serde_json::to_vec(&SettingsWire {
		stale_threshold_micros: policy.stale_threshold_micros().to_string(),
		dead_letters: match policy.dead_letters() {
			QueueDeadLetterPolicy::KeepFailed => DeadLetterWire::KeepFailed,
			QueueDeadLetterPolicy::MoveToDeadLetters => DeadLetterWire::MoveToDeadLetters,
		},
	})
	.map_err(json_error)
}

/// Sanitized codec failures retain actual parsed-policy errors without settings contents.
#[derive(Debug, thiserror::Error)]
pub(in crate::queue) enum QueueRecoverySettingsError {
	#[error("Queue recovery threshold must be a canonical positive signed microsecond integer")]
	InvalidThreshold,
	#[error("Queue recovery policy rejected its parsed settings")]
	Policy(#[source] QueueRecoveryPolicyError),
	#[error("Queue recovery JSON is invalid: {category:?} at line {line}, column {column}")]
	Json {
		category: serde_json::error::Category,
		line: usize,
		column: usize,
	},
}

fn json_error(error: serde_json::Error) -> QueueRecoverySettingsError {
	QueueRecoverySettingsError::Json {
		category: error.classify(),
		line: error.line(),
		column: error.column(),
	}
}
