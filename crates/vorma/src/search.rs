//! URL search parameters: the search shape of a view pattern or a GET endpoint.
//!
//! The browser client receives a view pattern's [`SearchShape`] as its
//! `search_shape`. It writes links and GET endpoint input with the grammar
//! below, and the server parses each request's query with the same shape. The
//! client's implementation (`packages/vorma-client/src/protocol/search.ts`) uses the
//! same grammar, and both are checked against one set of shared vectors.
//!
//! Dots in a key separate nesting. An array repeats its key once for each
//! item, so an array can hold only scalars and null; an array or an object
//! inside an array has no spelling. Reserved spellings start with a tilde: `~n`
//! is null, `~a` an empty array, `~o` an empty object, and `~s` prefixes a
//! scalar that itself starts with a tilde; `~s` alone is the empty string.
//! Inside a key segment, `~0` is a tilde, `~1` a dot, and `~2` the empty
//! segment.
//!
//! A plain empty value means that nothing was entered, as a form submits an
//! empty input: an optional field is absent, a nullable field is null, a
//! required string is the empty string, and a required field of another type
//! is missing. Empty entries of an array or a map are dropped.
//!
//! The name `vorma-json` belongs to Vorma's wire protocol: the browser sets it
//! on page reads, and the server removes it from a page's query before a
//! loader reads it. A search shape therefore cannot declare a top-level
//! field with that name, and deriving the contract fails when it does. The
//! browser client refuses to build an address that carries it.
//!
//! The name `dpl` belongs to Vercel's skew protection. It is reserved only
//! while skew protection is on in the process, which is when the process has
//! a Vercel deployment ID. Then the browser client sets `dpl` on the page
//! reads that revalidate the current page, and not on navigations or
//! prefetches, and it sends the deployment ID of an endpoint call in the
//! `x-deployment-id` header instead. The server removes `dpl` from a page's
//! query, the browser client refuses to build an address that carries it, and
//! loading the app fails when a search contract declares a top-level field
//! named `dpl`. While skew protection is off, `dpl` is an ordinary search
//! name.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;

use primitives_rs::tsgen::{
	self, FieldDef, FieldPresence, TsGen, TypeDef, TypePhase, TypeRef, TypeRegistry,
};
use serde::Serialize;
use serde::ser::{SerializeMap, Serializer};
use serde_json::{Map, Number, Value};

const ESCAPE_PREFIX: char = '~';
const KEY_SEPARATOR: char = '.';
const ESCAPED_TILDE: &str = "~0";
const ESCAPED_KEY_SEPARATOR: &str = "~1";
const EMPTY_KEY_SEGMENT: &str = "~2";
const NULL_MARKER: &str = "~n";
const EMPTY_ARRAY_MARKER: &str = "~a";
const EMPTY_OBJECT_MARKER: &str = "~o";
const ESCAPED_SCALAR_PREFIX: &str = "~s";

/// The query key through which the browser asks for a page's navigation
/// payload and names the build that it holds.
pub const VIEW_PAYLOAD_QUERY_KEY: &str = "vorma-json";
/// The query key through which Vercel's skew protection names a deployment.
/// It is reserved only while skew protection is on.
pub const DEPLOYMENT_QUERY_KEY: &str = "dpl";

/// Whether [`DEPLOYMENT_QUERY_KEY`] belongs to Vercel's skew protection in
/// the process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeploymentQueryKey {
	/// Skew protection is on: Vercel reads the key to choose the deployment
	/// that serves a request, and the server removes the key from a page's
	/// query. A target's search input cannot use it.
	Reserved,
	/// Skew protection is off, and the key is an ordinary search name.
	Ordinary,
}

/// The URL description of a search value: the part of the client's search
/// shape that a serde input type can declare.
///
/// It serializes to the browser client's `SearchShape` JSON representation.
/// Object-field order matches JavaScript property enumeration.
#[derive(Clone, Debug, PartialEq)]
pub enum SearchShape {
	/// One value spelled as text.
	Scalar(SearchScalar),
	/// Named fields, each under its own dotted key.
	Object {
		/// The fields in the order that a JavaScript object lists them.
		fields: Vec<SearchField>,
		/// Whether `~n` or an absent key means null.
		nullable: bool,
	},
	/// Scalars repeated under one key.
	Array {
		/// The description of each item.
		item: SearchScalar,
		/// Whether `~n` or an absent key means null.
		nullable: bool,
	},
	/// String-keyed entries, each under its own dotted key.
	Record {
		/// The permitted keys.
		permitted_keys: PermittedStrings,
		/// The description of each entry's value.
		value: Box<SearchShape>,
		/// Whether `~n` or an absent key means null.
		nullable: bool,
	},
}

/// The strings that a scalar or a record key may spell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PermittedStrings {
	/// Any string.
	AnyString,
	/// Exactly these strings, as a string enum lists them.
	OneOf(Vec<String>),
}

impl PermittedStrings {
	fn to_list(&self) -> Option<&[String]> {
		match self {
			Self::AnyString => None,
			Self::OneOf(strings) => Some(strings),
		}
	}
}

/// A value spelled as one URL text.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchScalar {
	/// What the text spells.
	pub kind: SearchScalarKind,
	/// The permitted strings of the scalar.
	pub permitted_strings: PermittedStrings,
	/// Whether `~n` or an absent key means null.
	pub nullable: bool,
}

/// What a scalar's text spells: the `type` of a `SearchScalar`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchScalarKind {
	/// Any text.
	String,
	/// A finite decimal number.
	Number,
	/// `true`, `false`, `1`, `0`, or `on`, in any letter case.
	Boolean,
}

/// One field of an object shape.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchField {
	/// The field's name in the deserialized value.
	pub name: String,
	/// The description of the field's value.
	pub shape: SearchShape,
	/// Whether the field may be absent.
	pub optional: bool,
}

impl SearchShape {
	/// Whether this shape is an object with a field of this name.
	pub(crate) fn has_top_level_field(&self, name: &str) -> bool {
		matches!(self, Self::Object { fields, .. } if fields.iter().any(|field| field.name == name))
	}

	fn is_nullable(&self) -> bool {
		match self {
			Self::Scalar(scalar) => scalar.nullable,
			Self::Object { nullable, .. }
			| Self::Array { nullable, .. }
			| Self::Record { nullable, .. } => *nullable,
		}
	}

	fn into_nullable(mut self) -> Self {
		match &mut self {
			Self::Scalar(SearchScalar { nullable, .. })
			| Self::Object { nullable, .. }
			| Self::Array { nullable, .. }
			| Self::Record { nullable, .. } => *nullable = true,
		}
		self
	}

	/// Writes the shape's JSON. An object field's `optional` flag follows the
	/// shape's own keys.
	fn serialize_with_optional<S: Serializer>(
		&self,
		serializer: S,
		optional: bool,
	) -> Result<S::Ok, S::Error> {
		let mut map = match self {
			Self::Scalar(scalar) => {
				return ScalarJson::of(scalar, optional).serialize(serializer);
			}
			Self::Object { fields, .. } => {
				let mut map = serializer.serialize_map(None)?;
				map.serialize_entry("kind", "object")?;
				map.serialize_entry("fields", &FieldsJson(fields))?;
				map
			}
			Self::Array { item, .. } => {
				let mut map = serializer.serialize_map(None)?;
				map.serialize_entry("kind", "array")?;
				map.serialize_entry("collection", "array")?;
				map.serialize_entry("item", &ScalarJson::of(item, false))?;
				map
			}
			Self::Record {
				permitted_keys,
				value,
				..
			} => {
				let mut map = serializer.serialize_map(None)?;
				map.serialize_entry("kind", "record")?;
				map.serialize_entry("collection", "record")?;
				map.serialize_entry(
					"key",
					&ScalarJson {
						kind: SearchScalarKind::String,
						values: permitted_keys.to_list(),
						nullable: false,
						optional: false,
					},
				)?;
				map.serialize_entry("value", value.as_ref())?;
				map
			}
		};
		if self.is_nullable() {
			map.serialize_entry("nullable", &true)?;
		}
		if optional {
			map.serialize_entry("optional", &true)?;
		}
		map.end()
	}
}

impl Serialize for SearchShape {
	fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		self.serialize_with_optional(serializer, false)
	}
}

/// The JSON of a scalar shape.
#[derive(Serialize)]
#[serde(tag = "kind", rename = "scalar")]
struct ScalarJson<'a> {
	#[serde(rename = "type")]
	kind: SearchScalarKind,
	#[serde(skip_serializing_if = "Option::is_none")]
	values: Option<&'a [String]>,
	#[serde(skip_serializing_if = "is_false")]
	nullable: bool,
	#[serde(skip_serializing_if = "is_false")]
	optional: bool,
}

impl<'a> ScalarJson<'a> {
	fn of(scalar: &'a SearchScalar, optional: bool) -> Self {
		Self {
			kind: scalar.kind,
			values: scalar.permitted_strings.to_list(),
			nullable: scalar.nullable,
			optional,
		}
	}
}

fn is_false(value: &bool) -> bool {
	!value
}

/// The JSON of an object shape's fields, keyed by name.
struct FieldsJson<'a>(&'a [SearchField]);

impl Serialize for FieldsJson<'_> {
	fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		serializer.collect_map(
			self.0
				.iter()
				.map(|field| (field.name.as_str(), FieldJson(field))),
		)
	}
}

/// The JSON of one field's shape, with its `optional` flag.
struct FieldJson<'a>(&'a SearchField);

impl Serialize for FieldJson<'_> {
	fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		self.0
			.shape
			.serialize_with_optional(serializer, self.0.optional)
	}
}

/// A search input type without a URL spelling.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SearchShapeError {
	/// The type's TypeScript definitions could not be collected.
	Defs(tsgen::Error),
	/// A top-level field has the name that Vorma's wire protocol owns.
	ReservedName {
		/// The reserved name that the contract declares.
		name: String,
	},
	/// The value at a dotted field path has no URL spelling.
	Unsupported {
		/// The dotted field path, empty for the root.
		path: String,
		/// Why the value has no URL spelling.
		reason: String,
	},
}

impl fmt::Display for SearchShapeError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Defs(error) => write!(f, "{error}"),
			Self::ReservedName { name } => write!(
				f,
				"field {name:?} uses a reserved name; Vorma's wire protocol reserves {VIEW_PAYLOAD_QUERY_KEY:?}"
			),
			Self::Unsupported { path, reason } if path.is_empty() => {
				write!(f, "unsupported search input: {reason}")
			}
			Self::Unsupported { path, reason } => {
				write!(f, "unsupported search input field {path:?}: {reason}")
			}
		}
	}
}

impl Error for SearchShapeError {}

fn unsupported_error(path: &str, reason: impl Into<String>) -> SearchShapeError {
	SearchShapeError::Unsupported {
		path: path.to_owned(),
		reason: reason.into(),
	}
}

/// Derives the search shape of a Rust input type from its `TsGen`
/// description of the deserialized form.
///
/// The root must be a struct or a string-keyed map. Strings, numbers,
/// booleans, and string enums are scalars; `Option` makes a value nullable; a
/// `Vec` of scalars is an array; and a string-keyed map is a record. An
/// `Option` cannot hold an `Option` or a `Vec` of `Option` values because a
/// query spells every null the same way and so cannot round-trip them. A
/// top-level field cannot be named [`VIEW_PAYLOAD_QUERY_KEY`]. A field named
/// [`DEPLOYMENT_QUERY_KEY`] is allowed here; loading the app rejects it while
/// Vercel skew protection is on.
pub fn derive_search_shape<T: TsGen>() -> Result<SearchShape, SearchShapeError> {
	let mut registry = TypeRegistry::default();
	T::collect_type_defs_for(TypePhase::Deserialize, &mut registry)
		.map_err(SearchShapeError::Defs)?;
	let graph =
		primitives_rs::tsgen::TypeGraph::new(T::type_ref_for(TypePhase::Deserialize), registry)
			.map_err(SearchShapeError::Defs)?;
	let defs = graph.type_defs;
	let root = graph.root;
	let shape = ShapeDerivation {
		defs: &defs,
		visiting: Vec::new(),
	}
	.derive(&root, "")?;
	if let SearchShape::Object {
		fields,
		nullable: false,
	} = &shape
		&& let Some(field) = fields
			.iter()
			.find(|field| field.name == VIEW_PAYLOAD_QUERY_KEY)
	{
		return Err(SearchShapeError::ReservedName {
			name: field.name.clone(),
		});
	}
	match shape {
		SearchShape::Object {
			nullable: false, ..
		}
		| SearchShape::Record {
			nullable: false, ..
		} => Ok(shape),
		_ => Err(unsupported_error(
			"",
			"the search input must deserialize from a structural object",
		)),
	}
}

/// A reference to a named type: its definition key and its name.
#[derive(Clone, Copy)]
struct NamedTypeRef<'a> {
	key: &'a str,
	name: &'a str,
}

struct ShapeDerivation<'a> {
	defs: &'a [TypeDef],
	/// The keys of the named types that enclose the current position.
	visiting: Vec<&'a str>,
}

impl<'a> ShapeDerivation<'a> {
	fn derive(
		&mut self,
		type_ref: &'a TypeRef,
		path: &str,
	) -> Result<SearchShape, SearchShapeError> {
		let no_spelling =
			|type_name: &str| unsupported_error(path, format!("{type_name} has no URL spelling"));
		match type_ref {
			TypeRef::Bool => Ok(scalar_shape(
				SearchScalarKind::Boolean,
				PermittedStrings::AnyString,
			)),
			TypeRef::String => Ok(scalar_shape(
				SearchScalarKind::String,
				PermittedStrings::AnyString,
			)),
			TypeRef::Number | TypeRef::Integer => Ok(scalar_shape(
				SearchScalarKind::Number,
				PermittedStrings::AnyString,
			)),
			TypeRef::StringLiteral(value) => Ok(scalar_shape(
				SearchScalarKind::String,
				PermittedStrings::OneOf(vec![value.clone()]),
			)),
			TypeRef::Nullable(inner) => create_nullable_shape(self.derive(inner, path)?, path),
			TypeRef::Union(members) => self.derive_union(members, path),
			TypeRef::Array(item) => match self.derive(item, path)? {
				SearchShape::Scalar(item) => Ok(SearchShape::Array {
					item,
					nullable: false,
				}),
				_ => Err(unsupported_error(
					path,
					"array items must have one scalar URL spelling",
				)),
			},
			TypeRef::Record(key, value) => {
				let SearchShape::Scalar(SearchScalar {
					kind: SearchScalarKind::String,
					permitted_strings: permitted_keys,
					nullable: false,
				}) = self.derive(key, path)?
				else {
					return Err(unsupported_error(
						path,
						"map keys must be strings or string enums",
					));
				};
				let value = self.derive(value, &format!("{path}{KEY_SEPARATOR}*"))?;
				Ok(SearchShape::Record {
					permitted_keys,
					value: Box::new(value),
					nullable: false,
				})
			}
			TypeRef::Object(fields) => self.derive_object(fields, path),
			TypeRef::Intersection(members) => self.derive_intersection(members, path),
			TypeRef::Named { key, name, .. } => self.derive_named(NamedTypeRef { key, name }, path),
			TypeRef::Tuple(_) => Err(no_spelling("a tuple")),
			TypeRef::Undefined => Err(no_spelling("undefined")),
			TypeRef::Null => Err(no_spelling("null")),
			TypeRef::Unknown => Err(no_spelling("unknown")),
			TypeRef::WideInteger { rust_type } => Err(unsupported_error(
				path,
				format!(
					"the Rust integer type {rust_type} has no URL spelling that JavaScript reads exactly. Use IntegerString<{rust_type}>, which is decimal text, or a 32-bit integer type (i32 or u32) when the range allows"
				),
			)),
			TypeRef::FormData => Err(no_spelling("FormData")),
			TypeRef::Blob => Err(no_spelling("Blob")),
			TypeRef::Raw(_) => Err(no_spelling("a raw TypeScript type")),
			TypeRef::Parameter(_) | TypeRef::Documented { .. } => {
				unreachable!("TypeGraph contains concrete undocumented expressions")
			}
		}
	}

	/// Null members make the union nullable; one other member is the union's
	/// shape, and several must be string literals.
	fn derive_union(
		&mut self,
		members: &'a [TypeRef],
		path: &str,
	) -> Result<SearchShape, SearchShapeError> {
		let carried = members
			.iter()
			.filter(|member| !matches!(member, TypeRef::Null))
			.collect::<Vec<_>>();
		let shape = match carried.as_slice() {
			[] => {
				return Err(unsupported_error(
					path,
					"a union of only null carries nothing",
				));
			}
			[member] => self.derive(member, path)?,
			_ => {
				let values = carried
					.iter()
					.map(|member| match member {
						TypeRef::StringLiteral(value) => Ok(value.clone()),
						_ => Err(unsupported_error(
							path,
							"unions other than string literal enums cannot be search parameters",
						)),
					})
					.collect::<Result<_, _>>()?;
				scalar_shape(SearchScalarKind::String, PermittedStrings::OneOf(values))
			}
		};
		if carried.len() < members.len() {
			create_nullable_shape(shape, path)
		} else {
			Ok(shape)
		}
	}

	fn derive_named(
		&mut self,
		named: NamedTypeRef<'a>,
		path: &str,
	) -> Result<SearchShape, SearchShapeError> {
		let NamedTypeRef { key, name } = named;
		if self.visiting.contains(&key) {
			return Err(unsupported_error(
				path,
				"recursive types cannot be search parameters",
			));
		}
		let defs = self.defs;
		let Some(def) = defs.iter().find(|def| def.key() == key) else {
			return Err(unsupported_error(
				path,
				format!("the type {name} has no collected definition"),
			));
		};
		self.visiting.push(key);
		let shape = self.derive(def.type_ref(), path);
		self.visiting.pop();
		shape
	}

	fn derive_object(
		&mut self,
		fields: &'a [FieldDef],
		path: &str,
	) -> Result<SearchShape, SearchShapeError> {
		let mut fields = fields
			.iter()
			.map(|field| {
				let field_path = if path.is_empty() {
					field.name().to_owned()
				} else {
					format!("{path}{KEY_SEPARATOR}{}", field.name())
				};
				Ok(SearchField {
					name: field.name().to_owned(),
					shape: self.derive(field.type_ref(), &field_path)?,
					optional: field.presence() == FieldPresence::Optional,
				})
			})
			.collect::<Result<Vec<_>, _>>()?;
		order_like_javascript(&mut fields);
		Ok(SearchShape::Object {
			fields,
			nullable: false,
		})
	}

	/// The fields of every member, as `#[serde(flatten)]` combines a struct
	/// with the structs that it flattens.
	fn derive_intersection(
		&mut self,
		members: &'a [TypeRef],
		path: &str,
	) -> Result<SearchShape, SearchShapeError> {
		let mut fields: Vec<SearchField> = Vec::new();
		for member in members {
			let SearchShape::Object {
				fields: member_fields,
				nullable: false,
			} = self.derive(member, path)?
			else {
				return Err(unsupported_error(
					path,
					"a flattened field must be a struct that is not optional",
				));
			};
			for field in member_fields {
				if fields.iter().any(|existing| existing.name == field.name) {
					return Err(unsupported_error(
						path,
						format!("the field {} appears more than once", field.name),
					));
				}
				fields.push(field);
			}
		}
		order_like_javascript(&mut fields);
		Ok(SearchShape::Object {
			fields,
			nullable: false,
		})
	}
}

/// Puts fields in the order in which a JavaScript object lists them:
/// array-index names first, in numeric order, then the others in their
/// order. Both parsers visit fields in that order.
fn order_like_javascript(fields: &mut [SearchField]) {
	fields.sort_by_key(|field| parse_array_index(&field.name).map_or((1, 0), |index| (0, index)));
}

/// Makes `shape` nullable. A query spells every null as `~n`, so a nullable
/// shape cannot hold a second kind of null, or an array of nullable items,
/// without two different values sharing one spelling.
fn create_nullable_shape(shape: SearchShape, path: &str) -> Result<SearchShape, SearchShapeError> {
	if shape.is_nullable() {
		return Err(unsupported_error(
			path,
			"Option<Option<T>> cannot round-trip through a URL because a query spells both \
			 None values as ~n. Use Option<T>",
		));
	}
	if let SearchShape::Array { item, .. } = &shape
		&& item.nullable
	{
		return Err(unsupported_error(
			path,
			"Option<Vec<Option<T>>> cannot round-trip through a URL because a query spells \
			 a null array and a null item both as ~n. Use Vec<Option<T>> when items can be \
			 null, or Option<Vec<T>> when the array can be null",
		));
	}
	Ok(shape.into_nullable())
}

fn scalar_shape(kind: SearchScalarKind, permitted_strings: PermittedStrings) -> SearchShape {
	SearchShape::Scalar(SearchScalar {
		kind,
		permitted_strings,
		nullable: false,
	})
}

/// The index of a property name that JavaScript treats as an array index.
fn parse_array_index(name: &str) -> Option<u32> {
	let index = name.parse::<u32>().ok()?;
	(index != u32::MAX && index.to_string() == name).then_some(index)
}

/// Text that a query entered, with the key that it was entered under.
#[derive(Clone, Copy)]
struct EnteredText<'a> {
	raw: &'a str,
	path: &'a str,
}

/// What the values under a key must spell, as an error message names it.
struct Expected(&'static str);

/// A query that does not spell a value of its search shape.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SearchParamsError {
	/// A required value was not entered.
	Missing {
		/// The encoded key.
		path: String,
	},
	/// The values under a key do not spell its shape.
	Invalid {
		/// The encoded key.
		path: String,
		/// What the key's values must spell.
		expected: &'static str,
	},
}

impl fmt::Display for SearchParamsError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Missing { path } => write!(f, "search parameter {path:?} is missing"),
			Self::Invalid { path, expected } => {
				write!(
					f,
					"search parameter {path:?} is invalid; expected {expected}"
				)
			}
		}
	}
}

impl Error for SearchParamsError {}

/// Parses a query into the JSON value that serde deserializes into the
/// search input type.
///
/// `entries` are the query's pairs in order, already decoded as
/// `application/x-www-form-urlencoded`. A key may repeat. The result equals
/// what the client's `parseSearch` produces for the same shape and entries. An
/// integral number is an integer JSON number so that it
/// deserializes into Rust integer types.
#[cfg(feature = "__internal")]
pub fn parse_search_params(
	shape: &SearchShape,
	entries: &[(String, String)],
) -> Result<Value, SearchParamsError> {
	QueryIndex::new(entries).parse_search(shape)
}

/// The ID of a key prefix in a [`QueryIndex`]: the root prefix, which is the
/// empty key, or a prefix that one or more segments of a query key spell.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct PrefixId(usize);

const ROOT_PREFIX: PrefixId = PrefixId(0);

/// A query's values by exact key, and the distinct child segments under each
/// key prefix in the order of first appearance.
///
/// Each prefix has an integer ID, and the index finds a prefix's child by the
/// prefix's ID and the child's segment, so indexing a key hashes each of its
/// segments once and never a whole prefix. Building the index takes time and
/// memory in proportion to the length of the query. One index serves every
/// search shape that parses the query.
pub(crate) struct QueryIndex<'a> {
	values: HashMap<&'a str, Vec<&'a str>>,
	/// The ID of each prefix other than the root, by its parent's ID and its
	/// last segment.
	prefix_ids: HashMap<(PrefixId, &'a str), PrefixId>,
	/// The distinct child segments of each prefix, with their IDs, by the
	/// prefix's ID.
	children: Vec<Vec<(&'a str, PrefixId)>>,
}

/// An encoded key that a parse visits, and the ID of the prefix that it
/// spells when a query key starts with it.
#[derive(Clone, Copy)]
struct KeyPosition<'p> {
	path: &'p str,
	prefix: Option<PrefixId>,
}

impl<'a> QueryIndex<'a> {
	pub(crate) fn new(entries: &'a [(String, String)]) -> Self {
		let mut values = HashMap::<&str, Vec<&str>>::new();
		let mut prefix_ids = HashMap::new();
		let mut children = vec![Vec::new()];
		for (key, value) in entries {
			if let Some(existing) = values.get_mut(key.as_str()) {
				existing.push(value);
				continue;
			}
			values.insert(key, vec![value]);
			if key.is_empty() {
				continue;
			}
			// A leading empty segment is a child of the root with its own ID,
			// so its children are not the root's.
			let mut parent = ROOT_PREFIX;
			for segment in key.split(KEY_SEPARATOR) {
				parent = *prefix_ids.entry((parent, segment)).or_insert_with(|| {
					let child = PrefixId(children.len());
					children.push(Vec::new());
					children[parent.0].push((segment, child));
					child
				});
			}
		}
		Self {
			values,
			prefix_ids,
			children,
		}
	}

	/// Parses the query into the JSON value of a search shape, as
	/// [`parse_search_params`] does.
	pub(crate) fn parse_search(&self, shape: &SearchShape) -> Result<Value, SearchParamsError> {
		self.parse(
			shape,
			KeyPosition {
				path: "",
				prefix: Some(ROOT_PREFIX),
			},
		)
	}

	fn parse(&self, shape: &SearchShape, key: KeyPosition<'_>) -> Result<Value, SearchParamsError> {
		let path = key.path;
		if shape.is_nullable() {
			if !self.is_present(shape, key) {
				return Ok(Value::Null);
			}
			let exact = self.values_at(path);
			if let SearchShape::Scalar(_) = shape {
				if exact.first() == Some(&NULL_MARKER) {
					return Ok(Value::Null);
				}
			} else if exact.contains(&NULL_MARKER) {
				return if exact.len() == 1 && !self.has_children(key) {
					Ok(Value::Null)
				} else {
					Err(invalid_error(path, Expected("an unambiguous null marker")))
				};
			}
		}
		match shape {
			SearchShape::Scalar(scalar) => {
				let Some(&raw) = self.values_at(path).first() else {
					return Err(missing_error(path));
				};
				// Nothing entered is the empty string only for a string.
				if raw.is_empty() && scalar.kind != SearchScalarKind::String {
					return Err(missing_error(path));
				}
				parse_scalar(scalar, EnteredText { raw, path })
			}
			SearchShape::Object { fields, .. } => self.parse_object(fields, key),
			SearchShape::Array { item, .. } => self.parse_array(item, path),
			SearchShape::Record {
				permitted_keys,
				value,
				..
			} => self.parse_record(permitted_keys, value, key),
		}
	}

	fn parse_object(
		&self,
		fields: &[SearchField],
		key: KeyPosition<'_>,
	) -> Result<Value, SearchParamsError> {
		let entered = self.entered_values(key.path);
		if !entered.is_empty() && (entered != [EMPTY_OBJECT_MARKER] || self.has_children(key)) {
			return Err(invalid_error(
				key.path,
				Expected("an unambiguous empty-object marker or child fields"),
			));
		}
		let mut object = Map::new();
		for field in fields {
			let segment = encode_key_segment(FieldName(&field.name));
			let field_path = join_key(key.path, &segment);
			let field_key = KeyPosition {
				path: &field_path,
				prefix: self.find_child(key, &segment),
			};
			if field.optional && !self.is_present(&field.shape, field_key) {
				continue;
			}
			object.insert(field.name.clone(), self.parse(&field.shape, field_key)?);
		}
		Ok(Value::Object(object))
	}

	fn parse_array(&self, item: &SearchScalar, path: &str) -> Result<Value, SearchParamsError> {
		let entered = self.entered_values(path);
		if entered.is_empty() || entered == [EMPTY_ARRAY_MARKER] {
			return Ok(Value::Array(Vec::new()));
		}
		if entered.contains(&EMPTY_ARRAY_MARKER) {
			return Err(invalid_error(
				path,
				Expected("an array without a mixed empty-array marker"),
			));
		}
		entered
			.into_iter()
			.map(|raw| parse_scalar(item, EnteredText { raw, path }))
			.collect::<Result<_, _>>()
			.map(Value::Array)
	}

	fn parse_record(
		&self,
		permitted_keys: &PermittedStrings,
		value: &SearchShape,
		key: KeyPosition<'_>,
	) -> Result<Value, SearchParamsError> {
		let path = key.path;
		let entered = self.entered_values(path);
		if entered == [EMPTY_OBJECT_MARKER] && !self.has_children(key) {
			return Ok(Value::Object(Map::new()));
		}
		if !entered.is_empty() {
			return Err(invalid_error(
				path,
				Expected("an empty-object marker or dotted map entries"),
			));
		}
		let mut record = Map::new();
		for &(encoded_key, prefix) in key.prefix.map_or(&[][..], |id| &self.children[id.0]) {
			if encoded_key.is_empty() {
				continue;
			}
			let decoded_key = decode_key_segment(EnteredText {
				raw: encoded_key,
				path,
			})?;
			if matches!(permitted_keys, PermittedStrings::OneOf(keys) if !keys.contains(&decoded_key))
			{
				return Err(invalid_error(path, Expected("a declared enum variant")));
			}
			let entry_path = join_key(path, encoded_key);
			let entry_key = KeyPosition {
				path: &entry_path,
				prefix: Some(prefix),
			};
			if self.is_entry_empty(value, entry_key) {
				continue;
			}
			record.insert(decoded_key, self.parse(value, entry_key)?);
		}
		Ok(Value::Object(record))
	}

	fn values_at(&self, path: &str) -> &[&'a str] {
		self.values.get(path).map_or(&[], Vec::as_slice)
	}

	/// The ID of the prefix that `segment` adds to `parent`, when a query key
	/// starts with that prefix.
	fn find_child(&self, parent: KeyPosition<'_>, segment: &str) -> Option<PrefixId> {
		let parent = parent.prefix?;
		self.prefix_ids.get(&(parent, segment)).copied()
	}

	/// The values at `path` other than plain empty ones, which say that
	/// nothing was entered.
	fn entered_values(&self, path: &str) -> Vec<&'a str> {
		self.values_at(path)
			.iter()
			.copied()
			.filter(|value| !value.is_empty())
			.collect()
	}

	fn has_children(&self, key: KeyPosition<'_>) -> bool {
		key.prefix
			.is_some_and(|prefix| !self.children[prefix.0].is_empty())
	}

	/// Whether the query has keys under `key` that belong to `shape`. A
	/// scalar has no child keys, so a key below a scalar's key is not the
	/// scalar's.
	fn has_child_keys(&self, shape: &SearchShape, key: KeyPosition<'_>) -> bool {
		!matches!(shape, SearchShape::Scalar(_)) && self.has_children(key)
	}

	fn is_present(&self, shape: &SearchShape, key: KeyPosition<'_>) -> bool {
		(self.values.contains_key(key.path) || self.has_child_keys(shape, key))
			&& !self.is_entry_empty(shape, key)
	}

	/// Whether the keys at `key` say only that nothing was entered: a scalar
	/// with a plain empty first value, or another shape with only plain empty
	/// values, without child keys.
	fn is_entry_empty(&self, shape: &SearchShape, key: KeyPosition<'_>) -> bool {
		if self.has_child_keys(shape, key) {
			return false;
		}
		let values = self.values_at(key.path);
		match shape {
			SearchShape::Scalar(_) => values.first() == Some(&""),
			_ => !values.is_empty() && values.iter().all(|value| value.is_empty()),
		}
	}
}

fn parse_scalar(
	scalar: &SearchScalar,
	entered: EnteredText<'_>,
) -> Result<Value, SearchParamsError> {
	let EnteredText { raw, path } = entered;
	if raw == NULL_MARKER && scalar.nullable {
		return Ok(Value::Null);
	}
	let text = decode_scalar(EnteredText { raw, path })?;
	match scalar.kind {
		SearchScalarKind::String => match &scalar.permitted_strings {
			PermittedStrings::OneOf(strings) if !strings.iter().any(|string| string == text) => {
				Err(invalid_error(path, Expected("a declared enum variant")))
			}
			_ => Ok(Value::String(text.to_owned())),
		},
		// The serializer writes true and false. Forms and people also write 1
		// and 0, and a checked checkbox without a value attribute sends on.
		SearchScalarKind::Boolean => match text.to_lowercase().as_str() {
			"true" | "1" | "on" => Ok(Value::Bool(true)),
			"false" | "0" => Ok(Value::Bool(false)),
			_ => Err(invalid_error(path, Expected("true, false, 1, 0, or on"))),
		},
		SearchScalarKind::Number => parse_number(EnteredText { raw: text, path }),
	}
}

/// Parses the decimal syntax `[+-]?(\d+(\.\d*)?|\.\d+)([eE][+-]?\d+)?`. An
/// integral result is an integer JSON number.
fn parse_number(entered: EnteredText<'_>) -> Result<Value, SearchParamsError> {
	let EnteredText { raw: text, path } = entered;
	let number = is_decimal_number(text)
		.then(|| text.parse::<f64>().ok())
		.flatten()
		.filter(|number| number.is_finite())
		.ok_or_else(|| invalid_error(path, Expected("a finite decimal number")))?;
	let integer_range = i64::MIN as f64..-(i64::MIN as f64);
	Ok(
		if number.fract() == 0.0 && integer_range.contains(&number) {
			Value::from(number as i64)
		} else {
			Value::from(number)
		},
	)
}

fn is_decimal_number(text: &str) -> bool {
	let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
	let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
	let (mantissa, exponent) = match unsigned.split_once(['e', 'E']) {
		Some((mantissa, exponent)) => (mantissa, Some(exponent)),
		None => (unsigned, None),
	};
	let mantissa_is_valid = match mantissa.split_once('.') {
		Some((whole, fraction)) => {
			(digits(whole) && (fraction.is_empty() || digits(fraction)))
				|| (whole.is_empty() && digits(fraction))
		}
		None => digits(mantissa),
	};
	mantissa_is_valid
		&& exponent
			.is_none_or(|exponent| digits(exponent.strip_prefix(['+', '-']).unwrap_or(exponent)))
}

fn decode_key_segment(entered: EnteredText<'_>) -> Result<String, SearchParamsError> {
	let EnteredText { raw: encoded, path } = entered;
	if encoded == EMPTY_KEY_SEGMENT {
		return Ok(String::new());
	}
	let mut decoded = String::with_capacity(encoded.len());
	let mut characters = encoded.chars();
	while let Some(character) = characters.next() {
		if character != ESCAPE_PREFIX {
			decoded.push(character);
			continue;
		}
		match characters.next() {
			Some('0') => decoded.push(ESCAPE_PREFIX),
			Some('1') => decoded.push(KEY_SEPARATOR),
			_ => {
				return Err(invalid_error(path, Expected("a well-formed key escape")));
			}
		}
	}
	Ok(decoded)
}

fn decode_scalar(entered: EnteredText<'_>) -> Result<&str, SearchParamsError> {
	let EnteredText { raw, path } = entered;
	if let Some(escaped) = raw.strip_prefix(ESCAPED_SCALAR_PREFIX) {
		// The escape alone is the empty string.
		return if escaped.is_empty() || escaped.starts_with(ESCAPE_PREFIX) {
			Ok(escaped)
		} else {
			Err(invalid_error(
				path,
				Expected("a well-formed escaped scalar"),
			))
		};
	}
	if raw.starts_with(ESCAPE_PREFIX) {
		Err(invalid_error(
			path,
			Expected("a scalar, not a reserved marker"),
		))
	} else {
		Ok(raw)
	}
}

fn missing_error(path: &str) -> SearchParamsError {
	SearchParamsError::Missing {
		path: path.to_owned(),
	}
}

fn invalid_error(path: &str, expected: Expected) -> SearchParamsError {
	SearchParamsError::Invalid {
		path: path.to_owned(),
		expected: expected.0,
	}
}

/// A search value that the grammar cannot spell. The client's
/// `serializeSearch` refuses the same values with the error
/// `unsupported-search-value`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SearchSerializeError {
	/// An array holds an array, even an empty one. An array repeats its key
	/// once for each item, so an item has no key of its own for its own items.
	ArrayInArray {
		/// The encoded key of the outer array.
		path: String,
	},
	/// An array holds an object, even an empty one. An array repeats its key
	/// once for each item, so an item has no key of its own for its fields.
	ObjectInArray {
		/// The encoded key of the array.
		path: String,
	},
}

impl fmt::Display for SearchSerializeError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let (path, item) = match self {
			Self::ArrayInArray { path } => (path, "an array"),
			Self::ObjectInArray { path } => (path, "an object"),
		};
		write!(
			f,
			"the array under the search key {path:?} holds {item}, but an array can hold only scalars and null"
		)
	}
}

impl Error for SearchSerializeError {}

/// Spells a search value as query entries, in the order that the client's
/// `serializeSearch` writes them for the same JSON value.
///
/// Object fields are written in UTF-16 code unit order of their names, each
/// array item under the array's key, and empty containers and null as their
/// markers. Form-urlencoding the entries produces the TypeScript function's
/// query text.
///
/// # Errors
///
/// An array that holds an array or an object has no spelling, because no
/// search shape reads it back: [`SearchSerializeError`] names the array's key.
/// Every other JSON value has a spelling.
pub fn serialize_search_params(
	value: &Value,
) -> Result<Vec<(String, String)>, SearchSerializeError> {
	let mut entries = Vec::new();
	append_value(&mut entries, "", value)?;
	Ok(entries)
}

fn append_value(
	entries: &mut Vec<(String, String)>,
	key: &str,
	value: &Value,
) -> Result<(), SearchSerializeError> {
	let text = match value {
		Value::Null => NULL_MARKER.to_owned(),
		Value::Array(items) if items.is_empty() => EMPTY_ARRAY_MARKER.to_owned(),
		Value::Object(fields) if fields.is_empty() => EMPTY_OBJECT_MARKER.to_owned(),
		Value::Array(items) => {
			for item in items {
				match item {
					Value::Array(_) => {
						return Err(SearchSerializeError::ArrayInArray {
							path: key.to_owned(),
						});
					}
					Value::Object(_) => {
						return Err(SearchSerializeError::ObjectInArray {
							path: key.to_owned(),
						});
					}
					Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
						append_value(entries, key, item)?;
					}
				}
			}
			return Ok(());
		}
		Value::Object(fields) => {
			let mut fields = fields.iter().collect::<Vec<_>>();
			fields.sort_by(|(left, _), (right, _)| left.encode_utf16().cmp(right.encode_utf16()));
			for (name, child) in fields {
				append_value(entries, &append_key(key, FieldName(name)), child)?;
			}
			return Ok(());
		}
		Value::Bool(boolean) => boolean.to_string(),
		Value::Number(number) => format_number(number),
		Value::String(text) if text.is_empty() => ESCAPED_SCALAR_PREFIX.to_owned(),
		Value::String(text) if text.starts_with(ESCAPE_PREFIX) => {
			format!("{ESCAPED_SCALAR_PREFIX}{text}")
		}
		Value::String(text) => text.clone(),
	};
	entries.push((key.to_owned(), text));
	Ok(())
}

/// The name of an object field, which becomes one segment of a key.
struct FieldName<'a>(&'a str);

/// The key segment that spells a field name.
fn encode_key_segment(field_name: FieldName<'_>) -> String {
	let name = field_name.0;
	if name.is_empty() {
		EMPTY_KEY_SEGMENT.to_owned()
	} else {
		name.replace(ESCAPE_PREFIX, ESCAPED_TILDE)
			.replace(KEY_SEPARATOR, ESCAPED_KEY_SEPARATOR)
	}
}

/// The key of an encoded segment below the key `parent`.
fn join_key(parent: &str, encoded_segment: &str) -> String {
	if parent.is_empty() {
		encoded_segment.to_owned()
	} else {
		format!("{parent}{KEY_SEPARATOR}{encoded_segment}")
	}
}

fn append_key(parent: &str, field_name: FieldName<'_>) -> String {
	join_key(parent, &encode_key_segment(field_name))
}

/// Spells a number as JavaScript's `String(number)` does.
fn format_number(number: &Number) -> String {
	let Some(float) = number.as_f64().filter(|_| number.is_f64()) else {
		return number.to_string();
	};
	if float == 0.0 {
		return "0".to_owned();
	}
	let magnitude = float.abs();
	let shortest = format!("{magnitude:e}");
	let (shortest_digits, _) = split_scientific(&shortest);
	// JavaScript uses the fewest digits that read back as the same number and,
	// of those, the spelling nearest to it, breaking a tie toward an even last
	// digit. Rust's shortest spelling breaks such a tie upward, and its
	// fixed-precision spelling rounds the exact value half to even.
	let nearest = format!("{magnitude:.*e}", shortest_digits.len() - 1);
	let (digits, exponent) = if nearest.parse() == Ok(magnitude) {
		split_scientific(&nearest)
	} else {
		split_scientific(&shortest)
	};
	let sign = if float < 0.0 { "-" } else { "" };
	let digit_count = digits.len() as i32;
	// The decimal point follows this many digits.
	let point = exponent + 1;
	if (digit_count..=21).contains(&point) {
		let zeros = "0".repeat((point - digit_count) as usize);
		format!("{sign}{digits}{zeros}")
	} else if (1..=21).contains(&point) {
		let (whole, fraction) = digits.split_at(point as usize);
		format!("{sign}{whole}.{fraction}")
	} else if (-5..=0).contains(&point) {
		let zeros = "0".repeat(-point as usize);
		format!("{sign}0.{zeros}{digits}")
	} else {
		let (first, rest) = digits.split_at(1);
		let separator = if rest.is_empty() { "" } else { "." };
		format!("{sign}{first}{separator}{rest}e{exponent:+}")
	}
}

/// The significant digits and the exponent of Rust's scientific notation.
fn split_scientific(text: &str) -> (String, i32) {
	let (mantissa, exponent) = text
		.split_once('e')
		.expect("scientific notation has an exponent");
	let exponent = exponent
		.parse()
		.expect("scientific notation has an integer exponent");
	(mantissa.replace('.', ""), exponent)
}
