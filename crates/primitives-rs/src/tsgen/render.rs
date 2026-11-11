//! TypeScript declaration rendering.
//!
//! [`render_local_type_def`] renders one [`TypeDef`] as a module-local
//! TypeScript type declaration, preceded by a JSDoc comment when the
//! definition has documentation text. [`render_type_ref`] renders one
//! [`TypeRef`] as a TypeScript type expression. An object type renders with
//! one property per line, indented with one tab per nesting level, and a
//! documented property is preceded by its JSDoc comment.

use crate::tsgen::declarations::{FieldDef, FieldPresence, RawTsPart, TypeDef, TypeRef};

/// Name of the platform `FormData` type in generated TypeScript.
///
/// [`TypeRef::FormData`] renders as this name, which refers to the built-in
/// `FormData` type. A declared type cannot use this name.
pub const FORM_DATA_TYPE_NAME: &str = "FormData";
/// Name of the platform `Blob` type in generated TypeScript.
///
/// [`TypeRef::Blob`] renders as this name, which refers to the built-in
/// `Blob` type. A declared type cannot use this name.
pub const BLOB_TYPE_NAME: &str = "Blob";

/// Error from rendering TypeScript declarations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TsContractError {
	/// A type definition name was invalid.
	///
	/// A declared name must be a TypeScript identifier other than a reserved
	/// word, `Array`, `Record`, `Blob`, or `FormData`. Rendered references use
	/// those four names for the platform types, so a declaration with one of
	/// them would change their meaning. The `TsGen` derive uses a type's Rust
	/// identifier as its TypeScript name, so a derived Rust type with one of
	/// these names must be renamed.
	InvalidTypeName {
		/// Rejected type name.
		name: String,
	},
	/// A named type reference name was invalid.
	InvalidTypeReferenceName {
		/// Rejected type reference name.
		name: String,
	},
	/// A Rust integer domain cannot be rendered losslessly as a JS number.
	IntegerRangeExceedsJsPrecision {
		/// Rejected Rust integer primitive.
		rust_type: String,
		/// The path of property names, joined with `.`, from the rendered
		/// type to the rejected integer. `None` when the rendered type is the
		/// integer itself.
		field: Option<String>,
	},
}

impl std::fmt::Display for TsContractError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::InvalidTypeName { name } => write!(f, "invalid TypeScript type name {name:?}"),
			Self::InvalidTypeReferenceName { name } => {
				write!(f, "invalid TypeScript type reference name {name:?}")
			}
			Self::IntegerRangeExceedsJsPrecision { rust_type, field } => {
				match field {
					Some(field) => write!(
						f,
						"the field `{field}` has the Rust integer type {rust_type}"
					)?,
					None => write!(f, "the Rust integer type {rust_type} appears")?,
				}
				write!(
					f,
					", which JavaScript would round because it reads JSON numbers as doubles. Use IntegerString<{rust_type}>, which crosses the wire as decimal text, or a 32-bit integer type (i32 or u32) when the range allows"
				)
			}
		}
	}
}

impl std::error::Error for TsContractError {}

/// Render one type definition as a module-local TypeScript declaration,
/// `type {name} = ...;`, and append it to `source`.
///
/// The output starts with a JSDoc comment when the definition has
/// documentation text and always ends with one newline. On error, `source`
/// can end with part of the declaration.
pub fn render_local_type_def(
	type_def: &TypeDef,
	source: &mut String,
) -> Result<(), TsContractError> {
	render_type_def(type_def, false, source)
}

/// Render one exported declaration, preserving its JSDoc and generic parameters.
///
/// On error, `source` can end with part of the declaration.
pub fn render_exported_type_def(
	type_def: &TypeDef,
	source: &mut String,
) -> Result<(), TsContractError> {
	render_type_def(type_def, true, source)
}

fn render_type_def(
	type_def: &TypeDef,
	exported: bool,
	source: &mut String,
) -> Result<(), TsContractError> {
	render_jsdoc(type_def.docs(), 0, source);
	if exported {
		source.push_str("export ");
	}
	let name = type_def.name();
	validate_type_name(name)?;
	source.push_str("type ");
	source.push_str(name);
	if !type_def.type_params().is_empty() {
		source.push('<');
		for (index, param) in type_def.type_params().iter().enumerate() {
			validate_type_name(param)?;
			if index > 0 {
				source.push_str(", ");
			}
			source.push_str(param);
		}
		source.push('>');
	}
	source.push_str(" = ");
	write_type_ref(type_def.type_ref(), 0, source)?;
	source.push_str(";\n");
	Ok(())
}

/// Render a type reference as a TypeScript type expression.
pub fn render_type_ref(type_ref: &TypeRef) -> Result<String, TsContractError> {
	let mut source = String::new();
	write_type_ref(type_ref, 0, &mut source)?;
	Ok(source)
}

/// Appends `type_ref` to `source`. `depth` is the object nesting level of
/// the position where the expression starts, which sets the indentation of
/// an object's properties and closing brace.
fn write_type_ref(
	type_ref: &TypeRef,
	depth: usize,
	source: &mut String,
) -> Result<(), TsContractError> {
	match type_ref {
		TypeRef::Undefined => source.push_str("undefined"),
		TypeRef::Null => source.push_str("null"),
		TypeRef::Unknown => source.push_str("unknown"),
		TypeRef::Bool => source.push_str("boolean"),
		TypeRef::String => source.push_str("string"),
		TypeRef::Number | TypeRef::Integer => source.push_str("number"),
		TypeRef::WideInteger { rust_type } => {
			return Err(TsContractError::IntegerRangeExceedsJsPrecision {
				rust_type: rust_type.clone(),
				field: None,
			});
		}
		TypeRef::Named {
			name, type_args, ..
		} => {
			if !is_valid_type_name(name) {
				return Err(TsContractError::InvalidTypeReferenceName { name: name.clone() });
			}
			source.push_str(name);
			if !type_args.is_empty() {
				source.push('<');
				for (index, arg) in type_args.iter().enumerate() {
					if index > 0 {
						source.push_str(", ");
					}
					write_type_ref(arg, depth, source)?;
				}
				source.push('>');
			}
		}
		TypeRef::Parameter(name) => {
			validate_type_name(name)?;
			source.push_str(name);
		}
		TypeRef::Documented { docs, type_ref } => {
			source.push_str("(\n");
			render_jsdoc(Some(docs), depth + 1, source);
			push_indent(depth + 1, source);
			write_type_ref(type_ref, depth + 1, source)?;
			source.push('\n');
			push_indent(depth, source);
			source.push(')');
		}
		TypeRef::Array(element) => {
			source.push_str("Array<");
			write_type_ref(element, depth, source)?;
			source.push('>');
		}
		TypeRef::Record(key, value) => {
			source.push_str("Record<");
			write_type_ref(key, depth, source)?;
			source.push_str(", ");
			write_type_ref(value, depth, source)?;
			source.push('>');
		}
		TypeRef::FormData => source.push_str(FORM_DATA_TYPE_NAME),
		TypeRef::Blob => source.push_str(BLOB_TYPE_NAME),
		TypeRef::Nullable(inner) => {
			write_operand(
				inner,
				matches!(inner.as_ref(), TypeRef::Raw(_)),
				depth,
				source,
			)?;
			source.push_str(" | null");
		}
		TypeRef::Union(members) => {
			if members.is_empty() {
				source.push_str("never");
			}
			for (index, member) in members.iter().enumerate() {
				if index > 0 {
					source.push_str(" | ");
				}
				write_operand(member, matches!(member, TypeRef::Raw(_)), depth, source)?;
			}
		}
		TypeRef::Intersection(members) => {
			if members.is_empty() {
				source.push_str("unknown");
			}
			for (index, member) in members.iter().enumerate() {
				if index > 0 {
					source.push_str(" & ");
				}
				// `&` binds more tightly than `|`, and raw text can contain
				// either operator.
				let parenthesize = matches!(member, TypeRef::Nullable(_) | TypeRef::Raw(_))
					|| matches!(member, TypeRef::Union(members) if members.len() > 1);
				write_operand(member, parenthesize, depth, source)?;
			}
		}
		TypeRef::Object(fields) => write_object(fields, depth, source)?,
		TypeRef::Tuple(items) => {
			source.push('[');
			for (index, item) in items.iter().enumerate() {
				if index > 0 {
					source.push_str(", ");
				}
				write_type_ref(item, depth, source)?;
			}
			source.push(']');
		}
		TypeRef::StringLiteral(value) => source.push_str(&quoted(value)),
		TypeRef::Raw(parts) => {
			for part in parts {
				match part {
					RawTsPart::Text(text) => source.push_str(text),
					RawTsPart::TypeRef(type_ref) => write_type_ref(type_ref, depth, source)?,
				}
			}
		}
	}
	Ok(())
}

/// Groups expressions when an enclosing operator could change their meaning.
fn write_operand(
	type_ref: &TypeRef,
	parenthesize: bool,
	depth: usize,
	source: &mut String,
) -> Result<(), TsContractError> {
	if parenthesize {
		source.push('(');
	}
	write_type_ref(type_ref, depth, source)?;
	if parenthesize {
		source.push(')');
	}
	Ok(())
}

fn write_object(
	fields: &[FieldDef],
	depth: usize,
	source: &mut String,
) -> Result<(), TsContractError> {
	if fields.is_empty() {
		source.push_str("Record<never, never>");
		return Ok(());
	}
	source.push_str("{\n");
	for field in fields {
		render_jsdoc(field.docs(), depth + 1, source);
		push_indent(depth + 1, source);
		source.push_str(&render_property_name(field.name()));
		if field.presence() == FieldPresence::Optional {
			source.push('?');
		}
		source.push_str(": ");
		write_type_ref(field.type_ref(), depth + 1, source).map_err(|error| match error {
			TsContractError::IntegerRangeExceedsJsPrecision {
				rust_type,
				field: inner,
			} => TsContractError::IntegerRangeExceedsJsPrecision {
				rust_type,
				field: Some(match inner {
					Some(inner) => format!("{}.{inner}", field.name()),
					None => field.name().to_owned(),
				}),
			},
			other => other,
		})?;
		source.push_str(";\n");
	}
	push_indent(depth, source);
	source.push('}');
	Ok(())
}

fn push_indent(depth: usize, source: &mut String) {
	for _ in 0..depth {
		source.push('\t');
	}
}

/// Emit a jsdoc block for `docs` when present, indented to `depth`.
///
/// Single-line docs that fit use `/** Text. */`; multi-line docs use a block.
/// Any `*/` sequence inside the text is escaped so it cannot terminate the
/// comment. Undocumented items emit nothing.
fn render_jsdoc(docs: Option<&str>, depth: usize, source: &mut String) {
	let Some(docs) = docs else {
		return;
	};
	let escaped = docs.replace("*/", "*\\/");
	let lines: Vec<&str> = escaped.lines().collect();
	if lines.len() == 1 && !lines[0].is_empty() && lines[0].len() <= 80 {
		push_indent(depth, source);
		source.push_str("/** ");
		source.push_str(lines[0]);
		source.push_str(" */\n");
		return;
	}
	push_indent(depth, source);
	source.push_str("/**\n");
	for line in lines {
		push_indent(depth, source);
		source.push_str(" *");
		if !line.is_empty() {
			source.push(' ');
			source.push_str(line);
		}
		source.push('\n');
	}
	push_indent(depth, source);
	source.push_str(" */\n");
}

fn render_property_name(name: &str) -> String {
	if is_valid_property_identifier(name) {
		return name.to_owned();
	}
	quoted(name)
}

fn validate_type_name(name: &str) -> Result<(), TsContractError> {
	if !is_valid_type_name(name) {
		return Err(TsContractError::InvalidTypeName {
			name: name.to_owned(),
		});
	}
	Ok(())
}

/// Reports whether a declared type name is usable without changing the
/// meaning of a reserved word or of a global type that rendered references use.
pub(crate) fn is_valid_type_name(name: &str) -> bool {
	is_valid_type_identifier(name)
		&& !super::is_ts_identifier_reserved(name)
		&& !matches!(
			name,
			"Array" | "Record" | FORM_DATA_TYPE_NAME | BLOB_TYPE_NAME
		)
}

fn is_valid_type_identifier(name: &str) -> bool {
	let mut chars = name.chars();
	match chars.next() {
		Some(ch) if ch == '_' || ch == '$' || ch.is_ascii_alphabetic() => {}
		_ => return false,
	}
	chars.all(|ch| ch == '_' || ch == '$' || ch.is_ascii_alphanumeric())
}

fn is_valid_property_identifier(name: &str) -> bool {
	is_valid_type_identifier(name)
}

fn quoted(value: &str) -> String {
	serde_json::to_string(value).expect("serializing a string literal should not fail")
}
