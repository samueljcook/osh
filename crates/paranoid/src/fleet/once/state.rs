//! Native permanent Once completion state, without execution or private lease authority.

use serde::{Deserialize, Serialize};

use crate::fleet::scalar_codec::ValueError;
use crate::installation::contract::{
	BoundComponentContract, ComponentDeclaration, ComponentDeclarationFields, ComponentPolicy,
	ComponentPolicyFields, ComponentSettings, ComponentSettingsDeclaration,
	ComponentSettingsRequirement, SemanticContractDef,
};
use crate::records::StateTimestampMicros;
use crate::records::state_schema::StateSchema;

use crate::fleet::HolderId;
use crate::fleet::scalar_codec::{
	ScalarSchemaBinding, ScalarStateError, bind_native_scalar, initial_native_schema_policy,
	invalid_value, json_error, native_schema, parse_integer,
};

const COMPONENT: &str = "paranoid.once-completion";
const SCHEMA: &str = "paranoid.once-completion";
const SEMANTICS: &str = "paranoid.once-completion.v1";

pub(crate) struct NativeOnceCompletionFields {
	pub(crate) finished_at: StateTimestampMicros,
	pub(crate) holder: HolderId,
	pub(crate) fencing_token: i64,
}
/// Completion is diagnostic state and grants no right to renew, release, or execute.
pub(crate) struct NativeOnceCompletion {
	fields: NativeOnceCompletionFields,
}
impl NativeOnceCompletion {
	pub(crate) fn new(fields: NativeOnceCompletionFields) -> Result<Self, ValueError> {
		if fields.fencing_token <= 0 {
			return Err(invalid_value(
				"Once completion fencing token must be positive",
			));
		}
		Ok(Self { fields })
	}
	pub(crate) fn finished_at(&self) -> StateTimestampMicros {
		self.fields.finished_at
	}
	pub(crate) fn holder(&self) -> &HolderId {
		&self.fields.holder
	}
	pub(crate) fn fencing_token(&self) -> i64 {
		self.fields.fencing_token
	}
}
impl std::fmt::Debug for NativeOnceCompletion {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("NativeOnceCompletion")
			.finish_non_exhaustive()
	}
}

pub(crate) enum OnceCompletionMutation {
	KeepExisting,
	Write(NativeOnceCompletion),
}
pub(crate) struct OnceCompletionComponent {
	schema: StateSchema<NativeOnceCompletion, ValueError>,
}
impl OnceCompletionComponent {
	pub(crate) fn new() -> Self {
		Self {
			schema: native_schema(SCHEMA, decode_completion, encode_completion),
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
		.expect("Once completion declares its actual schema and transition")
	}
	pub(crate) fn bind_schema<'a>(
		&'a self,
		component: &'a BoundComponentContract<'_, '_, Self>,
	) -> Result<ScalarSchemaBinding<'a, NativeOnceCompletion>, ScalarStateError> {
		bind_native_scalar(self, component, &self.schema)
	}
	/// The executor must authenticate current private execution authority before applying this
	/// decision.
	pub(crate) fn mark_done(
		&self,
		current: Option<&NativeOnceCompletion>,
		completion: NativeOnceCompletion,
	) -> OnceCompletionMutation {
		if current.is_some() {
			OnceCompletionMutation::KeepExisting
		} else {
			OnceCompletionMutation::Write(completion)
		}
	}
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CompletionPayload {
	finished_at: String,
	holder: String,
	fencing_token: String,
}
fn decode_completion(bytes: &[u8]) -> Result<NativeOnceCompletion, ValueError> {
	let payload: CompletionPayload = serde_json::from_slice(bytes).map_err(json_error)?;
	NativeOnceCompletion::new(NativeOnceCompletionFields {
		finished_at: StateTimestampMicros::new(parse_integer(&payload.finished_at)?),
		holder: HolderId::new(&payload.holder)
			.map_err(|_| invalid_value("Once completion holder is invalid"))?,
		fencing_token: parse_integer(&payload.fencing_token)?,
	})
}
fn encode_completion(value: &NativeOnceCompletion) -> Result<Vec<u8>, ValueError> {
	serde_json::to_vec(&CompletionPayload {
		finished_at: value.finished_at().as_i64().to_string(),
		holder: value.holder().as_str().to_owned(),
		fencing_token: value.fencing_token().to_string(),
	})
	.map_err(json_error)
}
