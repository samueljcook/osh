//! Exact scalar Fleet placement without schema, participant or mutation authority.

use primitives_rs::crypto::algorithms::blake3;
use std::{borrow::Cow, fmt};

use serde::{Deserialize, Serialize};

use crate::db::{InvalidPgIdentifierError, PgIdentifier, PgQualifiedTableName, PgSchemaName};
use crate::records::{RecordIdentityError, RecordNamespace};

use super::super::binding_fingerprint::frame_fingerprint_input;
use super::{BindingRole, NativeInstallationError};
use crate::records::postgres::PgPointStorage;

/// Every namespace is supplied explicitly within one actual ordinary record container.
pub(crate) struct PgNativeFleetScalarStorageFields {
	pub(crate) storage: PgPointStorage,
	pub(crate) once_completion: RecordNamespace,
	pub(crate) cache_epoch: RecordNamespace,
	pub(crate) topic_sequence: RecordNamespace,
	pub(crate) subscription_cursor: RecordNamespace,
}

#[derive(Clone)]
pub(crate) struct PgNativeFleetScalarStorage {
	storage: PgPointStorage,
	placement: FleetScalarPlacement,
}

impl PgNativeFleetScalarStorage {
	pub(crate) fn new(
		fields: PgNativeFleetScalarStorageFields,
	) -> Result<Self, NativeInstallationError> {
		let placement = FleetScalarPlacement {
			table: fields.storage.table().clone(),
			once_completion: fields.once_completion,
			cache_epoch: fields.cache_epoch,
			topic_sequence: fields.topic_sequence,
			subscription_cursor: fields.subscription_cursor,
		};
		placement.require_distinct_namespaces()?;
		Ok(Self {
			storage: fields.storage,
			placement,
		})
	}
	pub(crate) fn storage(&self) -> &PgPointStorage {
		&self.storage
	}
	pub(crate) fn once_completion(&self) -> &RecordNamespace {
		&self.placement.once_completion
	}
	pub(crate) fn cache_epoch(&self) -> &RecordNamespace {
		&self.placement.cache_epoch
	}
	pub(crate) fn topic_sequence(&self) -> &RecordNamespace {
		&self.placement.topic_sequence
	}
	pub(crate) fn subscription_cursor(&self) -> &RecordNamespace {
		&self.placement.subscription_cursor
	}
	pub(super) fn placement(&self) -> &FleetScalarPlacement {
		&self.placement
	}
	pub(super) fn roles(&self) -> [BindingRole<'_>; 4] {
		[
			("Fleet Once completion", self.once_completion()),
			("Fleet Cache epoch", self.cache_epoch()),
			("Fleet Topic sequence", self.topic_sequence()),
			("Fleet subscription cursor", self.subscription_cursor()),
		]
		.map(|(name, namespace)| BindingRole {
			name,
			storage: self.storage(),
			namespace,
			control_key: None,
		})
	}
	pub(super) fn frame_fingerprint(&self, hasher: &mut blake3::Hasher) {
		frame_fingerprint_input(hasher, b"fleet-scalar-placement.v1");
		frame_fingerprint_input(hasher, self.placement.table.schema().as_str().as_bytes());
		frame_fingerprint_input(hasher, self.placement.table.table().as_str().as_bytes());
		for namespace in self.placement.namespaces() {
			frame_fingerprint_input(hasher, namespace.as_bytes());
		}
	}
}
impl fmt::Debug for PgNativeFleetScalarStorage {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("PgNativeFleetScalarStorage { .. }")
	}
}

/// Parsed immutable comparison facts; these fields establish no physical validation proof.
#[derive(Clone, Eq, PartialEq)]
pub(super) struct FleetScalarPlacement {
	table: PgQualifiedTableName,
	once_completion: RecordNamespace,
	cache_epoch: RecordNamespace,
	topic_sequence: RecordNamespace,
	subscription_cursor: RecordNamespace,
}
impl FleetScalarPlacement {
	fn namespaces(&self) -> [&RecordNamespace; 4] {
		[
			&self.once_completion,
			&self.cache_epoch,
			&self.topic_sequence,
			&self.subscription_cursor,
		]
	}
	fn require_distinct_namespaces(&self) -> Result<(), FleetScalarPlacementError> {
		let namespaces = self.namespaces();
		for (right, namespace) in namespaces.iter().enumerate() {
			for (left, prior) in namespaces[..right].iter().enumerate() {
				if namespace == prior {
					return Err(FleetScalarPlacementError::OverlappingNamespaces { left, right });
				}
			}
		}
		Ok(())
	}
}
impl fmt::Debug for FleetScalarPlacement {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("FleetScalarPlacement { .. }")
	}
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FleetScalarPlacementWire<'a> {
	schema: Cow<'a, str>,
	table: Cow<'a, str>,
	once_completion: Cow<'a, [u8]>,
	cache_epoch: Cow<'a, [u8]>,
	topic_sequence: Cow<'a, [u8]>,
	subscription_cursor: Cow<'a, [u8]>,
}
impl<'a> FleetScalarPlacementWire<'a> {
	pub(super) fn from_placement(placement: &'a FleetScalarPlacement) -> Self {
		Self {
			schema: Cow::Borrowed(placement.table.schema().as_str()),
			table: Cow::Borrowed(placement.table.table().as_str()),
			once_completion: Cow::Borrowed(placement.once_completion.as_bytes()),
			cache_epoch: Cow::Borrowed(placement.cache_epoch.as_bytes()),
			topic_sequence: Cow::Borrowed(placement.topic_sequence.as_bytes()),
			subscription_cursor: Cow::Borrowed(placement.subscription_cursor.as_bytes()),
		}
	}
	pub(super) fn into_placement(self) -> Result<FleetScalarPlacement, FleetScalarPlacementError> {
		let placement = FleetScalarPlacement {
			table: PgQualifiedTableName::new(
				PgSchemaName::from_identifier_text(self.schema.as_ref())?,
				PgIdentifier::new(self.table.as_ref())?,
			),
			once_completion: RecordNamespace::new(self.once_completion.into_owned())?,
			cache_epoch: RecordNamespace::new(self.cache_epoch.into_owned())?,
			topic_sequence: RecordNamespace::new(self.topic_sequence.into_owned())?,
			subscription_cursor: RecordNamespace::new(self.subscription_cursor.into_owned())?,
		};
		placement.require_distinct_namespaces()?;
		Ok(placement)
	}
}

#[derive(thiserror::Error)]
pub(crate) enum FleetScalarPlacementError {
	#[error("Fleet scalar recorded table identity is invalid")]
	Identifier(#[from] InvalidPgIdentifierError),
	#[error("Fleet scalar recorded namespace is invalid")]
	Namespace(#[from] RecordIdentityError),
	#[error("Fleet scalar namespaces {left} and {right} overlap")]
	OverlappingNamespaces { left: usize, right: usize },
}
impl fmt::Debug for FleetScalarPlacementError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Identifier(_) => f.write_str("Identifier { .. }"),
			Self::Namespace(_) => f.write_str("Namespace { .. }"),
			Self::OverlappingNamespaces { left, right } => f
				.debug_struct("OverlappingNamespaces")
				.field("left", left)
				.field("right", right)
				.finish(),
		}
	}
}
