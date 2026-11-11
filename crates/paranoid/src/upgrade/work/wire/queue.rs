//! Queue payload conversion checkpoints use work format version 2.

use super::super::UpgradeWorkError;
use crate::installation::contract::policy_wire::ComponentPolicyWire;
use crate::records::RecordKey;
use crate::upgrade::policy::request::PolicyUpgradeRequestError;
use crate::upgrade::work::queue_checkpoint::{QueueJobConversionCheckpoint, QueueJobScanProgress};
use primitives_rs::serialization::json::deserialize_required_option;
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum QueueJobProgressWire {
	Scanning {
		#[serde(deserialize_with = "deserialize_required_option")]
		after_key: Option<Vec<u8>>,
	},
	Scanned {
		#[serde(deserialize_with = "deserialize_required_option")]
		last_key: Option<Vec<u8>>,
	},
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct QueueJobCheckpointWire<'a> {
	invariant: ComponentPolicyWire<'a>,
	progress: QueueJobProgressWire,
}

impl<'a> QueueJobCheckpointWire<'a> {
	pub(super) fn from_checkpoint(checkpoint: &'a QueueJobConversionCheckpoint) -> Self {
		Self {
			invariant: ComponentPolicyWire::from_policy(checkpoint.invariant()),
			progress: match checkpoint.progress() {
				QueueJobScanProgress::Scanning { after_key } => QueueJobProgressWire::Scanning {
					after_key: after_key.as_ref().map(|key| key.as_bytes().to_vec()),
				},
				QueueJobScanProgress::Scanned { last_key } => QueueJobProgressWire::Scanned {
					last_key: last_key.as_ref().map(|key| key.as_bytes().to_vec()),
				},
			},
		}
	}
	pub(super) fn into_checkpoint(self) -> Result<QueueJobConversionCheckpoint, UpgradeWorkError> {
		Ok(QueueJobConversionCheckpoint {
			invariant: self
				.invariant
				.into_policy()
				.map_err(PolicyUpgradeRequestError::from)?,
			progress: match self.progress {
				QueueJobProgressWire::Scanning { after_key } => QueueJobScanProgress::Scanning {
					after_key: after_key.map(RecordKey::new),
				},
				QueueJobProgressWire::Scanned { last_key } => QueueJobScanProgress::Scanned {
					last_key: last_key.map(RecordKey::new),
				},
			},
		})
	}
}
