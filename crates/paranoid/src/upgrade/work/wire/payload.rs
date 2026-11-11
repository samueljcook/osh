//! Closed Primitive payload conversion procedure and explicit required cursor values.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::records::RecordKey;
use crate::scope::wire::ScopePolicyWire;
use crate::upgrade::policy::request::PolicyUpgradeRequestError;

use super::super::UpgradeWorkError;
use super::super::payload_checkpoint::{
	PayloadCheckpointProcedure, PayloadConversionCheckpoint, PayloadScanProgress,
};
use primitives_rs::serialization::json::deserialize_required_option;

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ProcedureWire {
	PrimitivePayloadSelectedWriterConversionV1 {},
}
#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ProgressWire<'a> {
	Scanning {
		#[serde(deserialize_with = "deserialize_required_option")]
		after_key: Option<Cow<'a, [u8]>>,
	},
	Scanned {
		#[serde(deserialize_with = "deserialize_required_option")]
		last_key: Option<Cow<'a, [u8]>>,
	},
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PayloadCheckpointWire<'a> {
	invariant: ScopePolicyWire<'a>,
	procedure: ProcedureWire,
	progress: ProgressWire<'a>,
}
impl<'a> PayloadCheckpointWire<'a> {
	pub(super) fn from_checkpoint(checkpoint: &'a PayloadConversionCheckpoint) -> Self {
		Self {
			invariant: ScopePolicyWire::from_policy(checkpoint.invariant()),
			procedure: match checkpoint.procedure() {
				PayloadCheckpointProcedure::PrimitivePayloadSelectedWriterConversionV1 => {
					ProcedureWire::PrimitivePayloadSelectedWriterConversionV1 {}
				}
			},
			progress: match checkpoint.progress() {
				PayloadScanProgress::Scanning { after_key } => ProgressWire::Scanning {
					after_key: after_key.as_ref().map(|key| Cow::Borrowed(key.as_bytes())),
				},
				PayloadScanProgress::Scanned { last_key } => ProgressWire::Scanned {
					last_key: last_key.as_ref().map(|key| Cow::Borrowed(key.as_bytes())),
				},
			},
		}
	}
	pub(super) fn into_checkpoint(self) -> Result<PayloadConversionCheckpoint, UpgradeWorkError> {
		Ok(PayloadConversionCheckpoint {
			invariant: self
				.invariant
				.into_policy()
				.map_err(PolicyUpgradeRequestError::from)?,
			procedure: match self.procedure {
				ProcedureWire::PrimitivePayloadSelectedWriterConversionV1 {} => {
					PayloadCheckpointProcedure::PrimitivePayloadSelectedWriterConversionV1
				}
			},
			progress: match self.progress {
				ProgressWire::Scanning { after_key } => PayloadScanProgress::Scanning {
					after_key: after_key.map(|key| RecordKey::new(key.into_owned())),
				},
				ProgressWire::Scanned { last_key } => PayloadScanProgress::Scanned {
					last_key: last_key.map(|key| RecordKey::new(key.into_owned())),
				},
			},
		})
	}
}
