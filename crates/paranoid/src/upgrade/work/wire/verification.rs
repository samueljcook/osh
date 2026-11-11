//! Strict fixed-range document verification metadata with required cursor presence.

use std::borrow::Cow;

use primitives_rs::serialization::json::deserialize_required_option;
use serde::{Deserialize, Serialize};

use crate::records::RecordKey;
use crate::scope::wire::ScopePolicyWire;
use crate::upgrade::policy::request::PolicyUpgradeRequestError;

use super::super::UpgradeWorkError;
use super::super::document_verification::{
	DocumentVerificationCheckpoint, DocumentVerificationExtent, DocumentVerificationProcedure,
	DocumentVerificationProgress,
};

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ProcedureWire {
	SelectedWriterProjectionEqualityV1 {},
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ExtentWire<'a> {
	Empty {},
	Through { upper_key: Cow<'a, [u8]> },
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ProgressWire<'a> {
	Scanning {
		#[serde(deserialize_with = "deserialize_required_option")]
		after_key: Option<Cow<'a, [u8]>>,
	},
	Verified {
		#[serde(deserialize_with = "deserialize_required_option")]
		last_key: Option<Cow<'a, [u8]>>,
	},
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DocumentVerificationWire<'a> {
	invariant: ScopePolicyWire<'a>,
	procedure: ProcedureWire,
	extent: ExtentWire<'a>,
	progress: ProgressWire<'a>,
}

impl<'a> DocumentVerificationWire<'a> {
	pub(super) fn from_checkpoint(checkpoint: &'a DocumentVerificationCheckpoint) -> Self {
		Self {
			invariant: ScopePolicyWire::from_policy(checkpoint.invariant()),
			procedure: match checkpoint.procedure() {
				DocumentVerificationProcedure::SelectedWriterProjectionEqualityV1 => {
					ProcedureWire::SelectedWriterProjectionEqualityV1 {}
				}
			},
			extent: match checkpoint.extent() {
				DocumentVerificationExtent::Empty => ExtentWire::Empty {},
				DocumentVerificationExtent::Through { upper_key } => ExtentWire::Through {
					upper_key: Cow::Borrowed(upper_key.as_bytes()),
				},
			},
			progress: match checkpoint.progress() {
				DocumentVerificationProgress::Scanning { after_key } => ProgressWire::Scanning {
					after_key: after_key.as_ref().map(|key| Cow::Borrowed(key.as_bytes())),
				},
				DocumentVerificationProgress::Verified { last_key } => ProgressWire::Verified {
					last_key: last_key.as_ref().map(|key| Cow::Borrowed(key.as_bytes())),
				},
			},
		}
	}
	pub(super) fn into_checkpoint(
		self,
	) -> Result<DocumentVerificationCheckpoint, UpgradeWorkError> {
		Ok(DocumentVerificationCheckpoint::from_fields(
			self.invariant
				.into_policy()
				.map_err(PolicyUpgradeRequestError::from)?,
			match self.procedure {
				ProcedureWire::SelectedWriterProjectionEqualityV1 {} => {
					DocumentVerificationProcedure::SelectedWriterProjectionEqualityV1
				}
			},
			match self.extent {
				ExtentWire::Empty {} => DocumentVerificationExtent::Empty,
				ExtentWire::Through { upper_key } => DocumentVerificationExtent::Through {
					upper_key: RecordKey::new(upper_key.into_owned()),
				},
			},
			match self.progress {
				ProgressWire::Scanning { after_key } => DocumentVerificationProgress::Scanning {
					after_key: after_key.map(|key| RecordKey::new(key.into_owned())),
				},
				ProgressWire::Verified { last_key } => DocumentVerificationProgress::Verified {
					last_key: last_key.map(|key| RecordKey::new(key.into_owned())),
				},
			},
		)?)
	}
}
