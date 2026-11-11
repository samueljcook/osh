//! Form endpoint contracts and generated browser input types.

use bytes::Bytes;
use futures_util::stream::BoxStream;
use primitives_rs::tsgen::{self, TsGen, TypeDef, TypeGraph, TypePhase, TypeRef, TypeRegistry};
use std::error::Error as StdError;

pub use primitives_rs::http::form_data::UploadedFile;

/// A request body as a stream of chunks.
#[doc(hidden)]
pub type FormBody = BoxStream<'static, Result<Bytes, Box<dyn StdError + Send + Sync>>>;

/// Endpoint input read from the request body as a form and deserialized into `Fields`.
///
/// The request's `Content-Type` must be `multipart/form-data` or
/// `application/x-www-form-urlencoded`; any other, or none, gets a 415
/// failure, and the handler does not run. Duplicate content-type headers,
/// malformed form parameters, and missing or invalid multipart boundaries
/// fail with status 400. The configured body-size limit includes multipart
/// headers, boundaries, and trailing bytes. A
/// multipart part with a `filename` or `filename*` parameter is an
/// [`UploadedFile`]; every other part is text. Parts map onto `Fields` by name:
///
/// - A field takes the single part of its name. Several parts are an error.
/// - An `Option<_>` field admits an absent part. A present part is `Some`,
///   even when its text is empty.
/// - A `Vec<_>` field takes every part of its name in request order. An absent
///   repeated part needs `#[serde(default)]`.
/// - Text targeted at an integer, float, `bool`, or `char` parses with that
///   type's `FromStr`, so a `bool` accepts exactly `true` or `false`. Text also
///   names a unit enum variant, and a newtype struct reads its inner type.
/// - An [`UploadedFile`] field requires a file part, and any other field
///   requires text.
/// - Unknown parts are ignored unless `Fields` denies unknown fields.
///
/// `Fields` derives [`TsGen`], and a client call to the endpoint takes
/// `formInput`, which is a `FormData` or an object with the TypeScript shape of
/// the Rust `Fields`: [`UploadedFile`] is a `Blob`, an `Option<_>` field is
/// optional, and a `Vec<_>` field is an array. `Fields` must be a struct with named
/// fields, and each field must be a scalar, a unit enum, a newtype of one, an
/// [`UploadedFile`], an `Option<_>` of one of these or of a `Vec<_>`, or a
/// `Vec<_>` of one of these or of an `Option<_>`. A field of another shape,
/// such as a nested struct, a map, a tuple, or a flattened struct, is an error
/// in the declaration contract, which fails the build.
///
/// Input that does not fit `Fields` fails the request with status 400. A body that
/// cannot be read fails it with status 413 when a body limit stopped it and
/// with status 400 otherwise.
#[derive(Clone, Debug)]
pub struct FormInput<Fields>(pub Fields);

impl<Fields: TsGen> TsGen for FormInput<Fields> {
	fn type_ref() -> TypeRef {
		Fields::type_ref()
	}

	fn collect_type_defs(registry: &mut TypeRegistry) -> tsgen::Result<()> {
		collect_form_type_defs::<Fields>(TypePhase::Deserialize, registry)
	}

	fn type_ref_for(phase: TypePhase) -> TypeRef {
		Fields::type_ref_for(phase)
	}

	fn collect_type_defs_for(phase: TypePhase, registry: &mut TypeRegistry) -> tsgen::Result<()> {
		collect_form_type_defs::<Fields>(phase, registry)
	}
}

/// Collects `Fields`'s definitions into `registry` after checking that the form
/// decoder can read every field of `Fields`.
fn collect_form_type_defs<Fields: TsGen>(
	phase: TypePhase,
	registry: &mut TypeRegistry,
) -> tsgen::Result<()> {
	let mut collected = TypeRegistry::default();
	Fields::collect_type_defs_for(phase, &mut collected)?;
	let graph = TypeGraph::new(Fields::type_ref_for(phase), collected.clone())?;
	check_form_fields(&graph.root, &graph.type_defs).map_err(tsgen::Error::new)?;
	let defs = collected.into_defs();
	for def in defs {
		registry.try_define(def)?;
	}
	Ok(())
}

/// The definition that `type_ref` stands for, following named references
/// (a newtype of a named type refers to another definition).
fn resolve_form_type<'a>(mut type_ref: &'a TypeRef, defs: &'a [TypeDef]) -> &'a TypeRef {
	for _ in 0..=defs.len() {
		let TypeRef::Named { key, .. } = type_ref else {
			break;
		};
		match defs.iter().find(|def| def.key() == key) {
			Some(def) => type_ref = def.type_ref(),
			None => break,
		}
	}
	type_ref
}

/// Checks that the form decoder can read an input type of this shape: a
/// struct with fields that the decoder can read.
fn check_form_fields(type_ref: &TypeRef, defs: &[TypeDef]) -> Result<(), String> {
	let TypeRef::Object(fields) = resolve_form_type(type_ref, defs) else {
		return Err(
			"form input must be a struct with named fields, without flattened fields".to_owned(),
		);
	};
	for field in fields {
		check_form_field(field.type_ref(), true, defs).map_err(|problem| {
			format!(
				"the form field `{}` is {problem}, which form input cannot read",
				field.name()
			)
		})?;
	}
	Ok(())
}

/// Checks one field's type. A field takes several parts only when it is a
/// sequence at its top (`repeated`), so a sequence inside a sequence is not
/// readable.
fn check_form_field(type_ref: &TypeRef, repeated: bool, defs: &[TypeDef]) -> Result<(), String> {
	let described = match resolve_form_type(type_ref, defs) {
		TypeRef::Bool
		| TypeRef::String
		| TypeRef::Number
		| TypeRef::Integer
		| TypeRef::WideInteger { .. }
		| TypeRef::Blob
		| TypeRef::StringLiteral(_) => return Ok(()),
		TypeRef::Nullable(inner) => return check_form_field(inner, repeated, defs),
		TypeRef::Array(inner) if repeated => return check_form_field(inner, false, defs),
		TypeRef::Union(members)
			if !members.is_empty()
				&& members
					.iter()
					.all(|member| matches!(member, TypeRef::StringLiteral(_))) =>
		{
			return Ok(());
		}
		TypeRef::Array(_) => "a sequence inside a sequence",
		TypeRef::Object(_) => "a nested struct",
		TypeRef::Record(..) => "a map",
		TypeRef::Tuple(_) => "a tuple",
		TypeRef::Intersection(_) => "a flattened struct",
		TypeRef::Union(_) => "an enum with data",
		TypeRef::Undefined | TypeRef::Null => "an empty value",
		TypeRef::Unknown | TypeRef::Raw(_) => "a value of unknown shape",
		TypeRef::FormData => "a form",
		TypeRef::Parameter(_) | TypeRef::Documented { .. } => {
			unreachable!("TypeGraph contains concrete undocumented expressions")
		}
		TypeRef::Named { .. } => "a type that the declarations do not define",
	};
	Err(described.to_owned())
}
