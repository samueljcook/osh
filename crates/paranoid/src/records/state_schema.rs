//! Paranoid's bindings to shared versioned serialization.

pub(crate) use primitives_rs::serialization::versioned_state::{
	EncodedStateDocument as NativeStateDocument, ParsedStateDocument,
	StateDocumentError as NativeDocumentError, StateRepresentationCapability,
	StateSchemaCapabilities,
};
pub use primitives_rs::serialization::versioned_state::{
	StateDecoder, StateEncoder, StateRepresentationFormat, StateSchema, StateSchemaError,
	StateSchemaFields, StateVersion,
};
