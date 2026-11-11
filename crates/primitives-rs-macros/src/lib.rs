//! `primitives-rs` procedural macros.
//!
//! Application code should use `primitives_rs::tsgen::TsGen`; this crate is an
//! implementation detail.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]
#![forbid(unsafe_code)]

use proc_macro::TokenStream;
use syn::parse_macro_input;

/// Derive the TypeScript generation trait for a serializable Rust type.
///
/// Generates a `primitives_rs::tsgen::TsGen` implementation from the type's
/// shape and the `serde` attributes that `#[derive(Serialize)]` and
/// `#[derive(Deserialize)]` read so that the generated TypeScript shape follows
/// the type's `serde_json` representation without a separately maintained
/// description. The serialize-phase shape describes the values that
/// `serde_json` writes, and the deserialize-phase shape describes values
/// that `serde_json` accepts, with one exception: an `f32` or `f64` value is
/// declared as `number`, which describes only finite values because
/// `serde_json` writes `null` for NaN and for infinity.
///
/// The generated code refers to the runtime through the path
/// `::primitives_rs::tsgen`, so the crate that uses the derive macro must depend on
/// `primitives-rs` under its own name, `primitives_rs`, and not under a
/// renamed dependency key. The generated code names standard library items by
/// their full paths, so an item that the deriving crate imports or defines
/// with the name of a standard item, such as `Ok`, does not change the
/// meaning of the generated code.
///
/// # Structs
///
/// - **Named-field structs** become object types. Every field that is not
///   skipped becomes one property. The property uses the field's Rust name
///   unless `#[serde(rename = "...")]` or a container-level
///   `#[serde(rename_all = "...")]` overrides it. `rename_all` accepts the
///   same casing rules as `serde`: `lowercase`, `UPPERCASE`, `PascalCase`,
///   `camelCase`, `snake_case`, `SCREAMING_SNAKE_CASE`, `kebab-case`, and
///   `SCREAMING-KEBAB-CASE`. The derive macro also supports the phase-specific
///   `#[serde(rename(serialize = "...", deserialize = "..."))]` and
///   `#[serde(rename_all(serialize = "...", deserialize = "..."))]` forms,
///   so the two phases can use different names for the same field.
///   `#[serde(skip)]` drops a field from both phases, and
///   `#[serde(skip_serializing)]` or `#[serde(skip_deserializing)]` drops it
///   from one phase. The derive macro accepts `#[serde(deny_unknown_fields)]`,
///   which makes decoding stricter without changing the generated object's
///   fields, so the declared type also describes objects with other
///   properties, which the decoder rejects.
/// - **Newtype structs**, such as `struct Id(String)`, and
///   `#[serde(transparent)]` structs with exactly one field, named or
///   unnamed, become the TypeScript type of that field because `serde_json`
///   writes the struct as its field.
/// - **Tuple structs** with zero or at least two fields become tuple types,
///   such as `[number, string]`.
/// - **Unit structs** become `null`.
///
/// The TypeScript name of a named type is its Rust identifier. A
/// container-level `#[serde(rename = "...")]` does not change that name
/// because `serde_json` does not write a struct's or an enum's name. A
/// derived type therefore cannot be rendered when its Rust identifier is a
/// TypeScript reserved word or `Array`, `Record`, `Blob`, or `FormData`,
/// which the generated references use for platform types, and such a type
/// must be renamed.
///
/// # Enums
///
/// An enum becomes a union with one member for each variant that is not
/// skipped. A container-level `rename_all` rule applies to variant names,
/// and variants support `rename`, `skip`, `skip_serializing`, and
/// `skip_deserializing` in the same forms as struct fields. The fields of a
/// struct variant use the variant's `#[serde(rename_all = "...")]` rule, or
/// else the container's `#[serde(rename_all_fields = "...")]` rule, and they
/// support the same attributes as a named-field struct's fields. Each
/// representation that `serde` supports renders as follows, where `V` is the
/// variant's name, `T` is a newtype variant's field type, `[A, B]` lists a
/// tuple variant's field types, and `x: X` lists a struct variant's fields:
///
/// | Representation                       | Unit         | Newtype            | Tuple                   | Struct                     |
/// | ------------------------------------ | ------------ | ------------------ | ----------------------- | -------------------------- |
/// | External, `serde`'s default          | `"V"`        | `{ V: T }`         | `{ V: [A, B] }`         | `{ V: { x: X } }`          |
/// | `#[serde(tag = "t")]`                | `{ t: "V" }` | `{ t: "V" } & T`   | Rejected, as in `serde` | `{ t: "V"; x: X }`         |
/// | `#[serde(tag = "t", content = "c")]` | `{ t: "V" }` | `{ t: "V"; c: T }` | `{ t: "V"; c: [A, B] }` | `{ t: "V"; c: { x: X } }`  |
/// | `#[serde(untagged)]`                 | `null`       | `T`                | `[A, B]`                | `{ x: X }`                 |
///
/// An enum with only unit variants and the external representation
/// therefore becomes a union of string literals. A newtype variant of an
/// internally tagged enum must contain a type that `serde` writes as an
/// object, such as a struct, given that `serde` fails at run time to serialize
/// any other content in that position. For such a variant,
/// `primitives_rs::tsgen::TsGen::collect_type_defs_for` returns an error
/// when the content's type, apart from `Option`, is `null`, such as `()`,
/// or is a map with values other than `serde_json::Value` because
/// TypeScript cannot intersect either one with the tag's object. The check
/// reads only the content's own type expression and does not follow a named
/// type, such as a derived unit struct, to its definition.
///
/// # Generic types
///
/// A type such as `Page<T>` produces a named `Page<T>` declaration, and
/// `Page<User>` references it with `User` as its argument. Container docs appear
/// once on the declaration. Nested generic applications and direct or mutual
/// regular recursion preserve their names. Dependency collection visits every
/// concrete instantiation so that all referenced argument types are included.
/// The derive macro substitutes symbolic Rust parameter types when it constructs a
/// generic declaration, so Rust aliases retain their argument ordering and
/// fixed arguments. Symbolic parameters implement `TsGen`, `Clone`, `Copy`,
/// `Debug`, `Default`, `Eq`, `PartialEq`, `Ord`, `PartialOrd`, and `Hash`.
/// A generic field constructor that requires other parameter bounds needs
/// concrete derivation on its containing type.
///
/// Const parameters select separate declarations with explicit value suffixes:
/// `Fixed<T, 4>` with a const parameter named `N` becomes `Fixed_N_4<T>`.
/// Non-alphanumeric bytes in a const value's debug representation use `_xx`
/// hexadecimal escapes in the suffix. Identity keys also include const values.
/// Generic associated-type projections, such as `T::Item`, cannot be described
/// by an unconstrained TypeScript parameter. Use a separate type parameter for
/// the wire value or `#[tsgen(concrete)]` on the containing type. Concrete
/// derivation describes the actual field types and includes the complete Rust
/// instantiation in its identity key. It exports the type's ordinary name, so
/// one module can contain one concrete instantiation under that name; distinct
/// instantiations require manually named definitions. Concrete derivation also
/// supports field constructors with bounds that symbolic parameters cannot meet.
///
/// Enum-variant doc comments appear beside union members for every supported
/// enum representation. TypeScript exposes named-type and field documentation
/// to editors; string-literal union members have no symbol for individual hover
/// documentation. The generated source still retains their comments.
///
/// The derive macro implements `TsGen` for a type with lifetime parameters with
/// each lifetime parameter set to `'static` because `TsGen` requires
/// `'static`. For example, `Borrowed<'static>` implements `TsGen`. A type
/// with only lifetime parameters is a named type.
///
/// # Flattened fields
///
/// A struct or struct variant with `#[serde(flatten)]` fields becomes an
/// intersection of an object with its other fields and each flattened
/// field's type, such as `{ post_count: number } & User`. A flattened
/// `Option<T>` field contributes `T | Record<never, never>` because `serde`
/// writes no properties for `None`. A flattened map must have
/// `serde_json::Value` values, which render as `Record<string, unknown>`.
/// TypeScript requires every property of an intersection with
/// `Record<string, V>` to have type `V`, including the struct's other
/// fields, so for a flattened map with other values,
/// `primitives_rs::tsgen::TsGen::collect_type_defs_for` returns an error. A
/// flattened field cannot have `rename`, `skip_serializing_if`, or
/// `default`.
///
/// # Field optionality
///
/// The derive macro decides each field's TypeScript optionality (`name?: T` or
/// `name: T`) separately for each phase. In the serialize phase, a field is
/// optional when it has `#[serde(skip_serializing_if = "...")]`. The derive macro
/// does not evaluate the predicate because any field with a predicate can
/// be omitted from serialized output. The derive macro reads one predicate by its
/// path: for a field that is written as `Option<T>` with a predicate path
/// that ends in `Option::is_none`, `serde` writes the field only for `Some`,
/// so the field's serialize-phase type is the type of `T`, such as
/// `name?: string`. With any other predicate, the field's type is the type
/// of the whole `Option<T>`, such as `name?: string | null`. In the
/// deserialize phase, a field is
/// optional when its type is `Option<T>`, when it has `#[serde(default)]`,
/// or when its struct has `#[serde(default)]`.
///
/// # A named type in both phases
///
/// The derive macro registers a named type's serialize-phase and
/// deserialize-phase `primitives_rs::tsgen::TypeDef`s under two keys,
/// `{module_path}::{TypeIdent}::serialize` and
/// `{module_path}::{TypeIdent}::deserialize`, even when the two phases
/// render identically. One derived type can be an input in one place and an
/// output in another, such as a `User` type that one handler accepts and
/// another handler returns. A consumer that emits one TypeScript type for
/// both phases can merge the two definitions with
/// `primitives_rs::tsgen::TypeDef::merge_shared_name_with`. The merged type
/// makes a field optional when the serialize phase always writes the field
/// and the deserialize phase accepts a value without it, such as an
/// `Option<T>` field or a `#[serde(default)]` field without
/// `skip_serializing_if`. A `skip_serializing_if` field that the deserialize
/// phase requires because its type is not `Option<T>` and neither the field
/// nor its struct has `#[serde(default)]` prevents merging, and adding
/// `#[serde(default)]` to the field resolves that conflict. A field with
/// the type of `T` in the serialize phase and `T | null` in the deserialize
/// phase, which the predicate `Option::is_none` gives, has the type
/// `T | null` in the merged type. A field or
/// variant with a different name or presence in each phase also prevents
/// merging.
///
/// # Rejected shapes and attributes
///
/// The derive macro reports a compile error instead of guessing a TypeScript
/// shape. The errors for the following shapes and attributes recommend a
/// manual `primitives_rs::tsgen::TsGen` implementation:
///
/// - Rust unions.
/// - `#[serde(serialize_with = "...")]`, `#[serde(deserialize_with =
///   "...")]`, and `#[serde(with = "...")]` because the TypeScript shape of
///   a custom codec does not follow from the Rust type.
/// - `HashMap` and `BTreeMap` fields with non-`String` keys, including maps
///   inside `Option`, `Vec`, `Box`, or a fixed-size array because
///   TypeScript object keys are strings.
/// - `serde` attributes on the fields of tuple structs, newtype structs, and
///   tuple or newtype variants. A `#[serde(transparent)]` struct's field
///   accepts the attributes of a named-field struct's fields.
/// - A field with a generic associated-type projection.
/// - `#[serde(tag = "...")]` on a struct.
///
/// The derive macro accepts `#[tsgen(concrete)]` only on the type that derives it,
/// and it rejects `#[tsgen(...)]` on a field or an enum variant.
///
/// The derive macro also rejects the combinations that `serde` rejects and that
/// leave no TypeScript shape to generate: a tuple variant in an internally
/// tagged enum, `#[serde(content = "...")]` without `#[serde(tag =
/// "...")]`, an untagged enum with a tag or content, a tag and content with
/// the same name, `#[serde(untagged)]`, `#[serde(content = "...")]`, or
/// `#[serde(rename_all_fields = "...")]` on a struct, and
/// `#[serde(default)]` or `#[serde(transparent)]` on an enum. It also
/// rejects a `#[serde(transparent)]` struct without exactly one field, a
/// `rename_all` casing rule that is not listed above, and any other `serde`
/// attribute that this documentation does not list as supported, such as
/// `alias`, `other`, and `untagged` on a variant.
#[proc_macro_derive(TsGen, attributes(serde, tsgen))]
pub fn derive_ts_gen(input: TokenStream) -> TokenStream {
	let input = parse_macro_input!(input as syn::DeriveInput);
	primitives_rs_macros_core::expand_ts_gen(input, &quote::quote! { ::primitives_rs::tsgen })
		.unwrap_or_else(syn::Error::into_compile_error)
		.into()
}
