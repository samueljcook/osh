//! The declaration contract: what the `vorma` build reads from the server
//! binary to learn its declarations. When the build runs the
//! binary with [`CONTRACT_FILE_ENV_VAR`] set, [`crate::App::load`]
//! writes the contract to that file and ends the process before reading any
//! build output.
//!
//! The declarations hash identifies the declarations that the contract
//! describes. The build records it in the build directory, and
//! [`crate::App::load`] compares it with the hash of its own declarations.

use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;

use primitives_rs::tsgen::declarations::SharedTypeNameConflict;
use primitives_rs::tsgen::render::{render_local_type_def, render_type_ref};
use primitives_rs::tsgen::{ModuleError, PhaseNaming, TypeModule, TypeRef, TypeRegistry};
use serde::Serialize;

use crate::declarations::{ApiEndpointInputKind, ApiEndpointKind, HandlerTypeRefs};
use crate::{plan::Declarations, search::SearchShape};

/// The file to which the server binary writes its declaration contract.
pub(crate) const CONTRACT_FILE_ENV_VAR: &str = "__VORMA_DECLARATION_CONTRACT_FILE";

/// The contract's format version, which the build checks.
const CONTRACT_FORMAT_VERSION: u32 = 1;

/// One TypeScript type expression and the named declarations that it
/// references directly.
#[derive(Serialize)]
struct TypeText {
	text: String,
	references: BTreeSet<String>,
}

/// One handler's TypeScript types. `null` means that the handler has no
/// input, no output, or no declared error.
#[derive(Serialize)]
struct TypesContract {
	input: Option<TypeText>,
	output: Option<TypeText>,
	error: Option<TypeText>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ViewLoaderContract<'a> {
	/// Where and how the application declared the loader, for messages.
	location: String,
	pattern: &'a str,
	search_shape: Option<SearchShape>,
	types: TypesContract,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ApiEndpointContract<'a> {
	/// Where and how the application declared the endpoint, for messages.
	location: String,
	/// `query` or `mutation`.
	api_endpoint_kind: &'static str,
	method: &'a str,
	pattern: &'a str,
	api_endpoint_input_kind: ApiEndpointInputKind,
	search_shape: Option<SearchShape>,
	types: TypesContract,
}

/// One named TypeScript declaration, a complete module-local `type`
/// statement, and the declarations that it references directly.
#[derive(Serialize)]
struct TypeDeclaration {
	name: String,
	declaration: String,
	references: BTreeSet<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeclarationContract<'a> {
	format_version: u32,
	/// Named TypeScript declarations that the handler types reference, in
	/// name order.
	type_declarations: Vec<TypeDeclaration>,
	view_loaders: Vec<ViewLoaderContract<'a>>,
	api_endpoints: Vec<ApiEndpointContract<'a>>,
}

/// The contract as the build reads it: the declarations and their hash.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ContractFile<'contract, 'declarations> {
	/// BLAKE3, in lowercase hexadecimal, of the contract without its
	/// `location` fields. Loaders are sorted by pattern and endpoints by
	/// method and pattern, so moving or reordering declarations in the source
	/// does not change it.
	declarations_hash: &'contract str,
	#[serde(flatten)]
	contract: &'contract DeclarationContract<'declarations>,
}

/// The declarations as the build reads them.
pub(crate) struct DeclarationDescription {
	/// The contract file's JSON text.
	pub(crate) contract_json: String,
	/// The declarations hash that the contract carries.
	pub(crate) declarations_hash: String,
}

/// The hash of the contract's canonical form: its JSON without the
/// `location` fields, which hold source locations.
fn compute_declarations_hash(contract: &DeclarationContract<'_>) -> String {
	let mut canonical =
		serde_json::to_value(contract).expect("the declaration contract serializes");
	for list in ["viewLoaders", "apiEndpoints"] {
		let declarations = canonical[list]
			.as_array_mut()
			.expect("the contract lists its declarations");
		for declaration in declarations {
			declaration
				.as_object_mut()
				.expect("a declaration is an object")
				.remove("location");
		}
	}
	let text = serde_json::to_vec(&canonical).expect("the canonical contract serializes");
	blake3::hash(&text).to_hex().to_string()
}

/// The declarations cannot form a contract.
#[derive(Debug)]
pub(crate) struct ContractError {
	problems: Vec<String>,
}

impl ContractError {
	pub(crate) fn problems(&self) -> &[String] {
		&self.problems
	}
}

impl fmt::Display for ContractError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("the Vorma app's declarations cannot be described to the build:")?;
		for problem in &self.problems {
			write!(formatter, "\n- {problem}")?;
		}
		Ok(())
	}
}

impl std::error::Error for ContractError {}

struct TypeCollector {
	registry: TypeRegistry,
	roots: Vec<TypeRef>,
	problems: Vec<String>,
}

impl TypeCollector {
	/// One handler's types, rendered against the named declarations that the
	/// collector accumulates.
	fn collect(
		&mut self,
		location: &str,
		collect_types_fn: fn(&mut TypeRegistry) -> primitives_rs::tsgen::Result<HandlerTypeRefs>,
	) -> TypesContract {
		let refs = match collect_types_fn(&mut self.registry) {
			Ok(refs) => refs,
			Err(error) => {
				self.problems.push(format!("{location}: {error}"));
				return TypesContract {
					input: None,
					output: None,
					error: None,
				};
			}
		};
		let mut render = |type_ref: Option<TypeRef>, role: &str| {
			let type_ref = type_ref?;
			self.roots.push(type_ref.clone());
			match render_type_ref(&type_ref) {
				Ok(text) => {
					let references = type_ref.referenced_names();
					Some(TypeText { text, references })
				}
				Err(error) => {
					self.problems
						.push(format!("{location}: its {role} type {error}"));
					None
				}
			}
		};
		// `NoDeclaredError` renders as `never`, which declares no error.
		let error = match refs.errors {
			TypeRef::Union(members) if members.is_empty() => None,
			errors => Some(errors),
		};
		TypesContract {
			input: render(refs.input, "input"),
			output: render(refs.output, "output"),
			error: render(error, "declared error"),
		}
	}

	/// One declaration for each exported name. The two phases of one Rust
	/// type share a declaration when one definition describes both; any
	/// other shared name is a problem that names its fix.
	fn into_declarations(mut self) -> Result<Vec<TypeDeclaration>, ContractError> {
		let mut declarations = Vec::new();
		match TypeModule::new(self.registry, PhaseNaming::Shared) {
			Ok(module) => {
				for root in &self.roots {
					if let Err(error) = module.resolve_type_ref(root) {
						self.problems.push(error.to_string());
					}
				}
				for def in module.type_defs() {
					let mut declaration = String::new();
					match render_local_type_def(def, &mut declaration) {
						Ok(()) => declarations.push(TypeDeclaration {
							name: def.name().to_owned(),
							references: def.type_ref().referenced_names(),
							declaration,
						}),
						Err(error) => self
							.problems
							.push(format!("the type {}: {error}", def.name())),
					}
				}
			}
			Err(ModuleError::SharedName { name, keys, reason }) => {
				self.problems.push(describe_conflict(&name, &keys, reason))
			}
			Err(error) => self.problems.push(error.to_string()),
		}
		if self.problems.is_empty() {
			Ok(declarations)
		} else {
			Err(ContractError {
				problems: self.problems,
			})
		}
	}
}

/// The problem, with its fix, of the definitions that export `name`. `keys`
/// are the definitions' keys. A derived type's key is its module path, its
/// name, and its phase, so the keys name the types' modules.
fn describe_conflict(name: &str, keys: &[String], conflict: SharedTypeNameConflict) -> String {
	let keys = keys.join(", ");
	match conflict {
		SharedTypeNameConflict::UnrelatedTypes => format!(
			"more than one Rust type in the contract is named {name} (definition keys {keys}); rename one of them"
		),
		SharedTypeNameConflict::FieldOptionalOnlyInSerializePhase { field_name } => format!(
			"the Rust type {name} (definition keys {keys}) can omit its field {field_name} as output but requires it as input; add #[serde(default)] to the field"
		),
		SharedTypeNameConflict::DivergentPhaseShape { member_name } => {
			let location = match member_name {
				Some(member_name) => format!(" at its field {member_name}"),
				None => String::new(),
			};
			format!(
				"the Rust type {name} (definition keys {keys}) has a different shape as input than as output{location}; declare separate input and output types"
			)
		}
	}
}

/// Describes the declarations as the build reads them, with their hash. The
/// declaration read and [`crate::App::load`] both call it, so the hash that
/// the build records and the hash that `load` compares come from one
/// computation.
pub(crate) fn describe<State>(
	declarations: &Declarations<State>,
) -> Result<DeclarationDescription, ContractError> {
	let mut collector = TypeCollector {
		registry: TypeRegistry::default(),
		roots: Vec::new(),
		problems: Vec::new(),
	};
	let mut view_loaders = Vec::with_capacity(declarations.view_loaders.len());
	for loader in &declarations.view_loaders {
		let location = loader.describe();
		let search_shape = (loader.derive_search_shape_fn)().unwrap_or_else(|error| {
			collector
				.problems
				.push(format!("{location}: its search input {error}"));
			None
		});
		let types = collector.collect(&location, loader.collect_types_fn);
		view_loaders.push(ViewLoaderContract {
			location,
			pattern: loader.pattern,
			search_shape,
			types,
		});
	}
	let mut api_endpoints = Vec::with_capacity(declarations.api_endpoints.len());
	for endpoint in &declarations.api_endpoints {
		let location = endpoint.describe();
		let input_kind = match (endpoint.determine_input_kind_fn)(&endpoint.method) {
			Ok(input_kind) => input_kind,
			Err(problem) => {
				collector.problems.push(format!("{location}: {problem}"));
				continue;
			}
		};
		let search_shape = if input_kind == ApiEndpointInputKind::Search {
			(endpoint.derive_search_shape_fn)()
				.map_err(|error| {
					collector
						.problems
						.push(format!("{location}: its search input {error}"))
				})
				.ok()
		} else {
			None
		};
		let types = collector.collect(&location, endpoint.collect_types_fn);
		api_endpoints.push(ApiEndpointContract {
			location,
			api_endpoint_kind: match endpoint.kind {
				ApiEndpointKind::Query => "query",
				ApiEndpointKind::Mutation => "mutation",
			},
			method: endpoint.method.as_str(),
			pattern: endpoint.pattern,
			api_endpoint_input_kind: input_kind,
			search_shape,
			types,
		});
	}
	view_loaders.sort_by(|left, right| left.pattern.cmp(right.pattern));
	api_endpoints
		.sort_by(|left, right| (left.method, left.pattern).cmp(&(right.method, right.pattern)));
	let type_declarations = collector.into_declarations()?;
	let contract = DeclarationContract {
		format_version: CONTRACT_FORMAT_VERSION,
		type_declarations,
		view_loaders,
		api_endpoints,
	};
	let declarations_hash = compute_declarations_hash(&contract);
	let contract_json = serde_json::to_string(&ContractFile {
		declarations_hash: &declarations_hash,
		contract: &contract,
	})
	.expect("the declaration contract serializes");
	Ok(DeclarationDescription {
		contract_json,
		declarations_hash,
	})
}

/// Writes the contract and ends the process when the build asked for it. A
/// declaration that cannot be described ends the process with a message on
/// standard error.
pub(crate) fn write_contract_if_requested(
	description: &Result<DeclarationDescription, ContractError>,
) {
	let Some(file) = std::env::var_os(CONTRACT_FILE_ENV_VAR) else {
		return;
	};
	let result = description
		.as_ref()
		.map_err(ToString::to_string)
		.and_then(|description| {
			use primitives_rs::fs::{
				Dir, Durability, Permissions, RelativePath, WriteMode, WriteOptions,
			};
			let path = Path::new(&file);
			let write = || -> Result<(), Box<dyn std::error::Error>> {
				let parent = path
					.parent()
					.filter(|path| !path.as_os_str().is_empty())
					.unwrap_or(Path::new("."));
				let name = path.file_name().ok_or_else(|| {
					std::io::Error::new(
						std::io::ErrorKind::InvalidInput,
						"contract destination must name a file",
					)
				})?;
				Dir::open(parent)?.write_file_atomically(
					&RelativePath::new(name)?,
					description.contract_json.as_bytes(),
					WriteOptions {
						permissions: Permissions::PlatformDefault,
						mode: WriteMode::IfChanged,
						durability: Durability::None,
					},
				)?;
				Ok(())
			};
			write().map_err(|error| {
				format!(
					"Vorma could not write its declaration contract to {}: {error}",
					Path::new(&file).display()
				)
			})
		});
	match result {
		Ok(()) => std::process::exit(0),
		Err(msg) => {
			eprintln!("{msg}");
			std::process::exit(1);
		}
	}
}
