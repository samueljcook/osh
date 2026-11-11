//! Complete declaration resolution and module rendering.

use std::collections::{BTreeMap, BTreeSet};

use super::declarations::{SharedTypeNameConflict, split_phase_key};
use super::render::{
	TsContractError, is_valid_type_name, render_exported_type_def, render_type_ref,
};
use super::{TypeDef, TypePhase, TypeRef, TypeRegistry};

/// How a module names serialize and deserialize definitions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PhaseNaming {
	/// Merge compatible phases under their shared name and reject divergent shapes.
	Shared,
	/// Keep each phase exact and append the supplied suffix to each phase's name.
	Separate {
		/// Suffix for deserialize definitions, such as `Input`.
		input_suffix: String,
		/// Suffix for serialize definitions, such as `Output`.
		output_suffix: String,
	},
}

/// Failure to resolve or render a complete module.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModuleError {
	/// Definitions export the same name but cannot share one declaration.
	SharedName {
		/// Conflicting export name.
		name: String,
		/// The keys of every definition that exports the name, in lexical
		/// order.
		keys: Vec<String>,
		/// Reason that the declarations cannot merge.
		reason: SharedTypeNameConflict,
	},
	/// An expression references an absent, incorrectly named, or incorrectly applied definition.
	InvalidReference {
		/// Referenced identity key.
		key: String,
		/// Referenced TypeScript name.
		name: String,
		/// Specific mismatch.
		reason: String,
	},
	/// A parameter is invalid, repeated, unbound, or shadows a referenced declaration.
	InvalidParameter {
		/// Parameter name.
		name: String,
		/// Specific mismatch.
		reason: String,
	},
	/// A definition contains an invalid TypeScript name or unsupported wire value.
	Render {
		/// Definition name, or `None` for a standalone reference.
		name: Option<String>,
		/// Rendering failure.
		source: TsContractError,
	},
}

impl std::fmt::Display for ModuleError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::SharedName { name, keys, reason } => write!(
				f,
				"conflicting TypeScript declarations for {name} (definition keys {}): {reason}",
				keys.join(", ")
			),
			Self::InvalidReference { key, name, reason } => {
				write!(f, "invalid reference {name} ({key}): {reason}")
			}
			Self::InvalidParameter { name, reason } => {
				write!(f, "invalid type parameter {name}: {reason}")
			}
			Self::Render { name, source } => match name {
				Some(name) => write!(f, "the type {name}: {source}"),
				None => source.fmt(f),
			},
		}
	}
}
impl std::error::Error for ModuleError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::Render { source, .. } => Some(source),
			_ => None,
		}
	}
}

/// A validated, deterministically ordered set of declarations in one TS namespace.
///
/// Construction checks keys, names, parameter scope, argument counts, and every
/// dependency. Declarations are ordered by export name; TypeScript permits forward
/// references and recursive declarations. Callers own file layout and writes.
#[derive(Clone, Debug)]
pub struct TypeModule {
	definitions: Vec<TypeDef>,
	originals: BTreeMap<String, TypeDef>,
	names: BTreeMap<String, String>,
	keys: BTreeMap<String, String>,
}

impl TypeModule {
	/// Resolve all collected definitions with an explicit phase naming policy.
	pub fn new(registry: TypeRegistry, phase_naming: PhaseNaming) -> Result<Self, ModuleError> {
		let originals: BTreeMap<_, _> = registry
			.into_defs()
			.into_iter()
			.map(|def| (def.key().to_owned(), def))
			.collect();
		let mut names = BTreeMap::new();
		let mut groups = BTreeMap::<String, Vec<TypeDef>>::new();
		for def in originals.values() {
			let name = match (&phase_naming, split_phase_key(def.key())) {
				(
					PhaseNaming::Separate {
						input_suffix,
						output_suffix,
					},
					Some((_, phase)),
				) => {
					format!(
						"{}{}",
						def.name(),
						match phase {
							TypePhase::Serialize => output_suffix,
							TypePhase::Deserialize => input_suffix,
						}
					)
				}
				_ => def.name().to_owned(),
			};
			names.insert(def.key().to_owned(), name.clone());
			groups.entry(name).or_default().push(def.clone());
		}
		let mut module = Self {
			definitions: Vec::new(),
			originals,
			names,
			keys: BTreeMap::new(),
		};
		for def in module.originals.values() {
			let mut parameters = BTreeSet::new();
			let references =
				rename_references(def.type_ref(), &module.names, &module.keys).referenced_names();
			for param in def.type_params() {
				if !is_valid_type_name(param)
					|| !parameters.insert(param.clone())
					|| references.contains(param)
				{
					return Err(ModuleError::InvalidParameter { name: param.clone(), reason: "parameter names must be valid, unique, and distinct from referenced declarations".to_owned() });
				}
			}
			module.validate_reference(def.type_ref(), &parameters)?;
		}
		for (name, defs) in groups {
			let original_keys: Vec<_> = defs.iter().map(|def| def.key().to_owned()).collect();
			let mut defs = defs.into_iter();
			let first = defs.next().expect("each group contains a definition");
			let def = defs.try_fold(first, |merged, other| {
				if phase_naming != PhaseNaming::Shared {
					return Err(ModuleError::SharedName {
						name: name.clone(),
						keys: original_keys.clone(),
						reason: SharedTypeNameConflict::UnrelatedTypes,
					});
				}
				merged
					.merge_shared_name_with(&other)
					.map_err(|reason| ModuleError::SharedName {
						name: name.clone(),
						keys: original_keys.clone(),
						reason,
					})
			})?;
			for key in original_keys {
				module.keys.insert(key, def.key().to_owned());
			}
			let mut resolved = TypeDef::new(def.key(), &name, def.type_ref().clone())
				.set_type_params(def.type_params().to_vec());
			if let Some(docs) = def.docs() {
				resolved = resolved.set_docs(docs);
			}
			let mut checked = String::new();
			render_exported_type_def(&resolved, &mut checked).map_err(|source| {
				ModuleError::Render {
					name: Some(name),
					source,
				}
			})?;
			module.definitions.push(resolved);
		}
		for def in &mut module.definitions {
			*def = def.with_type_ref(rename_references(
				def.type_ref(),
				&module.names,
				&module.keys,
			));
		}
		Ok(module)
	}

	/// Return resolved declarations in lexical export-name order.
	pub fn type_defs(&self) -> &[TypeDef] {
		&self.definitions
	}

	/// Validate a collected type's reference and apply this module's phase names.
	pub fn resolve_type_ref(&self, reference: &TypeRef) -> Result<TypeRef, ModuleError> {
		self.validate_reference(reference, &BTreeSet::new())?;
		Ok(rename_references(reference, &self.names, &self.keys))
	}

	/// Render a collected type's reference with this module's phase names.
	pub fn render_type_ref(&self, reference: &TypeRef) -> Result<String, ModuleError> {
		render_type_ref(&self.resolve_type_ref(reference)?)
			.map_err(|source| ModuleError::Render { name: None, source })
	}

	/// Render every definition as an exported declaration, preserving documentation.
	pub fn render(&self) -> Result<String, ModuleError> {
		let mut source = String::new();
		for def in &self.definitions {
			render_exported_type_def(def, &mut source).map_err(|source| ModuleError::Render {
				name: Some(def.name().to_owned()),
				source,
			})?;
		}
		if source.is_empty() {
			source.push_str("export {};\n");
		}
		Ok(source)
	}

	fn validate_reference(
		&self,
		reference: &TypeRef,
		params: &BTreeSet<String>,
	) -> Result<(), ModuleError> {
		let mut result = Ok(());
		reference.visit(&mut |reference| {
			if result.is_err() {
				return;
			}
			match reference {
				TypeRef::Named {
					key,
					name,
					type_args,
				} => {
					let reason = match self.originals.get(key) {
						None => Some("no definition has this key".to_owned()),
						Some(def) if def.name() != name => {
							Some(format!("the definition is named {}", def.name()))
						}
						Some(def) if def.type_params().len() != type_args.len() => Some(format!(
							"expected {} arguments, received {}",
							def.type_params().len(),
							type_args.len()
						)),
						_ => None,
					};
					if let Some(reason) = reason {
						result = Err(ModuleError::InvalidReference {
							key: key.clone(),
							name: name.clone(),
							reason,
						});
					}
				}
				TypeRef::Parameter(name) if !params.contains(name) => {
					result = Err(ModuleError::InvalidParameter {
						name: name.clone(),
						reason: "no enclosing declaration binds this parameter".to_owned(),
					});
				}
				_ => {}
			}
		});
		result
	}
}

fn rename_references(
	reference: &TypeRef,
	names: &BTreeMap<String, String>,
	keys: &BTreeMap<String, String>,
) -> TypeRef {
	reference.map_type_refs(&mut |reference| match reference {
		TypeRef::Named {
			key,
			name,
			type_args,
		} => Some(TypeRef::generic(
			keys.get(key).unwrap_or(key),
			names.get(key).unwrap_or(name),
			type_args
				.iter()
				.map(|arg| rename_references(arg, names, keys))
				.collect(),
		)),
		_ => None,
	})
}
