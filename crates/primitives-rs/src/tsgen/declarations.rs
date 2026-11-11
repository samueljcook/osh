//! TypeScript declaration model: type expressions, named definitions, object
//! fields, and the merging of a type's two phases into one definition.
//!
//! A [`TypeRef`] is a TypeScript type expression. A [`TypeDef`] declares a
//! named type as `type {name} = {type expression};`, and a [`FieldDef`]
//! describes one property of an object type.
//! [`TypeDef::merge_shared_name_with`] merges two definitions that export the
//! same TypeScript name into one definition when they are the two phases of
//! one Rust type and one definition can describe both phases. The
//! [`TsGen`](crate::tsgen::TsGen) trait in [`crate::tsgen`] produces these
//! values, and [`crate::tsgen::render`] renders them as TypeScript source
//! text.

use serde::Serialize;

use crate::tsgen::TypePhase;

/// TypeScript type expression.
///
/// A `TypeRef` describes a Rust type's TypeScript shape where the type is
/// used, such as a field type or a handler's input or output type, and it is
/// the body of a named [`TypeDef`]. A [`TypeRef::Named`] reference identifies
/// a `TypeDef` by its key and name instead of describing the shape inline,
/// and a consumer that registers declarations should reject a `Named`
/// reference that does not match the key and name of a declared `TypeDef`.
/// The `TsGen` derive generates expressions for derived types. Hand-written
/// [`crate::tsgen::TsGen`] implementations can build them from the variants
/// and the constructor methods below.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub enum TypeRef {
	/// `undefined`.
	Undefined,
	/// `null`.
	Null,
	/// `unknown`.
	Unknown,
	/// `boolean`.
	Bool,
	/// `string`.
	String,
	/// `number`.
	Number,
	/// Integer-like Rust number, rendered as TypeScript `number`.
	///
	/// Use this variant only for Rust types with integer values that a
	/// JavaScript `number` represents exactly: `i8`, `i16`, `i32`, `u8`,
	/// `u16`, `u32`, their `std::num::NonZero` forms, and
	/// `primitives_rs::serialization::json::JsSafeU64` and
	/// `primitives_rs::serialization::json::JsSafeI64`, which reject values
	/// outside JavaScript's safe integer range.
	Integer,
	/// Rust integer type with a range that a JavaScript `number` cannot
	/// represent exactly.
	///
	/// [`crate::tsgen::render::render_type_ref`] returns
	/// [`crate::tsgen::render::TsContractError::IntegerRangeExceedsJsPrecision`]
	/// for this variant. An application must give such a value an explicit
	/// wire representation, such as [`crate::tsgen::IntegerString`] so that
	/// JavaScript code does not receive a rounded number.
	WideInteger {
		/// Name of the Rust integer type, such as `u64`.
		rust_type: String,
	},
	/// Named type reference.
	Named {
		/// Stable key that identifies the referenced [`TypeDef`].
		///
		/// Rendering never writes the key into TypeScript. The `TsGen`
		/// derive builds the key from the Rust type's module path, the Rust
		/// type name, and the phase, so two Rust types with the same `name`
		/// have different keys.
		key: String,
		/// TypeScript type name that rendering writes into generated
		/// TypeScript.
		///
		/// Two definitions with different keys can export the same name; see
		/// [`TypeDef::merge_shared_name_with`].
		name: String,
		/// Arguments supplied to the referenced generic declaration.
		#[serde(skip_serializing_if = "Vec::is_empty")]
		type_args: Vec<TypeRef>,
	},
	/// A parameter declared by the enclosing generic type.
	Parameter(String),
	/// Documentation attached to an expression, including an enum union member.
	Documented {
		/// JSDoc text.
		docs: String,
		/// The documented expression.
		type_ref: Box<TypeRef>,
	},
	/// `Array<T>`.
	Array(Box<TypeRef>),
	/// `Record<K, V>`.
	Record(Box<TypeRef>, Box<TypeRef>),
	/// Platform `FormData`.
	FormData,
	/// Platform `Blob`.
	Blob,
	/// `T | null`.
	Nullable(Box<TypeRef>),
	/// `A | B | ...`. An empty union renders as `never`.
	Union(Vec<TypeRef>),
	/// `A & B & ...`. An empty intersection renders as `unknown`.
	///
	/// The `TsGen` derive uses an intersection for a struct with
	/// `#[serde(flatten)]` fields and for a newtype variant of an internally
	/// tagged enum.
	Intersection(Vec<TypeRef>),
	/// Object type, `{ name: T; other?: U }`. An object without fields
	/// renders as `Record<never, never>`.
	Object(Vec<FieldDef>),
	/// Tuple type, `[A, B, ...]`.
	Tuple(Vec<TypeRef>),
	/// String literal type.
	StringLiteral(String),
	/// Trusted raw TypeScript type expression.
	///
	/// Use this variant for a TypeScript type that the other variants cannot
	/// express. Rendering writes the text parts without validating or
	/// escaping them, so the text must be TypeScript source that the
	/// application author controls. The text must never contain end-user or
	/// request data.
	Raw(Vec<RawTsPart>),
}

impl TypeRef {
	/// Create a named type reference that uses `name` as both its key and its
	/// name.
	pub fn named(name: impl Into<String>) -> Self {
		let name = name.into();
		Self::named_with_key(name.clone(), name)
	}

	/// Create a named type reference with a separate stable key.
	pub fn named_with_key(key: impl Into<String>, name: impl Into<String>) -> Self {
		Self::Named {
			key: key.into(),
			name: name.into(),
			type_args: Vec::new(),
		}
	}

	/// Create a reference to a generic declaration.
	pub fn generic(
		key: impl Into<String>,
		name: impl Into<String>,
		type_args: Vec<TypeRef>,
	) -> Self {
		Self::Named {
			key: key.into(),
			name: name.into(),
			type_args,
		}
	}

	/// Create an expression that documents `type_ref` with JSDoc text.
	pub fn documented(type_ref: TypeRef, docs: impl Into<String>) -> Self {
		Self::Documented {
			docs: docs.into(),
			type_ref: Box::new(type_ref),
		}
	}

	/// Create an array type reference.
	pub fn array(inner: TypeRef) -> Self {
		Self::Array(Box::new(inner))
	}

	/// Create a TypeScript Record type expression.
	pub fn record(key: TypeRef, value: TypeRef) -> Self {
		Self::Record(Box::new(key), Box::new(value))
	}

	/// Create a nullable type reference.
	pub fn nullable(inner: TypeRef) -> Self {
		Self::Nullable(Box::new(inner))
	}

	/// Create a string literal type reference.
	pub fn string_literal(value: impl Into<String>) -> Self {
		Self::StringLiteral(value.into())
	}

	/// Create a trusted raw TypeScript type expression.
	pub fn raw(ts: impl Into<String>) -> Self {
		Self::Raw(vec![RawTsPart::Text(ts.into())])
	}
}

/// Part of a raw TypeScript type expression.
///
/// A [`TypeRef::Raw`] expression consists of trusted text parts and nested
/// type references. Rendering writes text parts unchanged and renders nested
/// references with the same validation as other references, so a raw
/// expression can refer to a declared type without spelling that type's name
/// as text.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub enum RawTsPart {
	/// Literal TypeScript text.
	Text(String),
	/// Nested type reference, rendered and validated like any other reference.
	TypeRef(TypeRef),
}

/// Named TypeScript type definition, `type {name} = {type_ref};`.
///
/// A [`TypeRef`] refers to a type at a use site, and a `TypeDef` declares a
/// named type's shape once. A consumer that collects `TypeDef`s should
/// reject two different definitions with the same key. Two definitions with
/// different keys and the same exported name also conflict, unless
/// [`Self::merge_shared_name_with`] merges them into one definition.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TypeDef {
	key: String,
	name: String,
	type_ref: TypeRef,
	#[serde(skip_serializing_if = "Vec::is_empty")]
	type_params: Vec<String>,
	/// Documentation text, rendered as a JSDoc comment.
	#[serde(skip_serializing_if = "Option::is_none")]
	docs: Option<String>,
}

impl TypeDef {
	/// Create a definition that exports `name` with the shape `type_ref`.
	///
	/// `key` identifies the definition: a [`TypeRef::Named`] reference with
	/// this key refers to this definition, and
	/// [`crate::tsgen::TypeRegistry`] keeps one definition per key.
	///
	/// ```
	/// use primitives_rs::tsgen::{FieldDef, FieldPresence, TypeDef, TypeRef};
	///
	/// let user = TypeDef::new(
	///     "app::model::User",
	///     "User",
	///     TypeRef::Object(vec![FieldDef::new(
	///         "name",
	///         TypeRef::String,
	///         FieldPresence::Required,
	///     )]),
	/// );
	///
	/// assert_eq!(user.key(), "app::model::User");
	/// assert_eq!(user.name(), "User");
	/// ```
	pub fn new(key: impl Into<String>, name: impl Into<String>, type_ref: TypeRef) -> Self {
		Self {
			key: key.into(),
			name: name.into(),
			type_ref,
			type_params: Vec::new(),
			docs: None,
		}
	}

	pub(crate) fn with_type_ref(&self, type_ref: TypeRef) -> Self {
		Self {
			type_ref,
			..self.clone()
		}
	}

	/// Set the ordered type parameters that this declaration binds.
	pub fn set_type_params(mut self, type_params: Vec<String>) -> Self {
		self.type_params = type_params;
		self
	}

	/// Return the ordered parameter names.
	pub fn type_params(&self) -> &[String] {
		&self.type_params
	}

	/// Substitute concrete arguments into this definition's body.
	pub fn instantiate(&self, type_args: &[TypeRef]) -> crate::tsgen::Result<TypeRef> {
		if type_args.len() != self.type_params.len() {
			return Err(crate::tsgen::Error::new(format!(
				"{} expects {} type arguments, received {}",
				self.name,
				self.type_params.len(),
				type_args.len()
			)));
		}
		Ok(self
			.type_ref
			.map_type_refs(&mut |reference| match reference {
				TypeRef::Parameter(name) => self
					.type_params
					.iter()
					.position(|param| param == name)
					.map(|index| type_args[index].clone()),
				_ => None,
			}))
	}

	/// Set this definition's documentation text, replacing any prior value.
	pub fn set_docs(mut self, docs: impl Into<String>) -> Self {
		self.docs = Some(docs.into());
		self
	}

	/// Returns this definition's stable identity key.
	pub fn key(&self) -> &str {
		&self.key
	}

	/// Returns this definition's exported TypeScript name.
	pub fn name(&self) -> &str {
		&self.name
	}

	/// Returns the type expression that this definition names.
	pub fn type_ref(&self) -> &TypeRef {
		&self.type_ref
	}

	/// Returns this definition's documentation text, when present.
	///
	/// The `TsGen` derive sets this text from the Rust type's doc comments,
	/// and [`Self::set_docs`] sets it on any definition, including one that
	/// a hand-written [`crate::tsgen::TsGen`] implementation creates.
	/// [`crate::tsgen::render`] writes the text as a JSDoc comment.
	pub fn docs(&self) -> Option<&str> {
		self.docs.as_deref()
	}

	/// Merge two definitions that export the same TypeScript name into one
	/// definition that describes both, or report why one definition cannot
	/// describe both.
	///
	/// # Phases of one Rust type
	///
	/// The `TsGen` derive registers each Rust type's serialize-phase and
	/// deserialize-phase definitions under the keys
	/// `{module_path}::{TypeIdent}::serialize` and
	/// `{module_path}::{TypeIdent}::deserialize`. Two definitions are the two
	/// phases of one Rust type when one key ends with `::serialize`, the
	/// other key ends with `::deserialize`, removing those suffixes leaves
	/// the same string, and the definitions have the same name. Equivalent
	/// shapes alone do not make two definitions the phases of one type. A
	/// hand-written [`crate::tsgen::TsGen`] implementation must key its two
	/// phases with the same convention to take part in merging. For any
	/// other pair of definitions, this method returns
	/// [`SharedTypeNameConflict::UnrelatedTypes`].
	///
	/// # Merging the two phases
	///
	/// The merged definition describes every value that serialization writes
	/// and only values that deserialization accepts. The two phases merge
	/// when their type expressions have the same structure: the same
	/// kinds of expressions, the same object field names in the same order,
	/// the same union, intersection, and tuple members in the same order,
	/// and the same literals and raw text. Nested [`TypeRef::Named`]
	/// references are compared by name only because a nested derived
	/// type's key differs by phase even when that type has the same shape in
	/// both phases.
	///
	/// An object field, at any depth, may differ in presence between the two
	/// phases in one way. When the serialize phase always writes the field
	/// and the deserialize phase accepts a value without it, the merged
	/// field is optional (`name?: T`). For example, an `Option<T>` field
	/// without `#[serde(skip_serializing_if = "...")]` is required in the
	/// serialize phase and optional in the deserialize phase, so the merged
	/// field is `name?: T | null`. An object field may also differ in type in
	/// one way: when the deserialize phase accepts `T | null` and the
	/// serialize phase writes only `T`, as for an `Option<T>` field with
	/// `#[serde(skip_serializing_if = "Option::is_none")]`, the merged field
	/// has the type `T | null`. A field that the serialize phase can omit
	/// and the deserialize phase requires returns
	/// [`SharedTypeNameConflict::FieldOptionalOnlyInSerializePhase`], and any
	/// other difference returns [`SharedTypeNameConflict::DivergentPhaseShape`].
	///
	/// The merged definition has the key that the two phase keys share
	/// without their suffixes, such as `{module_path}::{TypeIdent}`, and the
	/// serialize-phase definition's documentation text. Its nested
	/// [`TypeRef::Named`] references keep the serialize phase's keys.
	///
	/// ```
	/// use primitives_rs::tsgen::declarations::SharedTypeNameConflict;
	/// use primitives_rs::tsgen::{FieldDef, FieldPresence, TypeDef, TypeRef};
	///
	/// let user = |key: &str, presence: FieldPresence| {
	///     TypeDef::new(
	///         key,
	///         "User",
	///         TypeRef::Object(vec![FieldDef::new(
	///             "nickname",
	///             TypeRef::nullable(TypeRef::String),
	///             presence,
	///         )]),
	///     )
	/// };
	/// let serialize_phase = user("app::User::serialize", FieldPresence::Required);
	/// let deserialize_phase = user("app::User::deserialize", FieldPresence::Optional);
	///
	/// assert_eq!(
	///     serialize_phase.merge_shared_name_with(&deserialize_phase),
	///     Ok(user("app::User", FieldPresence::Optional))
	/// );
	/// let unrelated = user("app::Account::serialize", FieldPresence::Required);
	/// assert_eq!(
	///     deserialize_phase.merge_shared_name_with(&unrelated),
	///     Err(SharedTypeNameConflict::UnrelatedTypes)
	/// );
	/// ```
	pub fn merge_shared_name_with(&self, other: &Self) -> Result<Self, SharedTypeNameConflict> {
		let (serialize_def, deserialize_def, base_key) =
			match (split_phase_key(&self.key), split_phase_key(&other.key)) {
				(
					Some((self_base, TypePhase::Serialize)),
					Some((other_base, TypePhase::Deserialize)),
				) if self_base == other_base => (self, other, self_base),
				(
					Some((self_base, TypePhase::Deserialize)),
					Some((other_base, TypePhase::Serialize)),
				) if self_base == other_base => (other, self, self_base),
				_ => return Err(SharedTypeNameConflict::UnrelatedTypes),
			};
		if self.name != other.name || self.type_params != other.type_params {
			return Err(SharedTypeNameConflict::UnrelatedTypes);
		}
		Ok(Self {
			key: base_key.to_owned(),
			name: serialize_def.name.clone(),
			type_ref: merge_phase_type_refs(&serialize_def.type_ref, &deserialize_def.type_ref)?,
			docs: serialize_def.docs.clone(),
			type_params: serialize_def.type_params.clone(),
		})
	}
}

/// Reason why [`TypeDef::merge_shared_name_with`] cannot merge two
/// definitions into one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SharedTypeNameConflict {
	/// The definitions are not the serialize-phase and deserialize-phase
	/// definitions of one Rust type, so they are two different types with
	/// one exported name.
	UnrelatedTypes,
	/// The serialize phase can omit an object field that the deserialize
	/// phase requires, so no one definition describes both phases.
	///
	/// A derived field has this difference when it has
	/// `#[serde(skip_serializing_if = "...")]`, its type is not `Option<T>`,
	/// and neither the field nor its struct has `#[serde(default)]`. Adding
	/// `#[serde(default)]` to the field makes the field optional in both
	/// phases.
	FieldOptionalOnlyInSerializePhase {
		/// The field's name in both phases. A field of a nested object is
		/// named without its enclosing fields.
		field_name: String,
	},
	/// The phases differ in a way other than field presence, so no one
	/// definition describes both phases.
	///
	/// For example, a field or variant can have a different name in each
	/// phase, or only one phase can contain it.
	DivergentPhaseShape {
		/// The name of the innermost object field that differs or that
		/// contains the difference, as the serialize phase names it when
		/// that phase has the field. A field of a nested object is named
		/// without its enclosing fields. `None` when the difference is not
		/// inside an object field, such as a union with a different number
		/// of members in each phase.
		member_name: Option<String>,
	},
}

impl std::fmt::Display for SharedTypeNameConflict {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::UnrelatedTypes => f.write_str(
				"the definitions are not the serialize-phase and deserialize-phase definitions of one Rust type",
			),
			Self::FieldOptionalOnlyInSerializePhase { field_name } => write!(
				f,
				"the serialize phase can omit the field {field_name}, which the deserialize phase requires"
			),
			Self::DivergentPhaseShape {
				member_name: Some(member_name),
			} => write!(
				f,
				"the serialize and deserialize phases differ at the field {member_name}"
			),
			Self::DivergentPhaseShape { member_name: None } => {
				f.write_str("the serialize and deserialize phases have different shapes")
			}
		}
	}
}

/// Names the field `field_name` in a divergence that no inner field names.
fn name_divergent_field(
	conflict: SharedTypeNameConflict,
	field_name: &str,
) -> SharedTypeNameConflict {
	match conflict {
		SharedTypeNameConflict::DivergentPhaseShape { member_name: None } => {
			SharedTypeNameConflict::DivergentPhaseShape {
				member_name: Some(field_name.to_owned()),
			}
		}
		other => other,
	}
}

/*
The `TsGen` derive macro builds every key as `{module_path}::{TypeIdent}::serialize`
or `{module_path}::{TypeIdent}::deserialize`, and
`TypeDef::merge_shared_name_with` uses these two suffixes to recognize the two
phases of one Rust type. The derive macro implementation in
`primitives-rs-macros-core/src/lib.rs` spells the suffixes as string
literals because `concat!` accepts only literal tokens, so those literals must
equal these constants.
*/
pub(crate) const SERIALIZE_PHASE_KEY_SUFFIX: &str = "::serialize";
pub(crate) const DESERIALIZE_PHASE_KEY_SUFFIX: &str = "::deserialize";

/// Splits a `TypeDef` key into the part that identifies the Rust type in
/// both phases and the phase that its suffix names. Returns `None` when
/// `key` ends with neither suffix.
pub(crate) fn split_phase_key(key: &str) -> Option<(&str, TypePhase)> {
	if let Some(base) = key.strip_suffix(SERIALIZE_PHASE_KEY_SUFFIX) {
		return Some((base, TypePhase::Serialize));
	}
	key.strip_suffix(DESERIALIZE_PHASE_KEY_SUFFIX)
		.map(|base| (base, TypePhase::Deserialize))
}

/// Merges a serialize-phase type expression with the deserialize-phase
/// expression of the same Rust type, as described on
/// [`TypeDef::merge_shared_name_with`].
fn merge_phase_type_refs(
	serialize: &TypeRef,
	deserialize: &TypeRef,
) -> Result<TypeRef, SharedTypeNameConflict> {
	Ok(match (serialize, deserialize) {
		(
			TypeRef::Named {
				key,
				name: s,
				type_args: sa,
			},
			TypeRef::Named {
				name: d,
				type_args: da,
				..
			},
		) if s == d => TypeRef::generic(key, s, merge_phase_type_ref_lists(sa, da)?),
		(TypeRef::Documented { docs, type_ref: s }, TypeRef::Documented { type_ref: d, .. }) => {
			TypeRef::documented(merge_phase_type_refs(s, d)?, docs)
		}
		(TypeRef::Array(s), TypeRef::Array(d)) => TypeRef::array(merge_phase_type_refs(s, d)?),
		(TypeRef::Nullable(s), TypeRef::Nullable(d)) => {
			TypeRef::nullable(merge_phase_type_refs(s, d)?)
		}
		(TypeRef::Record(s_key, s_value), TypeRef::Record(d_key, d_value)) => TypeRef::record(
			merge_phase_type_refs(s_key, d_key)?,
			merge_phase_type_refs(s_value, d_value)?,
		),
		(TypeRef::Union(s), TypeRef::Union(d)) => TypeRef::Union(merge_phase_type_ref_lists(s, d)?),
		(TypeRef::Intersection(s), TypeRef::Intersection(d)) => {
			TypeRef::Intersection(merge_phase_type_ref_lists(s, d)?)
		}
		(TypeRef::Tuple(s), TypeRef::Tuple(d)) => TypeRef::Tuple(merge_phase_type_ref_lists(s, d)?),
		(TypeRef::Object(s), TypeRef::Object(d)) => TypeRef::Object(merge_phase_fields(s, d)?),
		(TypeRef::Raw(s), TypeRef::Raw(d)) if s.len() == d.len() => TypeRef::Raw(
			s.iter()
				.zip(d)
				.map(|parts| match parts {
					(RawTsPart::Text(s), RawTsPart::Text(d)) if s == d => {
						Ok(RawTsPart::Text(s.clone()))
					}
					(RawTsPart::TypeRef(s), RawTsPart::TypeRef(d)) => {
						merge_phase_type_refs(s, d).map(RawTsPart::TypeRef)
					}
					_ => Err(SharedTypeNameConflict::DivergentPhaseShape { member_name: None }),
				})
				.collect::<Result<_, _>>()?,
		),
		// Every pair of expressions that can contain a `Named` reference has
		// an arm above, so equality decides the remaining pairs.
		_ if serialize == deserialize => serialize.clone(),
		_ => return Err(SharedTypeNameConflict::DivergentPhaseShape { member_name: None }),
	})
}

fn merge_phase_type_ref_lists(
	serialize: &[TypeRef],
	deserialize: &[TypeRef],
) -> Result<Vec<TypeRef>, SharedTypeNameConflict> {
	if serialize.len() != deserialize.len() {
		return Err(SharedTypeNameConflict::DivergentPhaseShape { member_name: None });
	}
	serialize
		.iter()
		.zip(deserialize)
		.map(|(s, d)| merge_phase_type_refs(s, d))
		.collect()
}

fn merge_phase_fields(
	serialize: &[FieldDef],
	deserialize: &[FieldDef],
) -> Result<Vec<FieldDef>, SharedTypeNameConflict> {
	if serialize.len() != deserialize.len() {
		// The first position where the phases name different fields, or the
		// first field that only the longer phase has.
		let member_name = (0..serialize.len().max(deserialize.len())).find_map(|index| {
			match (serialize.get(index), deserialize.get(index)) {
				(Some(s), Some(d)) if s.name == d.name => None,
				(Some(field), _) | (None, Some(field)) => Some(field.name.clone()),
				(None, None) => None,
			}
		});
		return Err(SharedTypeNameConflict::DivergentPhaseShape { member_name });
	}
	serialize
		.iter()
		.zip(deserialize)
		.map(|(s, d)| {
			if s.name != d.name {
				return Err(SharedTypeNameConflict::DivergentPhaseShape {
					member_name: Some(s.name.clone()),
				});
			}
			let type_ref = match merge_phase_type_refs(&s.type_ref, &d.type_ref) {
				Ok(type_ref) => type_ref,
				// The deserialize phase accepts `T | null` where the serialize
				// phase writes only `T`, so the merged field accepts both.
				Err(conflict @ SharedTypeNameConflict::DivergentPhaseShape { .. }) => {
					match &d.type_ref {
						TypeRef::Nullable(d_inner) => TypeRef::nullable(
							merge_phase_type_refs(&s.type_ref, d_inner)
								.map_err(|conflict| name_divergent_field(conflict, &s.name))?,
						),
						_ => return Err(name_divergent_field(conflict, &s.name)),
					}
				}
				Err(conflict) => return Err(conflict),
			};
			let presence = match (s.presence, d.presence) {
				(FieldPresence::Required, FieldPresence::Required) => FieldPresence::Required,
				(FieldPresence::Required | FieldPresence::Optional, FieldPresence::Optional) => {
					FieldPresence::Optional
				}
				(FieldPresence::Optional, FieldPresence::Required) => {
					return Err(SharedTypeNameConflict::FieldOptionalOnlyInSerializePhase {
						field_name: s.name.clone(),
					});
				}
			};
			Ok(FieldDef {
				name: s.name.clone(),
				type_ref,
				presence,
				docs: s.docs.clone(),
			})
		})
		.collect()
}

/// Whether an object field must be present.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum FieldPresence {
	/// The field is always present, `name: T`.
	Required,
	/// The field can be absent, `name?: T`.
	Optional,
}

/// Field of a TypeScript object type.
///
/// One property of a [`TypeRef::Object`]: the property's name, its
/// TypeScript type, and its [`FieldPresence`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FieldDef {
	name: String,
	type_ref: TypeRef,
	presence: FieldPresence,
	/// Documentation text, rendered as a JSDoc comment.
	#[serde(skip_serializing_if = "Option::is_none")]
	docs: Option<String>,
}

impl FieldDef {
	/// Create a field with a type reference and a presence.
	///
	/// A hand-written [`crate::tsgen::TsGen`] implementation can take a field
	/// type's reference from that type's
	/// [`TsGen::type_ref_for`](crate::tsgen::TsGen::type_ref_for).
	///
	/// ```
	/// use primitives_rs::tsgen::{FieldDef, FieldPresence, TsGen, TypePhase};
	///
	/// let field = FieldDef::new(
	///     "name",
	///     String::type_ref_for(TypePhase::Serialize),
	///     FieldPresence::Required,
	/// );
	///
	/// assert_eq!(field.name(), "name");
	/// assert_eq!(field.presence(), FieldPresence::Required);
	/// ```
	pub fn new(name: impl Into<String>, type_ref: TypeRef, presence: FieldPresence) -> Self {
		Self {
			name: name.into(),
			type_ref,
			presence,
			docs: None,
		}
	}

	/// Set this field's documentation text, replacing any prior value.
	pub fn set_docs(mut self, docs: impl Into<String>) -> Self {
		self.docs = Some(docs.into());
		self
	}

	/// Returns the field name.
	pub fn name(&self) -> &str {
		&self.name
	}

	/// Returns the field's type reference.
	pub fn type_ref(&self) -> &TypeRef {
		&self.type_ref
	}

	/// Returns whether the field must be present.
	pub fn presence(&self) -> FieldPresence {
		self.presence
	}

	/// Returns the field's documentation text, when present.
	///
	/// The `TsGen` derive sets this text from the Rust field's doc comments,
	/// and [`Self::set_docs`] sets it on any field.
	pub fn docs(&self) -> Option<&str> {
		self.docs.as_deref()
	}
}

impl TypeRef {
	/// Visit this expression and every nested expression in declaration order.
	pub fn visit(&self, visit_fn: &mut impl FnMut(&TypeRef)) {
		visit_fn(self);
		match self {
			Self::Named { type_args, .. }
			| Self::Union(type_args)
			| Self::Intersection(type_args)
			| Self::Tuple(type_args) => {
				for arg in type_args {
					arg.visit(visit_fn);
				}
			}
			Self::Array(inner)
			| Self::Nullable(inner)
			| Self::Documented {
				type_ref: inner, ..
			} => inner.visit(visit_fn),
			Self::Record(key, value) => {
				key.visit(visit_fn);
				value.visit(visit_fn);
			}
			Self::Object(fields) => {
				for field in fields {
					field.type_ref.visit(visit_fn);
				}
			}
			Self::Raw(parts) => {
				for part in parts {
					if let RawTsPart::TypeRef(inner) = part {
						inner.visit(visit_fn);
					}
				}
			}
			_ => {}
		}
	}

	/// Return the names referenced by this expression, in lexical order.
	pub fn referenced_names(&self) -> std::collections::BTreeSet<String> {
		let mut names = std::collections::BTreeSet::new();
		self.visit(&mut |reference| {
			if let Self::Named { name, .. } = reference {
				names.insert(name.clone());
			}
		});
		names
	}

	/// Replace matching expressions and recursively transform unmatched expressions.
	pub fn map_type_refs(&self, replace_fn: &mut impl FnMut(&TypeRef) -> Option<TypeRef>) -> Self {
		if let Some(replacement) = replace_fn(self) {
			return replacement;
		}
		match self.try_map_children(&mut |nested| {
			Ok::<_, std::convert::Infallible>(nested.map_type_refs(replace_fn))
		}) {
			Ok(mapped) => mapped,
			Err(never) => match never {},
		}
	}

	/// Transforms immediate children in declaration order, stopping at the first failure.
	pub(crate) fn try_map_children<E>(
		&self,
		map_fn: &mut impl FnMut(&TypeRef) -> std::result::Result<TypeRef, E>,
	) -> std::result::Result<Self, E> {
		Ok(match self {
			Self::Named {
				key,
				name,
				type_args,
			} => Self::generic(
				key,
				name,
				type_args
					.iter()
					.map(&mut *map_fn)
					.collect::<std::result::Result<_, _>>()?,
			),
			Self::Array(inner) => Self::array(map_fn(inner)?),
			Self::Nullable(inner) => Self::nullable(map_fn(inner)?),
			Self::Documented { docs, type_ref } => Self::documented(map_fn(type_ref)?, docs),
			Self::Record(key, value) => Self::record(map_fn(key)?, map_fn(value)?),
			Self::Union(members) => Self::Union(
				members
					.iter()
					.map(&mut *map_fn)
					.collect::<std::result::Result<_, _>>()?,
			),
			Self::Intersection(members) => Self::Intersection(
				members
					.iter()
					.map(&mut *map_fn)
					.collect::<std::result::Result<_, _>>()?,
			),
			Self::Tuple(members) => Self::Tuple(
				members
					.iter()
					.map(&mut *map_fn)
					.collect::<std::result::Result<_, _>>()?,
			),
			Self::Object(fields) => Self::Object(
				fields
					.iter()
					.map(|field| {
						Ok(FieldDef {
							type_ref: map_fn(&field.type_ref)?,
							..field.clone()
						})
					})
					.collect::<std::result::Result<_, _>>()?,
			),
			Self::Raw(parts) => Self::Raw(
				parts
					.iter()
					.map(|part| {
						Ok(match part {
							RawTsPart::Text(text) => RawTsPart::Text(text.clone()),
							RawTsPart::TypeRef(inner) => RawTsPart::TypeRef(map_fn(inner)?),
						})
					})
					.collect::<std::result::Result<_, _>>()?,
			),
			_ => self.clone(),
		})
	}
}
