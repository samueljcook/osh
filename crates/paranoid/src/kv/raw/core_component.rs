//! The core Raw component: the installation's own Raw records, which hold the key-value items
//! that a v10 installation moved into the native installation.

use crate::installation::contract::{
	ComponentDeclaration, ComponentDeclarationFields, ComponentPolicy, ComponentPolicyFields,
	ComponentSettings, ComponentSettingsDeclaration, ComponentSettingsRequirement,
	SemanticContractDef, StateContractError,
};

use super::engine::store::RawKvComponent;

const SEMANTICS: &str = "paranoid.core-raw.v1";

pub(crate) struct CoreRawComponent {
	raw: RawKvComponent,
}
impl CoreRawComponent {
	pub(crate) fn new() -> Self {
		Self {
			raw: RawKvComponent::new(),
		}
	}
	pub(crate) fn declaration(&self) -> ComponentDeclaration<'_, RawKvComponent> {
		ComponentDeclaration::new(ComponentDeclarationFields {
			identifier: self.raw.declaration().identifier(),
			fixed_schemas: Vec::new(),
			settings: ComponentSettingsDeclaration::Parameterless,
			semantics: vec![SemanticContractDef {
				identifier: SEMANTICS,
				transitions: &self.raw,
				settings: ComponentSettingsRequirement::Parameterless,
			}],
		})
		.expect("core Raw declares its byte transitions without settings")
	}
	pub(crate) fn initial_policy(&self) -> Result<ComponentPolicy, StateContractError> {
		let declaration = self.declaration();
		let policy = ComponentPolicy::new(ComponentPolicyFields {
			component_identifier: declaration.identifier().to_owned(),
			semantic_contract: SEMANTICS.to_owned(),
			schemas: Vec::new(),
			settings: ComponentSettings::Parameterless,
		})?;
		declaration.bind(&policy)?;
		Ok(policy)
	}
}
