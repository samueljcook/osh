//! Rust-to-TypeScript type generation from `serde` shapes.
//!
//! This module describes Rust types as TypeScript declarations. A type that
//! implements [`TsGen`](crate::tsgen::TsGen) reports the
//! [`TypeRef`](crate::tsgen::TypeRef) that it renders as at a use site and
//! registers the named [`TypeDef`](crate::tsgen::TypeDef)s that it depends
//! on into a [`TypeRegistry`](crate::tsgen::TypeRegistry). The
//! [`TsGen`](crate::tsgen::TsGen) derive macro generates a
//! [`TsGen`](crate::tsgen::TsGen) implementation from a type's shape and its
//! `serde` attributes, and the derive macro's documentation lists the shapes and
//! attributes that it supports. The
//! [`declarations`](crate::tsgen::declarations) module defines the
//! declaration model, and the [`render`](crate::tsgen::render) module
//! renders declarations as TypeScript source text.
//!
//! # Named types and generics
//!
//! Derived types register named declarations in each Serde phase. A generic
//! `Page<T>` registers one `Page<T>` declaration and references it as
//! `Page<User>` at a concrete use site. Container, field, and enum-variant
//! documentation is retained. Direct and mutual regular recursion use named
//! references. Const parameters select distinct declarations, such as
//! `Fixed_N_4<T>`. Generic associated-type projections require `#[tsgen(concrete)]`, a manual
//! implementation, or a separate type parameter for their wire value.
//!
//! # Complete modules
//!
//! [`TypeModule`](crate::tsgen::TypeModule) checks references and export names, orders declarations, and
//! renders exported TypeScript with JSDoc. [`PhaseNaming::Shared`](crate::tsgen::PhaseNaming::Shared) merges
//! compatible input/output definitions and reports incompatible definitions.
//! [`PhaseNaming::Separate`](crate::tsgen::PhaseNaming::Separate) preserves exact phase shapes with explicit name
//! suffixes. [`TypeRegistry::define_alias`](crate::tsgen::TypeRegistry::define_alias) names an arbitrary expression;
//! [`TypeDef::new`](crate::tsgen::TypeDef::new) with [`TypeDef::set_docs`](crate::tsgen::TypeDef::set_docs) defines a documented alias.
//!
//! ```
//! use primitives_rs::tsgen::{TsGen, TypeRegistry, TypeModule, PhaseNaming};
//! #[derive(TsGen)]
//! struct Page<T> { items: Vec<T> }
//! let mut registry = TypeRegistry::default();
//! Page::<String>::collect_type_defs(&mut registry)?;
//! registry.define_alias("app::TextPage", "TextPage", Page::<String>::type_ref())?;
//! let module = TypeModule::new(registry, PhaseNaming::Shared)?;
//! assert!(module.render()?.contains("export type TextPage = Page<string>;"));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! [`TypeGraph`](crate::tsgen::TypeGraph) resolves concrete arguments for consumers that inspect wire
//! shapes instead of generating TypeScript. Standard `PathBuf`, `Path`, and
//! borrowed paths describe strings; Serde remains responsible for rejecting
//! non-UTF-8 paths rather than converting them lossily.
//!
//! # Serialize and deserialize phases
//!
//! One Rust type can have a different TypeScript shape in each
//! [`TypePhase`](crate::tsgen::TypePhase). For example, a
//! `#[serde(skip_serializing_if = "...")]` field can be omitted from
//! serialized output, so it is optional in the serialize-phase shape. A
//! `#[serde(default)]` field can be omitted from deserialized input, so it is
//! optional in the deserialize-phase shape. An `Option<T>` field without
//! `skip_serializing_if` renders as `name: T | null` in the serialize phase
//! and as `name?: T | null` in the deserialize phase. The derive macro registers
//! each phase's definition under its own key, and
//! [`TypeRegistry`](crate::tsgen::TypeRegistry) keeps one definition per
//! key. When a consumer emits one TypeScript declaration per exported name,
//! [`TypeDef::merge_shared_name_with`](crate::tsgen::TypeDef::merge_shared_name_with)
//! merges the two phases of one Rust type into one definition that
//! describes both, or reports the conflict that prevents that.
//!
//! # Manual `TsGen` implementations
//!
//! `#[derive(TsGen)]` supports the struct and enum shapes and the `serde`
//! attributes that the derive macro's documentation lists. A type with a custom
//! `serde` codec, a map with non-string keys, or a Rust `union` needs a
//! hand-written [`TsGen`](crate::tsgen::TsGen) implementation. A hand-written
//! implementation can render a type inline:
//!
//! ```
//! use primitives_rs::tsgen::{Result, TsGen, TypeRef};
//!
//! struct Timestamp(String);
//!
//! impl TsGen for Timestamp {
//!     fn type_ref() -> TypeRef {
//!         TypeRef::String
//!     }
//! }
//!
//! # fn main() -> Result<()> {
//! assert_eq!(Timestamp::type_ref(), TypeRef::String);
//! # Ok(())
//! # }
//! ```
//!
//! A type with its own named shape also implements
//! [`collect_type_defs`](crate::tsgen::TsGen::collect_type_defs) to register a
//! [`TypeDef`](crate::tsgen::TypeDef). Object types are
//! [`TypeRef::Object`](crate::tsgen::TypeRef::Object) values with
//! [`FieldDef`](crate::tsgen::FieldDef)s, and each field has a
//! [`FieldPresence`](crate::tsgen::FieldPresence).
//!
//! The `TsGen` derive preserves Rust doc comments on types, fields, and enum
//! variants in [`TypeDef`](crate::tsgen::TypeDef),
//! [`FieldDef`](crate::tsgen::FieldDef), and documented
//! [`TypeRef`](crate::tsgen::TypeRef) expressions. A hand-written implementation can
//! set documentation text with
//! [`TypeDef::set_docs`](crate::tsgen::TypeDef::set_docs) and
//! [`FieldDef::set_docs`](crate::tsgen::FieldDef::set_docs).

pub mod declarations;
mod integer_string;
mod type_graph;
mod type_module;
pub use type_graph::TypeGraph;
pub mod render;

use std::collections::{BTreeMap, HashMap};
use std::error;
use std::fmt;

pub use primitives_rs_macros::TsGen;

pub use crate::tsgen::declarations::{FieldDef, FieldPresence, RawTsPart, TypeDef, TypeRef};
pub use crate::tsgen::integer_string::{IntegerString, WideInteger};
pub use type_module::{ModuleError, PhaseNaming, TypeModule};

/// TypeScript generation error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Error {
	message: String,
}

impl Error {
	/// Create a new TypeScript generation error.
	pub fn new(message: impl Into<String>) -> Self {
		Self {
			message: message.into(),
		}
	}
}

impl fmt::Display for Error {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.message)
	}
}

impl error::Error for Error {}

/// Result type for TypeScript generation helpers.
pub type Result<T> = std::result::Result<T, Error>;

/// Whether a Rust type is being used for serialized output or deserialized input.
///
/// One Rust type can need a different TypeScript shape in each phase. The
/// serialize-phase shape describes what a producer sends, so a
/// `skip_serializing_if` field is optional in that shape because the
/// producer can omit it. The deserialize-phase shape describes what a decoder
/// accepts, so an `Option<T>` field or a `#[serde(default)]` field is
/// optional in that shape because a caller can omit it.
/// [`TsGen::type_ref_for`] and [`TsGen::collect_type_defs_for`] take the phase
/// as an argument. The `TsGen` derive registers each phase's [`TypeDef`]
/// under its own key, even when the two shapes are identical, so
/// [`TypeRegistry::try_define`] never merges the two phases into one
/// definition. A consumer that combines definitions into one namespace can
/// use [`TypeDef::merge_shared_name_with`] to emit a type that both phases
/// reach as one TypeScript type, and to report an error when one type
/// cannot describe both phases.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TypePhase {
	/// Producer-to-consumer output shape.
	Serialize,
	/// Consumer-to-producer input shape.
	Deserialize,
}

/// Registry of TypeScript definitions collected while resolving types.
///
/// A [`TsGen::collect_type_defs`] or [`TsGen::collect_type_defs_for`] call
/// passes the same registry to each nested type's collection call, so the
/// registry accumulates one flat list of [`TypeDef`]s. [`Self::try_define`]
/// keeps one definition per key, so a nested type that is reached several
/// times contributes its definitions once. The registry accepts two
/// definitions that have different keys and the same [`TypeDef::name`]. A
/// [`TypeModule`] checks names and resolves dependencies before module rendering.
/// Cloning retains definitions and completed collections; active collection
/// scopes belong to the original registry.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct TypeRegistry {
	defs: Vec<TypeDef>,
	/// The position in `defs` of the definition with each key.
	positions: HashMap<String, usize>,
	collections: HashMap<(std::any::TypeId, TypePhase), TypeCollectionState>,
}

impl Clone for TypeRegistry {
	fn clone(&self) -> Self {
		Self {
			defs: self.defs.clone(),
			positions: self.positions.clone(),
			collections: self
				.collections
				.iter()
				.filter_map(|(key, state)| match state {
					TypeCollectionState::Collecting => None,
					TypeCollectionState::Collected => Some((*key, *state)),
				})
				.collect(),
		}
	}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TypeCollectionState {
	Collecting,
	Collected,
}

struct TypeCollectionGuard<'a> {
	registry: &'a mut TypeRegistry,
	key: (std::any::TypeId, TypePhase),
}

impl Drop for TypeCollectionGuard<'_> {
	fn drop(&mut self) {
		if matches!(
			self.registry.collections.get(&self.key),
			Some(TypeCollectionState::Collecting)
		) {
			self.registry.collections.remove(&self.key);
		}
	}
}

impl TypeRegistry {
	/// Collect each Rust instantiation once per phase, terminating recursive visits.
	/// Failed or unwound collections may be retried. Definitions already collected remain available.
	pub fn collect_type<T: TsGen + ?Sized>(
		&mut self,
		phase: TypePhase,
		collect_fn: impl FnOnce(&mut Self) -> Result<()>,
	) -> Result<()> {
		let key = (std::any::TypeId::of::<T>(), phase);
		if self.collections.contains_key(&key) {
			return Ok(());
		}
		self.collections
			.insert(key, TypeCollectionState::Collecting);
		let guard = TypeCollectionGuard {
			registry: self,
			key,
		};
		let result = collect_fn(guard.registry);
		if result.is_ok() {
			guard
				.registry
				.collections
				.insert(key, TypeCollectionState::Collected);
		}
		result
	}

	/// Define a named alias for any expression, including a concrete generic application.
	/// Collect the target's dependencies into this registry before rendering its module.
	pub fn define_alias(
		&mut self,
		key: impl Into<String>,
		name: impl Into<String>,
		target: TypeRef,
	) -> Result<TypeRef> {
		let key = key.into();
		let name = name.into();
		self.try_define(TypeDef::new(&key, &name, target))?;
		Ok(TypeRef::named_with_key(key, name))
	}

	/// Define a type and report whether it was newly inserted.
	///
	/// Returns `Ok(true)` when the registry did not contain a definition with
	/// `def`'s key. Returns `Ok(false)` when the registry contained an
	/// identical definition under that key. Generic instantiations can have
	/// different dependencies despite sharing one declaration; use
	/// [`Self::collect_type`] to deduplicate dependency traversal. Returns [`Error`] when the registry contains a
	/// different definition under that key.
	pub fn try_define(&mut self, def: TypeDef) -> Result<bool> {
		if let Some(&position) = self.positions.get(def.key()) {
			if self.defs[position] == def {
				return Ok(false);
			}
			return Err(Error::new(format!(
				"conflicting TypeScript type definitions for {}",
				def.name()
			)));
		}
		self.positions.insert(def.key().to_owned(), self.defs.len());
		self.defs.push(def);
		Ok(true)
	}

	/// Consume the registry into its collected definitions.
	pub fn into_defs(self) -> Vec<TypeDef> {
		self.defs
	}
}

/// Trait implemented by Rust types that can describe their generated TypeScript shape.
///
/// [`Self::type_ref`] and [`Self::collect_type_defs`] describe a type that
/// has the same shape in both phases. The default [`Self::type_ref_for`] and
/// [`Self::collect_type_defs_for`] ignore the phase and call those two
/// methods, and the primitive implementations in this module implement only
/// those two methods. A type with different serialize-phase and
/// deserialize-phase shapes (see [`TypePhase`]) also implements
/// [`Self::type_ref_for`] and [`Self::collect_type_defs_for`], and the
/// `TsGen` derive macro generates all four methods. A primitive's reference must
/// describe the primitive's serde value wherever the primitive is nested,
/// with one exception: the `number` reference of `f32` and `f64` describes
/// only finite values because `serde_json` writes `null` for NaN and for
/// infinity. Conventions for a consumer's contract boundary, such as
/// omitting an exact top-level `()` input, belong in that consumer.
///
/// A named type can be reached in both phases. For example, one handler can
/// accept a `User` type as input, and another handler can return it as
/// output. The two phases register [`TypeDef`]s under different keys with
/// the same exported name. [`TypeDef::merge_shared_name_with`] merges them
/// into one TypeScript type when one type can describe both phases, for
/// example when an `Option<T>` field or a `#[serde(default)]` field is
/// optional only in the deserialize phase, and reports a conflict when it
/// cannot. A hand-written `TsGen` implementation takes part in merging when
/// it keys its two phases with the convention that the method's
/// documentation describes.
pub trait TsGen: Send + Sync + 'static {
	/// Return this type's default TypeScript reference.
	fn type_ref() -> TypeRef;

	/// Collect any named definitions required by this type.
	fn collect_type_defs(_registry: &mut TypeRegistry) -> Result<()> {
		Ok(())
	}

	/// Return this type's TypeScript reference for a specific input/output phase.
	fn type_ref_for(_phase: TypePhase) -> TypeRef {
		Self::type_ref()
	}

	/// Collect named definitions required by this type for a specific phase.
	fn collect_type_defs_for(phase: TypePhase, registry: &mut TypeRegistry) -> Result<()> {
		let _ = phase;
		Self::collect_type_defs(registry)
	}
}

impl TsGen for () {
	fn type_ref() -> TypeRef {
		TypeRef::Null
	}
}

impl TsGen for bool {
	fn type_ref() -> TypeRef {
		TypeRef::Bool
	}
}

impl TsGen for String {
	fn type_ref() -> TypeRef {
		TypeRef::String
	}
}

impl TsGen for str {
	fn type_ref() -> TypeRef {
		TypeRef::String
	}
}

impl TsGen for std::path::Path {
	fn type_ref() -> TypeRef {
		TypeRef::String
	}
}
impl TsGen for std::path::PathBuf {
	fn type_ref() -> TypeRef {
		TypeRef::String
	}
}
impl<T: TsGen + ?Sized> TsGen for &'static T {
	fn type_ref() -> TypeRef {
		T::type_ref()
	}
	fn type_ref_for(phase: TypePhase) -> TypeRef {
		T::type_ref_for(phase)
	}
	fn collect_type_defs(registry: &mut TypeRegistry) -> Result<()> {
		T::collect_type_defs(registry)
	}
	fn collect_type_defs_for(phase: TypePhase, registry: &mut TypeRegistry) -> Result<()> {
		T::collect_type_defs_for(phase, registry)
	}
}

impl TsGen for serde_json::Value {
	fn type_ref() -> TypeRef {
		TypeRef::Unknown
	}
}

macro_rules! integer_type {
	($($ty:ty),* $(,)?) => {
		$(
			impl TsGen for $ty {
				fn type_ref() -> TypeRef {
					TypeRef::Integer
				}
			}
		)*
	};
}

integer_type!(
	i8,
	i16,
	i32,
	u8,
	u16,
	u32,
	std::num::NonZeroI8,
	std::num::NonZeroI16,
	std::num::NonZeroI32,
	std::num::NonZeroU8,
	std::num::NonZeroU16,
	std::num::NonZeroU32
);

macro_rules! wide_integer_type {
	($($ty:ty),* $(,)?) => {
		$(
			impl TsGen for $ty {
				fn type_ref() -> TypeRef {
					TypeRef::WideInteger {
						rust_type: stringify!($ty).to_owned(),
					}
				}
			}
		)*
	};
}

wide_integer_type!(
	i64,
	i128,
	isize,
	u64,
	u128,
	usize,
	std::num::NonZeroI64,
	std::num::NonZeroI128,
	std::num::NonZeroIsize,
	std::num::NonZeroU64,
	std::num::NonZeroU128,
	std::num::NonZeroUsize
);

macro_rules! number_type {
	($($ty:ty),* $(,)?) => {
		$(
			/// Declares `number`, which holds only for finite values:
			/// `serde_json` writes `null` for NaN and for positive and
			/// negative infinity, and its deserializer rejects `null` for
			/// this type.
			impl TsGen for $ty {
				fn type_ref() -> TypeRef {
					TypeRef::Number
				}
			}
		)*
	};
}

number_type!(f32, f64);

impl<T: TsGen> TsGen for Option<T> {
	fn type_ref() -> TypeRef {
		TypeRef::nullable(T::type_ref())
	}

	fn type_ref_for(phase: TypePhase) -> TypeRef {
		TypeRef::nullable(T::type_ref_for(phase))
	}

	fn collect_type_defs(registry: &mut TypeRegistry) -> Result<()> {
		T::collect_type_defs(registry)
	}

	fn collect_type_defs_for(phase: TypePhase, registry: &mut TypeRegistry) -> Result<()> {
		T::collect_type_defs_for(phase, registry)
	}
}

impl<T: TsGen> TsGen for Vec<T> {
	fn type_ref() -> TypeRef {
		TypeRef::array(T::type_ref())
	}

	fn type_ref_for(phase: TypePhase) -> TypeRef {
		TypeRef::array(T::type_ref_for(phase))
	}

	fn collect_type_defs(registry: &mut TypeRegistry) -> Result<()> {
		T::collect_type_defs(registry)
	}

	fn collect_type_defs_for(phase: TypePhase, registry: &mut TypeRegistry) -> Result<()> {
		T::collect_type_defs_for(phase, registry)
	}
}

/// Declares `Array<T>`, which does not state the length: `serde_json` writes
/// exactly `N` elements and its deserializer accepts only `N` elements.
impl<T: TsGen, const N: usize> TsGen for [T; N] {
	fn type_ref() -> TypeRef {
		TypeRef::array(T::type_ref())
	}

	fn type_ref_for(phase: TypePhase) -> TypeRef {
		TypeRef::array(T::type_ref_for(phase))
	}

	fn collect_type_defs(registry: &mut TypeRegistry) -> Result<()> {
		T::collect_type_defs(registry)
	}

	fn collect_type_defs_for(phase: TypePhase, registry: &mut TypeRegistry) -> Result<()> {
		T::collect_type_defs_for(phase, registry)
	}
}

impl<T: TsGen + ?Sized> TsGen for Box<T> {
	fn type_ref() -> TypeRef {
		T::type_ref()
	}

	fn type_ref_for(phase: TypePhase) -> TypeRef {
		T::type_ref_for(phase)
	}

	fn collect_type_defs(registry: &mut TypeRegistry) -> Result<()> {
		T::collect_type_defs(registry)
	}

	fn collect_type_defs_for(phase: TypePhase, registry: &mut TypeRegistry) -> Result<()> {
		T::collect_type_defs_for(phase, registry)
	}
}

impl<T: TsGen + ?Sized> TsGen for std::sync::Arc<T> {
	fn type_ref() -> TypeRef {
		T::type_ref()
	}

	fn type_ref_for(phase: TypePhase) -> TypeRef {
		T::type_ref_for(phase)
	}

	fn collect_type_defs(registry: &mut TypeRegistry) -> Result<()> {
		T::collect_type_defs(registry)
	}

	fn collect_type_defs_for(phase: TypePhase, registry: &mut TypeRegistry) -> Result<()> {
		T::collect_type_defs_for(phase, registry)
	}
}

impl<T: TsGen> TsGen for BTreeMap<String, T> {
	fn type_ref() -> TypeRef {
		TypeRef::record(TypeRef::String, T::type_ref())
	}

	fn type_ref_for(phase: TypePhase) -> TypeRef {
		TypeRef::record(TypeRef::String, T::type_ref_for(phase))
	}

	fn collect_type_defs(registry: &mut TypeRegistry) -> Result<()> {
		T::collect_type_defs(registry)
	}

	fn collect_type_defs_for(phase: TypePhase, registry: &mut TypeRegistry) -> Result<()> {
		T::collect_type_defs_for(phase, registry)
	}
}

impl<T: TsGen> TsGen for HashMap<String, T> {
	fn type_ref() -> TypeRef {
		TypeRef::record(TypeRef::String, T::type_ref())
	}

	fn type_ref_for(phase: TypePhase) -> TypeRef {
		TypeRef::record(TypeRef::String, T::type_ref_for(phase))
	}

	fn collect_type_defs(registry: &mut TypeRegistry) -> Result<()> {
		T::collect_type_defs(registry)
	}

	fn collect_type_defs_for(phase: TypePhase, registry: &mut TypeRegistry) -> Result<()> {
		T::collect_type_defs_for(phase, registry)
	}
}

macro_rules! tuple_types {
	($($param:ident),+) => {
		impl<$($param: TsGen),+> TsGen for ($($param,)+) {
			fn type_ref() -> TypeRef { Self::type_ref_for(TypePhase::Serialize) }
			fn type_ref_for(phase: TypePhase) -> TypeRef { TypeRef::Tuple(vec![$($param::type_ref_for(phase)),+]) }
			fn collect_type_defs(registry: &mut TypeRegistry) -> Result<()> { Self::collect_type_defs_for(TypePhase::Serialize, registry) }
			fn collect_type_defs_for(phase: TypePhase, registry: &mut TypeRegistry) -> Result<()> {
				$($param::collect_type_defs_for(phase, registry)?;)+
				Ok(())
			}
		}
	};
}
// Serde supports tuples with up to 16 elements.
tuple_types!(T0);
tuple_types!(T0, T1);
tuple_types!(T0, T1, T2);
tuple_types!(T0, T1, T2, T3);
tuple_types!(T0, T1, T2, T3, T4);
tuple_types!(T0, T1, T2, T3, T4, T5);
tuple_types!(T0, T1, T2, T3, T4, T5, T6);
tuple_types!(T0, T1, T2, T3, T4, T5, T6, T7);
tuple_types!(T0, T1, T2, T3, T4, T5, T6, T7, T8);
tuple_types!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9);
tuple_types!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10);
tuple_types!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11);
tuple_types!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12);
tuple_types!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13);
tuple_types!(
	T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14
);
tuple_types!(
	T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15
);

pub(crate) fn is_ts_identifier_reserved(name: &str) -> bool {
	matches!(
		name,
		"abstract"
			| "any" | "as"
			| "async" | "await"
			| "bigint"
			| "boolean"
			| "break" | "case"
			| "catch" | "class"
			| "const" | "constructor"
			| "continue"
			| "debugger"
			| "declare"
			| "default"
			| "delete"
			| "do" | "else"
			| "enum" | "export"
			| "extends"
			| "false" | "finally"
			| "for" | "from"
			| "function"
			| "get" | "if"
			| "implements"
			| "import"
			| "in" | "infer"
			| "instanceof"
			| "interface"
			| "is" | "keyof"
			| "let" | "module"
			| "namespace"
			| "never" | "new"
			| "null" | "number"
			| "object"
			| "of" | "package"
			| "private"
			| "protected"
			| "public"
			| "readonly"
			| "require"
			| "return"
			| "set" | "static"
			| "string"
			| "super" | "switch"
			| "symbol"
			| "this" | "throw"
			| "true" | "try"
			| "type" | "typeof"
			| "undefined"
			| "unique"
			| "unknown"
			| "var" | "void"
			| "while" | "with"
			| "yield"
	)
}

impl<T: TsGen + zeroize::Zeroize> TsGen for zeroize::Zeroizing<T> {
	fn type_ref() -> TypeRef {
		T::type_ref()
	}
	fn type_ref_for(phase: TypePhase) -> TypeRef {
		T::type_ref_for(phase)
	}
	fn collect_type_defs(registry: &mut TypeRegistry) -> Result<()> {
		T::collect_type_defs(registry)
	}
	fn collect_type_defs_for(phase: TypePhase, registry: &mut TypeRegistry) -> Result<()> {
		T::collect_type_defs_for(phase, registry)
	}
}
