//! Native permanent cache epoch schema and checked invalidation transition.

use crate::fleet::scalar_codec::ValueError;
use crate::installation::contract::{
	BoundComponentContract, ComponentDeclaration, ComponentDeclarationFields, ComponentPolicy,
	ComponentPolicyFields, ComponentSettings, ComponentSettingsDeclaration,
	ComponentSettingsRequirement, SemanticContractDef,
};
use crate::records::state_schema::StateSchema;

use crate::fleet::scalar_codec::{
	ScalarSchemaBinding, ScalarStateError, bind_native_scalar, decode_integer, encode_integer,
	initial_native_schema_policy, native_schema,
};

const COMPONENT: &str = "paranoid.cache-epoch";
const SCHEMA: &str = "paranoid.cache-epoch";
const SEMANTICS: &str = "paranoid.cache-epoch.v1";

pub(crate) struct CacheEpochComponent {
	schema: StateSchema<i64, ValueError>,
}
impl CacheEpochComponent {
	pub(crate) fn new() -> Self {
		Self {
			schema: native_schema(SCHEMA, decode_integer, encode_integer),
		}
	}
	/// Constructs and binds this owner's complete fixed initial policy without value IO.
	pub(crate) fn initial_policy(&self) -> Result<ComponentPolicy, ScalarStateError> {
		let policy = ComponentPolicy::new(ComponentPolicyFields {
			component_identifier: COMPONENT.to_owned(),
			semantic_contract: SEMANTICS.to_owned(),
			schemas: vec![initial_native_schema_policy(&self.schema)?],
			settings: ComponentSettings::Parameterless,
		})?;
		let declaration = self.declaration();
		let binding = declaration.bind(&policy)?;
		self.bind_schema(&binding)?;
		Ok(policy)
	}
	pub(crate) fn declaration(&self) -> ComponentDeclaration<'_, Self> {
		ComponentDeclaration::new(ComponentDeclarationFields {
			identifier: COMPONENT,
			fixed_schemas: vec![self.schema.capabilities()],
			settings: ComponentSettingsDeclaration::Parameterless,
			semantics: vec![SemanticContractDef {
				identifier: SEMANTICS,
				transitions: self,
				settings: ComponentSettingsRequirement::Parameterless,
			}],
		})
		.expect("cache epoch declares its actual schema and transition")
	}
	pub(crate) fn bind_schema<'a>(
		&'a self,
		component: &'a BoundComponentContract<'_, '_, Self>,
	) -> Result<ScalarSchemaBinding<'a, i64>, ScalarStateError> {
		bind_native_scalar(self, component, &self.schema)
	}
	pub(crate) fn epoch(&self, current: Option<i64>) -> i64 {
		current.unwrap_or(0)
	}
	/// Absence initializes at one before invalidation; every signed epoch then increments.
	pub(crate) fn next_epoch(&self, current: Option<i64>) -> Result<i64, ScalarStateError> {
		current
			.unwrap_or(1)
			.checked_add(1)
			.ok_or(ScalarStateError::ArithmeticOverflow)
	}
}
