//! Native permanent topic sequence and subscription cursor metadata.
//!
//! Event persistence, handler execution, and private polling authority belong to executors.

use crate::fleet::scalar_codec::ValueError;
use crate::installation::contract::{
	BoundComponentContract, ComponentDeclaration, ComponentDeclarationFields, ComponentPolicy,
	ComponentPolicyFields, ComponentSettings, ComponentSettingsDeclaration,
	ComponentSettingsRequirement, SemanticContractDef,
};
use crate::records::state_schema::StateSchema;

use crate::fleet::scalar_codec::{
	ScalarSchemaBinding, ScalarStateError, bind_native_scalar, decode_nonnegative_integer,
	encode_nonnegative_integer, initial_native_schema_policy, native_schema, require_nonnegative,
};

const COMPONENT: &str = "paranoid.topic-metadata";
const SEQUENCE_SCHEMA: &str = "paranoid.topic-sequence";
const CURSOR_SCHEMA: &str = "paranoid.subscription-cursor";
const SEMANTICS: &str = "paranoid.topic-metadata.v1";

pub(crate) struct TopicMetadataComponent {
	sequence: StateSchema<i64, ValueError>,
	cursor: StateSchema<i64, ValueError>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SubscriptionCursorMutation {
	KeepExisting,
	Set(i64),
}
impl TopicMetadataComponent {
	pub(crate) fn new() -> Self {
		Self {
			sequence: native_schema(
				SEQUENCE_SCHEMA,
				decode_nonnegative_integer,
				encode_nonnegative_integer,
			),
			cursor: native_schema(
				CURSOR_SCHEMA,
				decode_nonnegative_integer,
				encode_nonnegative_integer,
			),
		}
	}
	/// Constructs and binds this owner's complete fixed initial policy without value IO.
	pub(crate) fn initial_policy(&self) -> Result<ComponentPolicy, ScalarStateError> {
		let policy = ComponentPolicy::new(ComponentPolicyFields {
			component_identifier: COMPONENT.to_owned(),
			semantic_contract: SEMANTICS.to_owned(),
			schemas: vec![
				initial_native_schema_policy(&self.sequence)?,
				initial_native_schema_policy(&self.cursor)?,
			],
			settings: ComponentSettings::Parameterless,
		})?;
		let declaration = self.declaration();
		let binding = declaration.bind(&policy)?;
		self.bind_sequence(&binding)?;
		self.bind_cursor(&binding)?;
		Ok(policy)
	}
	pub(crate) fn declaration(&self) -> ComponentDeclaration<'_, Self> {
		ComponentDeclaration::new(ComponentDeclarationFields {
			identifier: COMPONENT,
			fixed_schemas: vec![self.sequence.capabilities(), self.cursor.capabilities()],
			settings: ComponentSettingsDeclaration::Parameterless,
			semantics: vec![SemanticContractDef {
				identifier: SEMANTICS,
				transitions: self,
				settings: ComponentSettingsRequirement::Parameterless,
			}],
		})
		.expect("topic metadata declares both actual schemas and one transition owner")
	}
	pub(crate) fn bind_sequence<'a>(
		&'a self,
		component: &'a BoundComponentContract<'_, '_, Self>,
	) -> Result<ScalarSchemaBinding<'a, i64>, ScalarStateError> {
		bind_native_scalar(self, component, &self.sequence)
	}
	pub(crate) fn bind_cursor<'a>(
		&'a self,
		component: &'a BoundComponentContract<'_, '_, Self>,
	) -> Result<ScalarSchemaBinding<'a, i64>, ScalarStateError> {
		bind_native_scalar(self, component, &self.cursor)
	}
	pub(crate) fn sequence(&self, current: Option<i64>) -> Result<i64, ScalarStateError> {
		require_nonnegative(current.unwrap_or(0)).map_err(ScalarStateError::Domain)
	}
	/// The executor must persist this sequence and its corresponding event atomically.
	pub(crate) fn next_sequence(&self, current: Option<i64>) -> Result<i64, ScalarStateError> {
		self.sequence(current)?
			.checked_add(1)
			.ok_or(ScalarStateError::ArithmeticOverflow)
	}
	pub(crate) fn cursor(&self, current: Option<i64>) -> Result<i64, ScalarStateError> {
		self.sequence(current)
	}
	pub(crate) fn advance_cursor(
		&self,
		current: Option<i64>,
		requested: i64,
	) -> Result<SubscriptionCursorMutation, ScalarStateError> {
		let requested = require_nonnegative(requested).map_err(ScalarStateError::Domain)?;
		let actual = self.cursor(current)?;
		Ok(if current.is_some() && actual >= requested {
			SubscriptionCursorMutation::KeepExisting
		} else {
			SubscriptionCursorMutation::Set(requested)
		})
	}
	/// Explicit setting can lower the cursor; it does not alter the topic high-water sequence.
	pub(crate) fn set_cursor(
		&self,
		requested: i64,
	) -> Result<SubscriptionCursorMutation, ScalarStateError> {
		Ok(SubscriptionCursorMutation::Set(
			require_nonnegative(requested).map_err(ScalarStateError::Domain)?,
		))
	}
}
