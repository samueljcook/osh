//! Concrete wire shapes for consumers that inspect a type graph at runtime.

use std::collections::{BTreeMap, BTreeSet};

use super::declarations::{FieldPresence, RawTsPart};
use super::{Error, Result, TypeDef, TypeRef, TypeRegistry};

/// A concrete root and the definitions reachable from it.
///
/// Every generic application has its own definition with substituted arguments.
/// Applications with arguments that differ only in documentation share one
/// definition. The root and the definitions contain no
/// [`TypeRef::Documented`] or [`TypeRef::Parameter`] expressions, and no
/// [`TypeRef::Named`] reference in them has type arguments, although their
/// type, [`TypeRef`], can express all three, so callers can inspect wire
/// shapes directly.
/// This graph supports regular recursive types. A recursive path that revisits
/// one constructor with different arguments is rejected rather than expanded
/// without a bound. Use [`super::TypeModule`] to emit generic TS declarations.
#[derive(Clone, Debug)]
pub struct TypeGraph {
	/// Concrete root reference.
	pub root: TypeRef,
	/// Concrete definitions, ordered by identity key.
	pub type_defs: Vec<TypeDef>,
}

impl TypeGraph {
	/// Resolve a concrete root using the collected definitions.
	pub fn new(root: TypeRef, registry: TypeRegistry) -> Result<Self> {
		let mut resolver = GraphResolver {
			originals: registry
				.into_defs()
				.into_iter()
				.map(|def| (def.key().to_owned(), def))
				.collect(),
			applications: BTreeMap::new(),
			active_constructors: BTreeSet::new(),
		};
		let root = resolver.resolve(&root)?;
		Ok(Self {
			root,
			type_defs: resolver
				.applications
				.into_values()
				.map(|application| match application {
					ApplicationResolution::Resolved(definition) => definition,
					ApplicationResolution::Resolving => {
						unreachable!("a successful graph has no unresolved applications")
					}
				})
				.collect(),
		})
	}
}

struct GraphResolver {
	originals: BTreeMap<String, TypeDef>,
	applications: BTreeMap<String, ApplicationResolution>,
	// The constructor keys of the Resolving applications, one per constructor.
	active_constructors: BTreeSet<String>,
}

enum ApplicationResolution {
	Resolving,
	Resolved(TypeDef),
}

impl GraphResolver {
	fn resolve(&mut self, reference: &TypeRef) -> Result<TypeRef> {
		match reference {
			TypeRef::Named {
				key,
				name,
				type_args,
			} => {
				let def = self
					.originals
					.get(key)
					.ok_or_else(|| Error::new(format!("missing definition {key}")))?
					.clone();
				if name != def.name() {
					return Err(Error::new(format!(
						"reference {name} does not match definition {}",
						def.name()
					)));
				}
				let type_args: Vec<TypeRef> = type_args.iter().map(remove_docs).collect();
				// Resolve arguments before their enclosing application so that a finite
				// nested application, such as Page<Page<T>>, has its own definition.
				for argument in &type_args {
					self.resolve(argument)?;
				}
				let identity = if type_args.is_empty() {
					key.clone()
				} else {
					let mut identity = format!("{key}<");
					write_identity_list(&type_args, &mut identity);
					identity.push('>');
					identity
				};
				let resolved = TypeRef::named_with_key(&identity, name);
				if self.applications.contains_key(&identity) {
					return Ok(resolved);
				}
				if self.active_constructors.contains(key) {
					return Err(Error::new(format!(
						"recursive type {name} changes its arguments; concrete graph expansion does not support changing arguments on a recursive path"
					)));
				}
				let body = def.instantiate(&type_args)?;
				self.applications
					.insert(identity.clone(), ApplicationResolution::Resolving);
				self.active_constructors.insert(key.clone());
				let result = self.resolve(&body);
				self.active_constructors.remove(key);
				match result {
					Ok(body) => {
						self.applications.insert(
							identity.clone(),
							ApplicationResolution::Resolved(TypeDef::new(identity, name, body)),
						);
						Ok(resolved)
					}
					Err(error) => {
						self.applications.remove(&identity);
						Err(error)
					}
				}
			}
			TypeRef::Documented { type_ref, .. } => self.resolve(type_ref),
			TypeRef::Parameter(name) => Err(Error::new(format!("unbound type parameter {name}"))),
			_ => reference.try_map_children(&mut |nested| self.resolve(nested)),
		}
	}
}

/// Returns `reference` without documentation wrappers at any depth.
fn remove_docs(reference: &TypeRef) -> TypeRef {
	reference.map_type_refs(&mut |nested| match nested {
		TypeRef::Documented { type_ref, .. } => Some(remove_docs(type_ref)),
		_ => None,
	})
}

/*
The identity encoding writes each expression as a one-character tag followed by
its contents. A string is written as its byte length, `:`, and its text, and a
list is written as its length, `:`, and its items, so two expressions have the
same encoding only when they differ in nothing but documentation. Field and
expression documentation is not part of an identity because it does not change
the wire shape.
*/

fn write_identity(reference: &TypeRef, identity: &mut String) {
	match reference {
		TypeRef::Undefined => identity.push('u'),
		TypeRef::Null => identity.push('n'),
		TypeRef::Unknown => identity.push('k'),
		TypeRef::Bool => identity.push('b'),
		TypeRef::String => identity.push('s'),
		TypeRef::Number => identity.push('f'),
		TypeRef::Integer => identity.push('i'),
		TypeRef::WideInteger { rust_type } => {
			identity.push('w');
			write_identity_text(rust_type, identity);
		}
		TypeRef::Named {
			key,
			name,
			type_args,
		} => {
			identity.push('N');
			write_identity_text(key, identity);
			write_identity_text(name, identity);
			write_identity_list(type_args, identity);
		}
		TypeRef::Parameter(name) => {
			identity.push('P');
			write_identity_text(name, identity);
		}
		TypeRef::Documented { type_ref, .. } => write_identity(type_ref, identity),
		TypeRef::Array(item) => {
			identity.push('A');
			write_identity(item, identity);
		}
		TypeRef::Record(key, value) => {
			identity.push('R');
			write_identity(key, identity);
			write_identity(value, identity);
		}
		TypeRef::FormData => identity.push('F'),
		TypeRef::Blob => identity.push('B'),
		TypeRef::Nullable(inner) => {
			identity.push('?');
			write_identity(inner, identity);
		}
		TypeRef::Union(members) => {
			identity.push('U');
			write_identity_list(members, identity);
		}
		TypeRef::Intersection(members) => {
			identity.push('I');
			write_identity_list(members, identity);
		}
		TypeRef::Object(fields) => {
			identity.push('O');
			write_identity_count(fields.len(), identity);
			for field in fields {
				write_identity_text(field.name(), identity);
				identity.push(match field.presence() {
					FieldPresence::Required => 'r',
					FieldPresence::Optional => 'o',
				});
				write_identity(field.type_ref(), identity);
			}
		}
		TypeRef::Tuple(items) => {
			identity.push('T');
			write_identity_list(items, identity);
		}
		TypeRef::StringLiteral(value) => {
			identity.push('L');
			write_identity_text(value, identity);
		}
		TypeRef::Raw(parts) => {
			identity.push('W');
			write_identity_count(parts.len(), identity);
			for part in parts {
				match part {
					RawTsPart::Text(text) => {
						identity.push('t');
						write_identity_text(text, identity);
					}
					RawTsPart::TypeRef(nested) => {
						identity.push('e');
						write_identity(nested, identity);
					}
				}
			}
		}
	}
}

fn write_identity_list(references: &[TypeRef], identity: &mut String) {
	write_identity_count(references.len(), identity);
	for reference in references {
		write_identity(reference, identity);
	}
}

fn write_identity_text(text: &str, identity: &mut String) {
	write_identity_count(text.len(), identity);
	identity.push_str(text);
}

fn write_identity_count(count: usize, identity: &mut String) {
	identity.push_str(&count.to_string());
	identity.push(':');
}
