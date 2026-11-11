//! Pure application role binding without weakening complete primitive component binding.

use crate::records::state_schema::{StateSchema, StateSchemaCapabilities};

use super::{
	BoundStateSchema, ContractStateReadError, SchemaPolicy, StateContractError,
	check_representation,
};

/// An exact retained-reader binding. Construction requires no encoder or projector.
pub(crate) struct BoundSchemaReader<'a, T, E> {
	binding: BoundStateSchema<'a, T, E>,
}

impl<'a, T, E> BoundSchemaReader<'a, T, E> {
	pub(crate) fn policy(&self) -> &SchemaPolicy {
		self.binding.policy()
	}
	/// Component reader binding already checked retained readers and this exact schema instance.
	pub(super) fn from_checked_binding(binding: BoundStateSchema<'a, T, E>) -> Self {
		Self { binding }
	}

	pub(crate) fn bind(
		declared: StateSchemaCapabilities<'_>,
		schema: &'a StateSchema<T, E>,
		policy: &'a SchemaPolicy,
	) -> Result<Self, StateContractError> {
		check_declaration(declared, schema, policy)?;
		for reader in policy.required_readers() {
			check_representation(&declared, *reader, 0, false)?;
		}
		Ok(Self {
			binding: BoundStateSchema {
				schema,
				policy,
				policy_index: 0,
			},
		})
	}

	pub(crate) fn decode_source(
		&self,
		version: u32,
		bytes: &[u8],
	) -> Result<T, ContractStateReadError<E>> {
		self.binding.decode_source(version, bytes)
	}

	pub(crate) fn decode_document(
		&self,
		bytes: &[u8],
	) -> Result<(u32, T), ContractStateReadError<E>> {
		self.binding.decode_document(bytes)
	}

	pub(crate) fn matches_schema_declaration(&self, schema: &StateSchema<T, E>) -> bool {
		self.binding.matches_schema_declaration(schema)
	}

	pub(crate) fn decode_selected_writer_document(
		&self,
		bytes: &[u8],
	) -> Result<(u32, T), ContractStateReadError<E>> {
		self.binding.decode_selected_writer_document(bytes)
	}
}

/// Checks only the selected writer and its actual reader registration.
/// Complete retained-reader validation belongs to a separate consuming role.
pub(crate) fn bind_producer<'a, T, E>(
	declared: StateSchemaCapabilities<'_>,
	schema: &'a StateSchema<T, E>,
	policy: &'a SchemaPolicy,
) -> Result<BoundStateSchema<'a, T, E>, StateContractError> {
	check_declaration(declared, schema, policy)?;
	check_representation(&declared, policy.writer(), 0, true)?;
	check_representation(&declared, policy.writer(), 0, false)?;
	Ok(BoundStateSchema {
		schema,
		policy,
		policy_index: 0,
	})
}

fn check_declaration<T, E>(
	declared: StateSchemaCapabilities<'_>,
	schema: &StateSchema<T, E>,
	policy: &SchemaPolicy,
) -> Result<(), StateContractError> {
	if !declared.is_same_declaration(schema.capabilities()) {
		return Err(StateContractError::DifferentSchemaDeclaration { policy_index: 0 });
	}
	if policy.identifier() != declared.identifier() {
		return Err(StateContractError::SchemaNotBound);
	}
	Ok(())
}
