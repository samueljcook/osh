//! The complete initial opaque-byte policy derives from the actual raw component.

use crate::installation::contract::{
	ComponentPolicy, ComponentPolicyFields, ComponentSettings, StateContractError,
};

use super::{RAW_KV_COMPONENT, RAW_KV_SEMANTICS, RawKvComponent};

impl RawKvComponent {
	pub(crate) fn initial_policy(&self) -> Result<ComponentPolicy, StateContractError> {
		let policy = ComponentPolicy::new(ComponentPolicyFields {
			component_identifier: RAW_KV_COMPONENT.to_owned(),
			semantic_contract: RAW_KV_SEMANTICS.to_owned(),
			schemas: Vec::new(),
			settings: ComponentSettings::Parameterless,
		})?;
		self.declaration().bind(&policy)?;
		Ok(policy)
	}
}
