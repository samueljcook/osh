//! The expansion of the TypeScript generation derive macro.
//!
//! A procedural macro crate cannot export ordinary library functions, so this
//! library implements the expansion. A crate that re-exports the derive macro under its own name
//! passes the path through which generated code reaches the runtime crate's
//! `tsgen` module.

#![deny(missing_docs)]
#![forbid(unsafe_code)]

use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use syn::ext::IdentExt;
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::visit_mut::VisitMut;
use syn::{GenericArgument, GenericParam, PathArguments, Type};

/// Expands the derive macro for `input`. The generated code names the TypeScript
/// generation module through `tsgen_module`, the path to that module in the
/// crate that the application depends on, such as `::primitives_rs::tsgen`.
pub fn expand_ts_gen(
	mut input: syn::DeriveInput,
	tsgen_module: &TokenStream,
) -> syn::Result<TokenStream> {
	let container = ContainerAttrs::from_attrs(&input.attrs)?;
	let container_docs = collect_doc_comment(&input.attrs);
	let ident = input.ident.clone();
	let type_name = ident.unraw().to_string();

	// `TsGen` requires `'static`, so the implementation is for the type with
	// every lifetime parameter set to `'static`.
	let self_type_args = input
		.generics
		.params
		.iter()
		.map(|param| match param {
			GenericParam::Lifetime(_) => quote! { 'static },
			GenericParam::Type(param) => {
				let ident = &param.ident;
				quote! { #ident }
			}
			GenericParam::Const(param) => {
				let ident = &param.ident;
				quote! { #ident }
			}
		})
		.collect::<Vec<_>>();
	let mut static_lifetimes = StaticLifetimeRewriter {
		lifetimes: input
			.generics
			.lifetimes()
			.map(|param| param.lifetime.ident.clone())
			.collect(),
	};
	static_lifetimes.visit_data_mut(&mut input.data);
	static_lifetimes.visit_generics_mut(&mut input.generics);
	let mut generics = input.generics;
	generics.params = generics
		.params
		.into_iter()
		.filter(|param| !matches!(param, GenericParam::Lifetime(_)))
		.collect();
	let type_params = generics
		.type_params()
		.map(|param| param.ident.clone())
		.collect::<Vec<_>>();
	for type_param in type_params.iter().filter(|_| !container.concrete) {
		generics
			.make_where_clause()
			.predicates
			.push(syn::parse_quote! { #type_param: __ts_gen::TsGen });
	}
	let const_params: Vec<_> = generics
		.const_params()
		.map(|param| param.ident.clone())
		.collect();

	let symbolic_params: Vec<_> = type_params
		.iter()
		.map(|param| format_ident!("__TsGen{}", param))
		.collect();

	let mut template_data = input.data;
	let mut concrete_self: Type = if self_type_args.is_empty() {
		syn::parse_quote!(#ident)
	} else {
		syn::parse_quote!(#ident<#(#self_type_args),*>)
	};
	let mut rewriter = SymbolicTypeRewriter {
		params: &type_params,
		replacements: &symbolic_params,
		self_type: None,
	};
	rewriter.visit_type_mut(&mut concrete_self);
	if !container.concrete {
		let shape = Shape::parse(template_data.clone(), &ident, &container)?;
		shape.check_generic_projections(&type_params)?;
		rewriter.self_type = Some(concrete_self);
		rewriter.visit_data_mut(&mut template_data);
	}
	let shape = Shape::parse(template_data, &ident, &container)?;
	if container.concrete {
		let mut bounds = ConcreteBounds {
			params: &type_params,
			types: Vec::new(),
		};
		for ty in shape.rendered_types() {
			bounds.visit_type(ty);
		}
		for ty in bounds.types {
			generics
				.make_where_clause()
				.predicates
				.push(syn::parse_quote!(#ty: __ts_gen::TsGen));
		}
		generics
			.make_where_clause()
			.predicates
			.push(syn::parse_quote!(Self: ::std::marker::Send + ::std::marker::Sync + 'static));
	}
	let serialize = shape.phase_tokens(Phase::Serialize, &type_name);
	let deserialize = shape.phase_tokens(Phase::Deserialize, &type_name);
	let (impl_generics, _, where_clause) = generics.split_for_impl();
	let self_type = if self_type_args.is_empty() {
		quote! { #ident }
	} else {
		quote! { #ident<#(#self_type_args),*> }
	};
	let serialize_phase = Phase::Serialize.type_phase_tokens();
	let deserialize_phase = Phase::Deserialize.type_phase_tokens();
	let PhaseTokens {
		type_ref: serialize_type_ref,
		collect: serialize_collect,
	} = serialize;
	let PhaseTokens {
		type_ref: deserialize_type_ref,
		collect: deserialize_collect,
	} = deserialize;
	let type_name_lit = syn::LitStr::new(&type_name, ident.span());
	let const_values = quote! { ::std::vec![#(::std::format!("{:?}", #const_params)),*].join(",") };
	let phase_key = |phase: Phase| {
		let suffix = phase.key_suffix();
		if container.concrete {
			quote! { ::std::format!("{}{}", ::std::any::type_name::<Self>(), #suffix) }
		} else if const_params.is_empty() {
			quote! { ::core::concat!(::core::module_path!(), "::", ::core::stringify!(#ident), #suffix) }
		} else {
			quote! { ::std::format!("{}<{}>{}", ::core::concat!(::core::module_path!(), "::", ::core::stringify!(#ident)), #const_values, #suffix) }
		}
	};
	let serialize_key = phase_key(Phase::Serialize);
	let deserialize_key = phase_key(Phase::Deserialize);
	let type_name_expr = if const_params.is_empty() {
		quote! { #type_name_lit }
	} else {
		let const_names: Vec<_> = const_params
			.iter()
			.map(|param| param.unraw().to_string())
			.collect();
		quote! {{
			let mut name = ::std::string::String::from(#type_name_lit);
			#(
				name.push('_'); name.push_str(#const_names); name.push('_');
				for byte in ::std::format!("{:?}", #const_params).bytes() {
					if byte.is_ascii_alphanumeric() { name.push(::core::primitive::char::from(byte)); }
					else { name.push_str(&::std::format!("_{byte:02x}")); }
				}
			)*
			name
		}}
	};
	let exported_params = if container.concrete {
		&[][..]
	} else {
		type_params.as_slice()
	};
	let param_names: Vec<_> = exported_params
		.iter()
		.map(|param| param.unraw().to_string())
		.collect();
	let set_docs = container_docs
		.as_deref()
		.map(|docs| quote! { .set_docs(#docs) });
	let type_ref_for = quote! {
		fn type_ref_for(phase: __ts_gen::TypePhase) -> __ts_gen::TypeRef {
			let key = match phase { #serialize_phase => #serialize_key, #deserialize_phase => #deserialize_key };
			__ts_gen::TypeRef::generic(key, #type_name_expr, ::std::vec![#(<#exported_params as __ts_gen::TsGen>::type_ref_for(phase)),*])
		}
	};
	let collect_type_defs_for = quote! {
		fn collect_type_defs_for(phase: __ts_gen::TypePhase, registry: &mut __ts_gen::TypeRegistry) -> __ts_gen::Result<()> {
			registry.collect_type::<Self>(phase, |registry| {
				match phase {
					#serialize_phase => {
						registry.try_define(__ts_gen::TypeDef::new(#serialize_key, #type_name_expr, #serialize_type_ref)
							.set_type_params(::std::vec![#(::std::string::String::from(#param_names)),*]) #set_docs)?;
						#(#serialize_collect)*
					}
					#deserialize_phase => {
						registry.try_define(__ts_gen::TypeDef::new(#deserialize_key, #type_name_expr, #deserialize_type_ref)
							.set_type_params(::std::vec![#(::std::string::String::from(#param_names)),*]) #set_docs)?;
						#(#deserialize_collect)*
					}
				}
				#(<#exported_params as __ts_gen::TsGen>::collect_type_defs_for(phase, registry)?;)*
				::core::result::Result::Ok(())
			})
		}
	};

	let marker_names: Vec<_> = type_params
		.iter()
		.map(|param| param.unraw().to_string())
		.collect();
	let markers = if container.concrete {
		quote! {}
	} else {
		quote! {
			#(
				#[derive(
					::core::clone::Clone,
					::core::marker::Copy,
					::core::fmt::Debug,
					::core::default::Default,
					::core::cmp::Eq,
					::core::cmp::PartialEq,
					::core::cmp::Ord,
					::core::cmp::PartialOrd,
					::core::hash::Hash,
				)]
				struct #symbolic_params;
				impl __ts_gen::TsGen for #symbolic_params {
					fn type_ref() -> __ts_gen::TypeRef { __ts_gen::TypeRef::Parameter(::std::string::String::from(#marker_names)) }
				}
			)*
		}
	};

	Ok(quote! {
		const _: () = {
		use #tsgen_module as __ts_gen;
		#markers

		impl #impl_generics __ts_gen::TsGen for #self_type #where_clause {
			fn type_ref() -> __ts_gen::TypeRef {
				Self::type_ref_for(#serialize_phase)
			}

			fn collect_type_defs(
				registry: &mut __ts_gen::TypeRegistry,
			) -> ::std::result::Result<(), __ts_gen::Error> {
				Self::collect_type_defs_for(#serialize_phase, registry)
			}

			#type_ref_for

			#collect_type_defs_for
		}
		};
	})
}

/// Replaces each listed lifetime with `'static`.
struct StaticLifetimeRewriter {
	lifetimes: Vec<syn::Ident>,
}

impl VisitMut for StaticLifetimeRewriter {
	fn visit_lifetime_mut(&mut self, lifetime: &mut syn::Lifetime) {
		if self.lifetimes.contains(&lifetime.ident) {
			*lifetime = syn::Lifetime::new("'static", lifetime.span());
		}
	}
}

#[derive(Clone, Copy)]
enum Phase {
	Serialize,
	Deserialize,
}

impl Phase {
	fn type_phase_tokens(self) -> TokenStream {
		match self {
			Self::Serialize => quote! { __ts_gen::TypePhase::Serialize },
			Self::Deserialize => quote! { __ts_gen::TypePhase::Deserialize },
		}
	}

	/// The suffix of a named type's `TypeDef` key in this phase. These
	/// suffixes must equal the phase key suffixes in
	/// `primitives_rs::tsgen::declarations`.
	fn key_suffix(self) -> &'static str {
		match self {
			Self::Serialize => "::serialize",
			Self::Deserialize => "::deserialize",
		}
	}
}

/// The TypeScript expression of one phase of a shape, and the statements
/// that collect the definitions of the Rust types that the expression
/// refers to.
struct PhaseTokens {
	type_ref: TokenStream,
	collect: Vec<TokenStream>,
}

/// The `serde` shape of a derived type.
enum Shape {
	/// A struct with named fields.
	Record(NamedFields),
	/// A tuple struct with zero or at least two fields, which `serde`
	/// writes as a JSON array.
	Tuple(Vec<Type>),
	/// A newtype struct or a `#[serde(transparent)]` struct, which `serde`
	/// writes as its one field.
	Inner(Type),
	/// A unit struct, which `serde` writes as `null`.
	Unit,
	Enum {
		representation: EnumRepresentation,
		/// The container's `rename_all` rules, which apply to variant names.
		rename_all: PhaseRenameRules,
		variants: Vec<Variant>,
	},
}

impl Shape {
	fn parse(data: syn::Data, ident: &syn::Ident, container: &ContainerAttrs) -> syn::Result<Self> {
		match data {
			syn::Data::Union(data) => Err(syn::Error::new_spanned(
				data.union_token,
				"TsGen derive does not support unions; use a manual TsGen implementation for a union",
			)),
			syn::Data::Struct(data) => {
				container.reject_enum_attrs()?;
				if container.transparent.is_some() {
					let fields = data.fields.iter().collect::<Vec<_>>();
					let [field] = fields.as_slice() else {
						return Err(syn::Error::new_spanned(
							ident,
							"serde(transparent) requires exactly one field for TsGen derive",
						));
					};
					// Parsing rejects field codecs, which would change the
					// serialized shape.
					FieldAttrs::from_attrs(&field.attrs)?;
					reject_non_string_map_key(&field.ty)?;
					return Ok(Self::Inner(field.ty.clone()));
				}
				match data.fields {
					syn::Fields::Named(fields) => Ok(Self::Record(NamedFields::parse(
						fields.named,
						container.rename_all,
						container.default.is_some(),
					)?)),
					syn::Fields::Unnamed(fields) if fields.unnamed.len() == 1 => {
						Ok(Self::Inner(parse_unnamed_field(&fields.unnamed[0])?))
					}
					syn::Fields::Unnamed(fields) => Ok(Self::Tuple(
						fields
							.unnamed
							.iter()
							.map(parse_unnamed_field)
							.collect::<syn::Result<_>>()?,
					)),
					syn::Fields::Unit => Ok(Self::Unit),
				}
			}
			syn::Data::Enum(data) => {
				container.reject_struct_attrs()?;
				let representation = container.enum_representation()?;
				let variants = data
					.variants
					.into_iter()
					.filter_map(|variant| {
						Variant::parse(variant, container, &representation).transpose()
					})
					.collect::<syn::Result<_>>()?;
				Ok(Self::Enum {
					representation,
					rename_all: container.rename_all,
					variants,
				})
			}
		}
	}

	fn rendered_types(&self) -> Vec<&Type> {
		let mut types = Vec::new();
		match self {
			Self::Record(fields) => types.extend(fields.rendered_types()),
			Self::Enum { variants, .. } => {
				for variant in variants {
					match &variant.fields {
						VariantFields::Unit => {}
						VariantFields::Newtype(ty) => types.push(ty),
						VariantFields::Tuple(tys) => types.extend(tys),
						VariantFields::Named(fields) => types.extend(fields.rendered_types()),
					}
				}
			}
			Self::Tuple(tys) => types.extend(tys),
			Self::Inner(ty) => types.push(ty),
			Self::Unit => {}
		}
		types
	}

	/// Rejects associated projections that cannot be expressed by a TS parameter.
	fn check_generic_projections(&self, params: &[syn::Ident]) -> syn::Result<()> {
		let types = self.rendered_types();
		for ty in types {
			let mut finder = GenericProjectionFinder {
				params,
				found: false,
			};
			finder.visit_type(ty);
			if finder.found {
				return Err(syn::Error::new_spanned(
					ty,
					"TsGen derive cannot represent a generic associated type projection; use a type parameter for the wire value or implement TsGen for the concrete shape",
				));
			}
		}
		Ok(())
	}

	fn phase_tokens(&self, phase: Phase, type_name: &str) -> PhaseTokens {
		match self {
			Self::Record(fields) => fields.phase_tokens(phase, Vec::new(), type_name),
			Self::Tuple(tys) => tuple_tokens(tys, phase),
			Self::Inner(ty) => PhaseTokens {
				type_ref: type_ref_tokens(ty, phase),
				collect: vec![collect_type_defs_tokens(ty, phase)],
			},
			Self::Unit => PhaseTokens {
				type_ref: quote! { __ts_gen::TypeRef::Null },
				collect: Vec::new(),
			},
			Self::Enum {
				representation,
				rename_all,
				variants,
			} => {
				let mut members = Vec::new();
				let mut collect = Vec::new();
				for variant in variants {
					if let Some(tokens) =
						variant.phase_tokens(phase, representation, *rename_all, type_name)
					{
						let type_ref = tokens.type_ref;
						members.push(match &variant.docs {
							Some(docs) => {
								quote! { __ts_gen::TypeRef::documented(#type_ref, #docs) }
							}
							None => type_ref,
						});
						collect.extend(tokens.collect);
					}
				}
				PhaseTokens {
					type_ref: quote! {
						__ts_gen::TypeRef::Union(::std::vec![#(#members),*])
					},
					collect,
				}
			}
		}
	}
}

struct ConcreteBounds<'a> {
	params: &'a [syn::Ident],
	types: Vec<Type>,
}
impl<'ast> Visit<'ast> for ConcreteBounds<'_> {
	fn visit_type(&mut self, ty: &'ast Type) {
		if let Type::Path(path) = ty
			&& (path.qself.is_some()
				|| self
					.params
					.iter()
					.any(|param| path.path.segments[0].ident == *param))
		{
			self.types.push(ty.clone());
			return;
		}
		syn::visit::visit_type(self, ty);
	}
}

struct SymbolicTypeRewriter<'a> {
	params: &'a [syn::Ident],
	replacements: &'a [syn::Ident],
	self_type: Option<Type>,
}
impl VisitMut for SymbolicTypeRewriter<'_> {
	fn visit_type_mut(&mut self, ty: &mut Type) {
		if let Type::Path(path) = ty {
			if path.path.is_ident("Self")
				&& let Some(replacement) = &self.self_type
			{
				*ty = replacement.clone();
				return;
			}
			if let Some(index) = self
				.params
				.iter()
				.position(|param| path.path.is_ident(param))
			{
				let ident = &self.replacements[index];
				*ty = syn::parse_quote!(#ident);
				return;
			}
		}
		syn::visit_mut::visit_type_mut(self, ty);
	}
}

struct GenericParameterFinder<'a> {
	params: &'a [syn::Ident],
	found: bool,
}
impl<'ast> Visit<'ast> for GenericParameterFinder<'_> {
	fn visit_type_path(&mut self, path: &'ast syn::TypePath) {
		if path.path.is_ident("Self") || self.params.iter().any(|param| path.path.is_ident(param)) {
			self.found = true;
		}
		syn::visit::visit_type_path(self, path);
	}
}

struct GenericProjectionFinder<'a> {
	params: &'a [syn::Ident],
	found: bool,
}
impl<'ast> Visit<'ast> for GenericProjectionFinder<'_> {
	fn visit_type_path(&mut self, path: &'ast syn::TypePath) {
		let mut parameters = GenericParameterFinder {
			params: self.params,
			found: false,
		};
		parameters.visit_type_path(path);
		if (path.qself.is_some() && parameters.found)
			|| (path.path.segments.len() > 1
				&& self
					.params
					.iter()
					.any(|param| path.path.segments[0].ident == *param))
		{
			self.found = true;
		}
		syn::visit::visit_type_path(self, path);
	}
}

/// How `serde` represents an enum's variants.
enum EnumRepresentation {
	/// `serde`'s default: `"Unit"` or `{ "Variant": content }`.
	External,
	/// `#[serde(tag = "...")]`: `{ "tag": "Variant", ...fields }`.
	Internal { tag: String },
	/// `#[serde(tag = "...", content = "...")]`:
	/// `{ "tag": "Variant", "content": content }`.
	Adjacent { tag: String, content: String },
	/// `#[serde(untagged)]`: the content alone.
	Untagged,
}

struct Variant {
	docs: Option<String>,
	rust_name: String,
	attrs: VariantAttrs,
	fields: VariantFields,
}

enum VariantFields {
	Unit,
	Newtype(Type),
	Tuple(Vec<Type>),
	Named(NamedFields),
}

impl Variant {
	fn parse(
		variant: syn::Variant,
		container: &ContainerAttrs,
		representation: &EnumRepresentation,
	) -> syn::Result<Option<Self>> {
		let attrs = VariantAttrs::from_attrs(&variant.attrs)?;
		if !attrs.skips.retains(Phase::Serialize) && !attrs.skips.retains(Phase::Deserialize) {
			return Ok(None);
		}
		let fields = match &variant.fields {
			syn::Fields::Unit => VariantFields::Unit,
			syn::Fields::Unnamed(fields) if fields.unnamed.len() == 1 => {
				VariantFields::Newtype(parse_unnamed_field(&fields.unnamed[0])?)
			}
			syn::Fields::Unnamed(fields) => {
				if matches!(representation, EnumRepresentation::Internal { .. }) {
					return Err(syn::Error::new_spanned(
						&variant,
						"serde(tag = \"...\") cannot be used with tuple variants because serde does not support them in an internally tagged enum",
					));
				}
				VariantFields::Tuple(
					fields
						.unnamed
						.iter()
						.map(parse_unnamed_field)
						.collect::<syn::Result<_>>()?,
				)
			}
			syn::Fields::Named(fields) => VariantFields::Named(NamedFields::parse(
				fields.named.clone(),
				attrs.rename_all.or(container.rename_all_fields),
				false,
			)?),
		};
		Ok(Some(Self {
			rust_name: variant.ident.unraw().to_string(),
			docs: collect_doc_comment(&variant.attrs),
			attrs,
			fields,
		}))
	}

	fn phase_tokens(
		&self,
		phase: Phase,
		representation: &EnumRepresentation,
		rename_all: PhaseRenameRules,
		type_name: &str,
	) -> Option<PhaseTokens> {
		if !self.attrs.skips.retains(phase) {
			return None;
		}
		let name = self.attrs.rename.resolve(
			phase,
			&self.rust_name,
			rename_all.for_phase(phase),
			RenameRule::apply_to_variant,
		);
		let tag_value = quote! { __ts_gen::TypeRef::string_literal(#name) };
		let content = match &self.fields {
			VariantFields::Unit => None,
			VariantFields::Newtype(ty) => {
				let mut collect = vec![collect_type_defs_tokens(ty, phase)];
				// `serde` writes the tag into the object that the content
				// writes.
				if let EnumRepresentation::Internal { tag } = representation {
					let variant = format!("{type_name}::{}", self.rust_name);
					collect.push(merged_content_check_tokens(
						ty,
						phase,
						&format!(
							"TsGen cannot describe the newtype variant {variant} of an internally tagged enum because its content is a map with values other than serde_json::Value, and TypeScript would require the tag field {tag:?} to have the map's value type; use a struct or a map with serde_json::Value values as the content"
						),
						Some(&format!(
							"TsGen cannot describe the newtype variant {variant} of an internally tagged enum because its content serializes as null, and serde writes only the tag field {tag:?} for it; make {} a unit variant",
							self.rust_name
						)),
					));
				}
				Some(PhaseTokens {
					type_ref: type_ref_tokens(ty, phase),
					collect,
				})
			}
			VariantFields::Tuple(tys) => Some(tuple_tokens(tys, phase)),
			VariantFields::Named(fields) => {
				let description = format!("{type_name}::{}", self.rust_name);
				// An internally tagged struct variant writes its tag as the
				// first field of its own object.
				if let EnumRepresentation::Internal { tag } = representation {
					return Some(fields.phase_tokens(
						phase,
						vec![required_field_tokens(tag, &tag_value)],
						&description,
					));
				}
				Some(fields.phase_tokens(phase, Vec::new(), &description))
			}
		};
		let (content_type_ref, collect) = match content {
			Some(PhaseTokens { type_ref, collect }) => (Some(type_ref), collect),
			None => (None, Vec::new()),
		};
		let type_ref = match (representation, content_type_ref) {
			(EnumRepresentation::External, None) => tag_value,
			(EnumRepresentation::External, Some(content)) => {
				object_tokens(&[required_field_tokens(&name, &content)])
			}
			(
				EnumRepresentation::Internal { tag } | EnumRepresentation::Adjacent { tag, .. },
				None,
			) => object_tokens(&[required_field_tokens(tag, &tag_value)]),
			// This arm handles a newtype variant. `serde` writes the
			// tag into the object that the newtype's content writes.
			(EnumRepresentation::Internal { tag }, Some(content)) => {
				let tag_object = object_tokens(&[required_field_tokens(tag, &tag_value)]);
				quote! {
					__ts_gen::TypeRef::Intersection(::std::vec![#tag_object, #content])
				}
			}
			(
				EnumRepresentation::Adjacent {
					tag,
					content: content_key,
				},
				Some(content),
			) => object_tokens(&[
				required_field_tokens(tag, &tag_value),
				required_field_tokens(content_key, &content),
			]),
			(EnumRepresentation::Untagged, None) => {
				quote! { __ts_gen::TypeRef::Null }
			}
			(EnumRepresentation::Untagged, Some(content)) => content,
		};
		Some(PhaseTokens { type_ref, collect })
	}
}

/// The named fields of a struct or a struct variant.
struct NamedFields {
	fields: Vec<NamedField>,
	rename_all: PhaseRenameRules,
	/// Whether the container has `#[serde(default)]`.
	default: bool,
}

struct NamedField {
	rust_name: String,
	ty: Type,
	attrs: FieldAttrs,
	docs: Option<String>,
}

impl NamedFields {
	fn parse(
		fields: syn::punctuated::Punctuated<syn::Field, syn::Token![,]>,
		rename_all: PhaseRenameRules,
		default: bool,
	) -> syn::Result<Self> {
		let fields = fields
			.into_iter()
			.map(|field| {
				let attrs = FieldAttrs::from_attrs(&field.attrs)?;
				if attrs.flatten
					&& (attrs.rename.serialize.is_some()
						|| attrs.rename.deserialize.is_some()
						|| attrs.skip_serializing_if != SkipSerializingIf::Never
						|| attrs.default)
				{
					return Err(syn::Error::new_spanned(
						&field,
						"TsGen derive does not support serde(rename), serde(skip_serializing_if), or serde(default) on a serde(flatten) field",
					));
				}
				if attrs.skips.retains(Phase::Serialize) || attrs.skips.retains(Phase::Deserialize) {
					reject_non_string_map_key(&field.ty)?;
				}
				let Some(ident) = &field.ident else {
					return Err(syn::Error::new_spanned(&field, "expected a named field"));
				};
				Ok(NamedField {
					rust_name: ident.unraw().to_string(),
					docs: collect_doc_comment(&field.attrs),
					ty: field.ty,
					attrs,
				})
			})
			.collect::<syn::Result<_>>()?;
		Ok(Self {
			fields,
			rename_all,
			default,
		})
	}

	/// The types of the fields that at least one phase renders.
	fn rendered_types(&self) -> impl Iterator<Item = &Type> {
		self.fields
			.iter()
			.filter(|field| {
				field.attrs.skips.retains(Phase::Serialize)
					|| field.attrs.skips.retains(Phase::Deserialize)
			})
			.map(|field| &field.ty)
	}

	/// `leading_fields` are object fields that precede the struct's own
	/// fields, such as an internally tagged variant's tag. `description`
	/// names the struct or variant in errors.
	fn phase_tokens(
		&self,
		phase: Phase,
		leading_fields: Vec<TokenStream>,
		description: &str,
	) -> PhaseTokens {
		let mut object_fields = leading_fields;
		let mut flattened = Vec::new();
		let mut collect = Vec::new();
		for field in &self.fields {
			if !field.attrs.skips.retains(phase) {
				continue;
			}
			collect.push(collect_type_defs_tokens(&field.ty, phase));
			if field.attrs.flatten {
				flattened.push(flattened_type_ref_tokens(&field.ty, phase));
				collect.push(merged_content_check_tokens(
					&field.ty,
					phase,
					&format!(
						"TsGen cannot describe the flattened field {} of {description} because it is a map with values other than serde_json::Value, and TypeScript would require every property of the object to have the map's value type",
						field.rust_name
					),
					None,
				));
				continue;
			}
			let name = field.attrs.rename.resolve(
				phase,
				&field.rust_name,
				self.rename_all.for_phase(phase),
				RenameRule::apply_to_field,
			);
			let optional = match phase {
				Phase::Serialize => field.attrs.skip_serializing_if != SkipSerializingIf::Never,
				Phase::Deserialize => {
					self.default || field.attrs.default || is_option_type(&field.ty)
				}
			};
			let presence = if optional {
				quote! { __ts_gen::FieldPresence::Optional }
			} else {
				quote! { __ts_gen::FieldPresence::Required }
			};
			let type_ref = match (
				phase,
				field.attrs.skip_serializing_if,
				option_inner_type(&field.ty),
			) {
				// With this predicate, `serde` writes the field only for `Some(value)`, so
				// the written value has the type inside the `Option`.
				(Phase::Serialize, SkipSerializingIf::OptionIsNone, Some(inner)) => {
					type_ref_tokens(inner, phase)
				}
				_ => type_ref_tokens(&field.ty, phase),
			};
			let set_docs = field.docs.as_deref().map(|docs| {
				let docs = syn::LitStr::new(docs, Span::call_site());
				quote! { .set_docs(#docs) }
			});
			object_fields.push(quote! {
				__ts_gen::FieldDef::new(#name, #type_ref, #presence)#set_docs
			});
		}
		let type_ref = match (object_fields.is_empty(), flattened.is_empty()) {
			(_, true) => object_tokens(&object_fields),
			(true, false) => quote! {
				__ts_gen::TypeRef::Intersection(::std::vec![#(#flattened),*])
			},
			(false, false) => {
				let object = object_tokens(&object_fields);
				quote! {
					__ts_gen::TypeRef::Intersection(::std::vec![#object, #(#flattened),*])
				}
			}
		};
		PhaseTokens { type_ref, collect }
	}
}

fn type_ref_tokens(ty: &Type, phase: Phase) -> TokenStream {
	let phase = phase.type_phase_tokens();
	quote! { <#ty as __ts_gen::TsGen>::type_ref_for(#phase) }
}

fn collect_type_defs_tokens(ty: &Type, phase: Phase) -> TokenStream {
	let phase = phase.type_phase_tokens();
	quote! {
		<#ty as __ts_gen::TsGen>::collect_type_defs_for(#phase, registry)?;
	}
}

fn object_tokens(fields: &[TokenStream]) -> TokenStream {
	quote! { __ts_gen::TypeRef::Object(::std::vec![#(#fields),*]) }
}

fn required_field_tokens(name: &str, type_ref: &TokenStream) -> TokenStream {
	quote! {
		__ts_gen::FieldDef::new(
			#name,
			#type_ref,
			__ts_gen::FieldPresence::Required,
		)
	}
}

fn tuple_tokens(tys: &[Type], phase: Phase) -> PhaseTokens {
	let items = tys.iter().map(|ty| type_ref_tokens(ty, phase));
	PhaseTokens {
		type_ref: quote! { __ts_gen::TypeRef::Tuple(::std::vec![#(#items),*]) },
		collect: tys
			.iter()
			.map(|ty| collect_type_defs_tokens(ty, phase))
			.collect(),
	}
}

/// A flattened field contributes its type's object to an intersection.
/// `serde` writes nothing for a flattened `None`, so a nullable type
/// contributes either its non-null type or an empty object.
fn flattened_type_ref_tokens(ty: &Type, phase: Phase) -> TokenStream {
	let type_ref = type_ref_tokens(ty, phase);
	quote! {
		match #type_ref {
			__ts_gen::TypeRef::Nullable(inner) => {
				__ts_gen::TypeRef::Union(::std::vec![
					*inner,
					__ts_gen::TypeRef::Object(::std::vec::Vec::new()),
				])
			}
			flattened => flattened,
		}
	}
}

/// Statements that check content that `serde` writes into an enclosing
/// object: the type of a flattened field, or the content of a newtype variant
/// of an internally tagged enum. The declaration intersects the enclosing
/// object with the content's type, apart from a nullable wrapper.
///
/// A TypeScript intersection with `Record<string, V>` requires every
/// property of the object, including the other fields or the tag, to have
/// type `V`. Only `unknown` describes every such property, so content that is
/// a map must have `serde_json::Value` values, and the statements return
/// `typed_map_message` otherwise. When `null_message` is present, the
/// statements return it for `null` content, which TypeScript intersects to
/// `never`.
fn merged_content_check_tokens(
	ty: &Type,
	phase: Phase,
	typed_map_message: &str,
	null_message: Option<&str>,
) -> TokenStream {
	let type_ref = type_ref_tokens(ty, phase);
	let null_check = null_message.map(|message| {
		quote! {
			if merged == __ts_gen::TypeRef::Null {
				return ::core::result::Result::Err(__ts_gen::Error::new(#message));
			}
		}
	});
	quote! {
		let merged = match #type_ref {
			__ts_gen::TypeRef::Nullable(inner) => *inner,
			merged => merged,
		};
		if ::core::matches!(
			&merged,
			__ts_gen::TypeRef::Record(_, value)
				if **value != __ts_gen::TypeRef::Unknown
		) {
			return ::core::result::Result::Err(__ts_gen::Error::new(#typed_map_message));
		}
		#null_check
	}
}

/// Parses an unnamed field of a tuple struct, a newtype struct, or a tuple or
/// newtype variant.
fn parse_unnamed_field(field: &syn::Field) -> syn::Result<Type> {
	// Parsing rejects field codecs and unknown attributes with their own
	// messages first.
	FieldAttrs::from_attrs(&field.attrs)?;
	if let Some(attr) = field
		.attrs
		.iter()
		.find(|attr| attr.path().is_ident("serde"))
	{
		return Err(syn::Error::new_spanned(
			attr,
			"TsGen derive does not support serde attributes on the fields of tuple structs, newtype structs, or tuple or newtype variants; use a manual TsGen implementation",
		));
	}
	reject_non_string_map_key(&field.ty)?;
	Ok(field.ty.clone())
}

fn reject_non_string_map_key(ty: &Type) -> syn::Result<()> {
	match find_non_string_map_key(ty) {
		Some(key_ty) => Err(syn::Error::new_spanned(
			key_ty,
			"non-string map keys are not supported by TsGen derive; use a manual TsGen implementation",
		)),
		None => Ok(()),
	}
}

#[derive(Default)]
struct ContainerAttrs {
	concrete: bool,
	rename_all: PhaseRenameRules,
	rename_all_fields: PhaseRenameRules,
	rename_all_fields_span: Option<Span>,
	default: Option<Span>,
	transparent: Option<Span>,
	tag: Option<syn::LitStr>,
	content: Option<syn::LitStr>,
	untagged: Option<Span>,
}

impl ContainerAttrs {
	fn from_attrs(attrs: &[syn::Attribute]) -> syn::Result<Self> {
		let mut out = Self::default();
		for attr in attrs.iter().filter(|attr| attr.path().is_ident("tsgen")) {
			attr.parse_nested_meta(|meta| {
				if meta.path.is_ident("concrete") {
					out.concrete = true;
					Ok(())
				} else {
					Err(meta.error("unsupported tsgen attribute; expected concrete"))
				}
			})?;
		}
		for attr in attrs.iter().filter(|attr| attr.path().is_ident("serde")) {
			attr.parse_nested_meta(|meta| {
				if meta.path.is_ident("rename_all") {
					out.rename_all = parse_rename_all(&meta)?;
					return Ok(());
				}
				if meta.path.is_ident("rename_all_fields") {
					out.rename_all_fields = parse_rename_all(&meta)?;
					out.rename_all_fields_span = Some(meta.path.span());
					return Ok(());
				}
				if meta.path.is_ident("transparent") {
					out.transparent = Some(meta.path.span());
					return Ok(());
				}
				if meta.path.is_ident("rename") {
					parse_phase_names(&meta)?;
					return Ok(());
				}
				if meta.path.is_ident("default") {
					if meta.input.peek(syn::Token![=]) {
						let value = meta.value()?;
						let _: syn::LitStr = value.parse()?;
					}
					out.default = Some(meta.path.span());
					return Ok(());
				}
				if meta.path.is_ident("tag") {
					out.tag = Some(meta.value()?.parse()?);
					return Ok(());
				}
				if meta.path.is_ident("content") {
					out.content = Some(meta.value()?.parse()?);
					return Ok(());
				}
				if meta.path.is_ident("untagged") {
					out.untagged = Some(meta.path.span());
					return Ok(());
				}
				if meta.path.is_ident("deny_unknown_fields") {
					// This attribute makes decoding stricter without changing the
					// fields or the TypeScript value types that the decoder accepts.
					return Ok(());
				}
				Err(meta.error(
					"unsupported serde container attribute for TsGen derive; the derive supports rename, rename_all, rename_all_fields, default, transparent, tag, content, untagged, and deny_unknown_fields, and a type with another serde container attribute needs a manual TsGen implementation",
				))
			})?;
		}
		Ok(out)
	}

	fn reject_enum_attrs(&self) -> syn::Result<()> {
		if let Some(tag) = &self.tag {
			return Err(syn::Error::new_spanned(
				tag,
				"TsGen derive does not support serde(tag) on a struct; use a manual TsGen implementation",
			));
		}
		if let Some(content) = &self.content {
			return Err(syn::Error::new_spanned(
				content,
				"serde(content) can only be used on enums",
			));
		}
		if let Some(span) = self.untagged {
			return Err(syn::Error::new(
				span,
				"serde(untagged) can only be used on enums",
			));
		}
		if let Some(span) = self.rename_all_fields_span {
			return Err(syn::Error::new(
				span,
				"serde(rename_all_fields) can only be used on enums",
			));
		}
		Ok(())
	}

	fn reject_struct_attrs(&self) -> syn::Result<()> {
		if let Some(span) = self.default {
			return Err(syn::Error::new(
				span,
				"serde(default) can only be used on structs",
			));
		}
		if let Some(span) = self.transparent {
			return Err(syn::Error::new(
				span,
				"serde(transparent) can only be used on structs",
			));
		}
		Ok(())
	}

	fn enum_representation(&self) -> syn::Result<EnumRepresentation> {
		match (&self.tag, &self.content, self.untagged) {
			(None, None, None) => Ok(EnumRepresentation::External),
			(Some(tag), None, None) => Ok(EnumRepresentation::Internal { tag: tag.value() }),
			(Some(tag), Some(content), None) => {
				if tag.value() == content.value() {
					return Err(syn::Error::new_spanned(
						content,
						"serde(tag) and serde(content) must name different fields",
					));
				}
				Ok(EnumRepresentation::Adjacent {
					tag: tag.value(),
					content: content.value(),
				})
			}
			(None, None, Some(_)) => Ok(EnumRepresentation::Untagged),
			(None, Some(content), None) => Err(syn::Error::new_spanned(
				content,
				"serde(content) requires serde(tag)",
			)),
			(_, _, Some(span)) => Err(syn::Error::new(
				span,
				"an untagged enum cannot have serde(tag) or serde(content)",
			)),
		}
	}
}

/// What the derive macro reads from a field's `skip_serializing_if` predicate.
#[derive(Clone, Copy, Default, Eq, PartialEq)]
enum SkipSerializingIf {
	/// The field has no predicate, so the serialize phase always writes the field.
	#[default]
	Never,
	/// The predicate's path ends in `Option::is_none`, so the serialize phase writes an
	/// `Option` field only for `Some`.
	OptionIsNone,
	/// Another predicate, which the derive macro does not evaluate.
	OtherPredicate,
}

impl SkipSerializingIf {
	fn from_predicate(predicate: &syn::LitStr) -> Self {
		let ends_in_option_is_none = predicate.parse::<syn::Path>().is_ok_and(|path| {
			let mut segments = path.segments.iter().rev();
			segments
				.next()
				.is_some_and(|segment| segment.ident == format_ident!("is_none"))
				&& segments
					.next()
					.is_some_and(|segment| segment.ident == format_ident!("Option"))
		});
		if ends_in_option_is_none {
			Self::OptionIsNone
		} else {
			Self::OtherPredicate
		}
	}
}

#[derive(Default)]
struct FieldAttrs {
	rename: PhaseNames,
	skips: Skips,
	skip_serializing_if: SkipSerializingIf,
	default: bool,
	flatten: bool,
}

impl FieldAttrs {
	fn from_attrs(attrs: &[syn::Attribute]) -> syn::Result<Self> {
		reject_tsgen_attrs(attrs, "a field")?;
		let mut out = Self::default();
		for attr in attrs.iter().filter(|attr| attr.path().is_ident("serde")) {
			attr.parse_nested_meta(|meta| {
				if meta.path.is_ident("rename") {
					out.rename = parse_phase_names(&meta)?;
					return Ok(());
				}
				if out.skips.parse(&meta) {
					return Ok(());
				}
				if meta.path.is_ident("skip_serializing_if") {
					let value = meta.value()?;
					let predicate: syn::LitStr = value.parse()?;
					out.skip_serializing_if = SkipSerializingIf::from_predicate(&predicate);
					return Ok(());
				}
				if meta.path.is_ident("default") {
					if meta.input.peek(syn::Token![=]) {
						let value = meta.value()?;
						let _: syn::LitStr = value.parse()?;
					}
					out.default = true;
					return Ok(());
				}
				if meta.path.is_ident("flatten") {
					out.flatten = true;
					return Ok(());
				}
				if meta.path.is_ident("serialize_with")
					|| meta.path.is_ident("deserialize_with")
					|| meta.path.is_ident("with")
				{
					return Err(meta.error(
						"custom serde codecs are not supported by TsGen derive; use a manual TsGen implementation",
					));
				}
				Err(meta.error(
					"unsupported serde field attribute for TsGen derive; the derive supports rename, skip, skip_serializing, skip_deserializing, skip_serializing_if, default, and flatten, and a type with another serde field attribute needs a manual TsGen implementation",
				))
			})?;
		}
		Ok(out)
	}
}

#[derive(Default)]
struct VariantAttrs {
	rename: PhaseNames,
	/// Rules for the fields of a struct variant.
	rename_all: PhaseRenameRules,
	skips: Skips,
}

impl VariantAttrs {
	fn from_attrs(attrs: &[syn::Attribute]) -> syn::Result<Self> {
		reject_tsgen_attrs(attrs, "an enum variant")?;
		let mut out = Self::default();
		for attr in attrs.iter().filter(|attr| attr.path().is_ident("serde")) {
			attr.parse_nested_meta(|meta| {
				if meta.path.is_ident("rename") {
					out.rename = parse_phase_names(&meta)?;
					return Ok(());
				}
				if meta.path.is_ident("rename_all") {
					out.rename_all = parse_rename_all(&meta)?;
					return Ok(());
				}
				if out.skips.parse(&meta) {
					return Ok(());
				}
				Err(meta.error(
					"unsupported serde enum variant attribute for TsGen derive; the derive supports rename, rename_all, skip, skip_serializing, and skip_deserializing, and a type with another serde variant attribute needs a manual TsGen implementation",
				))
			})?;
		}
		Ok(out)
	}
}

/// Rejects `#[tsgen(...)]` on `position`, such as a field, given that the
/// derive macro reads `tsgen` attributes only on the type that derives `TsGen`.
fn reject_tsgen_attrs(attrs: &[syn::Attribute], position: &str) -> syn::Result<()> {
	match attrs.iter().find(|attr| attr.path().is_ident("tsgen")) {
		Some(attr) => Err(syn::Error::new_spanned(
			attr,
			format!(
				"#[tsgen(...)] is accepted only on the type that derives TsGen, not on {position}"
			),
		)),
		None => Ok(()),
	}
}

/// `#[serde(skip)]`, `#[serde(skip_serializing)]`, and
/// `#[serde(skip_deserializing)]` on a field or variant.
#[derive(Default)]
struct Skips {
	skip: bool,
	skip_serializing: bool,
	skip_deserializing: bool,
}

impl Skips {
	/// Records `meta` and returns `true` when it is one of the skip
	/// attributes.
	fn parse(&mut self, meta: &syn::meta::ParseNestedMeta<'_>) -> bool {
		let slot = if meta.path.is_ident("skip") {
			&mut self.skip
		} else if meta.path.is_ident("skip_serializing") {
			&mut self.skip_serializing
		} else if meta.path.is_ident("skip_deserializing") {
			&mut self.skip_deserializing
		} else {
			return false;
		};
		*slot = true;
		true
	}

	/// Returns `true` when no skip attribute removes the field or variant
	/// from `phase`.
	fn retains(&self, phase: Phase) -> bool {
		!self.skip
			&& match phase {
				Phase::Serialize => !self.skip_serializing,
				Phase::Deserialize => !self.skip_deserializing,
			}
	}
}

/// The names from `#[serde(rename = "...")]` or
/// `#[serde(rename(serialize = "...", deserialize = "..."))]`.
#[derive(Default)]
struct PhaseNames {
	serialize: Option<String>,
	deserialize: Option<String>,
}

impl PhaseNames {
	/// Returns the name for `phase`: the explicit name, or `rust_name` with
	/// `rule` applied by `apply`.
	fn resolve(
		&self,
		phase: Phase,
		rust_name: &str,
		rule: Option<RenameRule>,
		apply: fn(RenameRule, &str) -> String,
	) -> String {
		let name = match phase {
			Phase::Serialize => &self.serialize,
			Phase::Deserialize => &self.deserialize,
		};
		name.clone().unwrap_or_else(|| {
			rule.map_or_else(|| rust_name.to_owned(), |rule| apply(rule, rust_name))
		})
	}
}

fn parse_phase_names(meta: &syn::meta::ParseNestedMeta<'_>) -> syn::Result<PhaseNames> {
	let (serialize, deserialize) = parse_phase_values(meta)?;
	Ok(PhaseNames {
		serialize: serialize.map(|value| value.value()),
		deserialize: deserialize.map(|value| value.value()),
	})
}

/// The rules from `#[serde(rename_all = "...")]` or
/// `#[serde(rename_all(serialize = "...", deserialize = "..."))]`, and the
/// same forms of `rename_all_fields`.
#[derive(Clone, Copy, Default)]
struct PhaseRenameRules {
	serialize: Option<RenameRule>,
	deserialize: Option<RenameRule>,
}

impl PhaseRenameRules {
	fn for_phase(self, phase: Phase) -> Option<RenameRule> {
		match phase {
			Phase::Serialize => self.serialize,
			Phase::Deserialize => self.deserialize,
		}
	}

	/// Returns these rules, with each phase that has no rule taking the rule
	/// from `fallback`.
	fn or(self, fallback: Self) -> Self {
		Self {
			serialize: self.serialize.or(fallback.serialize),
			deserialize: self.deserialize.or(fallback.deserialize),
		}
	}
}

fn parse_rename_all(meta: &syn::meta::ParseNestedMeta<'_>) -> syn::Result<PhaseRenameRules> {
	let (serialize, deserialize) = parse_phase_values(meta)?;
	Ok(PhaseRenameRules {
		serialize: serialize.as_ref().map(parse_rename_rule).transpose()?,
		deserialize: deserialize.as_ref().map(parse_rename_rule).transpose()?,
	})
}

/// Parses `name = "..."`, which applies to both phases, or
/// `name(serialize = "...", deserialize = "...")`.
fn parse_phase_values(
	meta: &syn::meta::ParseNestedMeta<'_>,
) -> syn::Result<(Option<syn::LitStr>, Option<syn::LitStr>)> {
	if meta.input.peek(syn::Token![=]) {
		let value: syn::LitStr = meta.value()?.parse()?;
		return Ok((Some(value.clone()), Some(value)));
	}
	let mut serialize = None;
	let mut deserialize = None;
	meta.parse_nested_meta(|nested| {
		let value: syn::LitStr = nested.value()?.parse()?;
		if nested.path.is_ident("serialize") {
			serialize = Some(value);
			return Ok(());
		}
		if nested.path.is_ident("deserialize") {
			deserialize = Some(value);
			return Ok(());
		}
		Err(nested.error("unsupported serde phase for TsGen derive"))
	})?;
	Ok((serialize, deserialize))
}

#[derive(Clone, Copy)]
enum RenameRule {
	Lower,
	Upper,
	Pascal,
	Camel,
	Snake,
	ScreamingSnake,
	Kebab,
	ScreamingKebab,
}

impl RenameRule {
	fn parse(value: &str) -> std::result::Result<Self, String> {
		match value {
			"lowercase" => Ok(Self::Lower),
			"UPPERCASE" => Ok(Self::Upper),
			"PascalCase" => Ok(Self::Pascal),
			"camelCase" => Ok(Self::Camel),
			"snake_case" => Ok(Self::Snake),
			"SCREAMING_SNAKE_CASE" => Ok(Self::ScreamingSnake),
			"kebab-case" => Ok(Self::Kebab),
			"SCREAMING-KEBAB-CASE" => Ok(Self::ScreamingKebab),
			_ => Err(format!("unsupported serde rename rule {value:?}")),
		}
	}

	fn apply_to_field(self, field: &str) -> String {
		match self {
			Self::Lower | Self::Snake => field.to_owned(),
			Self::Upper => field.to_ascii_uppercase(),
			Self::Pascal => pascal_case(field),
			Self::Camel => camel_case(&pascal_case(field)),
			Self::ScreamingSnake => field.to_ascii_uppercase(),
			Self::Kebab => field.replace('_', "-"),
			Self::ScreamingKebab => field.to_ascii_uppercase().replace('_', "-"),
		}
	}

	fn apply_to_variant(self, variant: &str) -> String {
		match self {
			Self::Pascal => variant.to_owned(),
			Self::Lower => variant.to_ascii_lowercase(),
			Self::Upper => variant.to_ascii_uppercase(),
			Self::Camel => camel_case(variant),
			Self::Snake => snake_case_variant(variant),
			Self::ScreamingSnake => snake_case_variant(variant).to_ascii_uppercase(),
			Self::Kebab => snake_case_variant(variant).replace('_', "-"),
			Self::ScreamingKebab => snake_case_variant(variant)
				.to_ascii_uppercase()
				.replace('_', "-"),
		}
	}
}

fn parse_rename_rule(value: &syn::LitStr) -> syn::Result<RenameRule> {
	RenameRule::parse(&value.value()).map_err(|err| syn::Error::new_spanned(value, err))
}

fn pascal_case(field: &str) -> String {
	field
		.split('_')
		.flat_map(|word| {
			let mut chars = word.chars();
			chars
				.next()
				.map(|first| first.to_ascii_uppercase())
				.into_iter()
				.chain(chars)
		})
		.collect()
}

fn camel_case(value: &str) -> String {
	let Some(first) = value.chars().next() else {
		return String::new();
	};
	let first_len = first.len_utf8();
	first.to_ascii_lowercase().to_string() + &value[first_len..]
}

fn snake_case_variant(variant: &str) -> String {
	let mut snake = String::new();
	for (index, ch) in variant.char_indices() {
		if index > 0 && ch.is_uppercase() {
			snake.push('_');
		}
		snake.push(ch.to_ascii_lowercase());
	}
	snake
}

fn is_option_type(ty: &Type) -> bool {
	let Type::Path(path) = ty else {
		return false;
	};
	path.path
		.segments
		.last()
		.is_some_and(|segment| segment.ident == format_ident!("Option"))
}

/// The type `T` of a field that is written as `Option<T>`.
fn option_inner_type(ty: &Type) -> Option<&Type> {
	let Type::Path(path) = ty else {
		return None;
	};
	let segment = path.path.segments.last()?;
	if segment.ident != format_ident!("Option") {
		return None;
	}
	nth_generic_type(segment, 0)
}

fn find_non_string_map_key(ty: &Type) -> Option<&Type> {
	match ty {
		Type::Array(array) => find_non_string_map_key(&array.elem),
		Type::Path(path) => {
			let segment = path.path.segments.last()?;
			let ident = &segment.ident;
			if ident == &format_ident!("HashMap") || ident == &format_ident!("BTreeMap") {
				let key_ty = nth_generic_type(segment, 0)?;
				if is_string_type(key_ty) {
					return None;
				}
				return Some(key_ty);
			}
			if ident == &format_ident!("Option")
				|| ident == &format_ident!("Vec")
				|| ident == &format_ident!("Box")
			{
				return nth_generic_type(segment, 0).and_then(find_non_string_map_key);
			}
			None
		}
		_ => None,
	}
}

fn nth_generic_type(segment: &syn::PathSegment, index: usize) -> Option<&Type> {
	let PathArguments::AngleBracketed(args) = &segment.arguments else {
		return None;
	};
	args.args
		.iter()
		.filter_map(|arg| match arg {
			GenericArgument::Type(ty) => Some(ty),
			_ => None,
		})
		.nth(index)
}

fn is_string_type(ty: &Type) -> bool {
	let Type::Path(path) = ty else {
		return false;
	};
	path.path
		.segments
		.last()
		.is_some_and(|segment| segment.ident == format_ident!("String"))
}

/// Collect every `#[doc = "..."]` attribute into one documentation string.
///
/// A Rust `///` comment desugars to `#[doc = "..."]` with a leading space on
/// each line. This function removes that space from each line, joins the
/// lines with newlines, and returns `None` for empty or whitespace-only
/// documentation.
fn collect_doc_comment(attrs: &[syn::Attribute]) -> Option<String> {
	let mut lines = Vec::new();
	for attr in attrs {
		if !attr.path().is_ident("doc") {
			continue;
		}
		let syn::Meta::NameValue(meta) = &attr.meta else {
			continue;
		};
		let syn::Expr::Lit(syn::ExprLit {
			lit: syn::Lit::Str(lit),
			..
		}) = &meta.value
		else {
			continue;
		};
		let value = lit.value();
		let trimmed = value.strip_prefix(' ').unwrap_or(&value);
		lines.push(trimmed.to_owned());
	}
	if lines.is_empty() {
		return None;
	}
	let joined = lines.join("\n");
	if joined.trim().is_empty() {
		None
	} else {
		Some(joined)
	}
}

#[cfg(test)]
mod tests {
	use quote::quote;

	use super::expand_ts_gen;

	#[test]
	fn rejected_shapes_report_their_reasons() {
		let cases = [
			(
				quote! { union Bits { int: u32, float: f32 } },
				"TsGen derive does not support unions",
			),
			(
				quote! { struct Event { #[serde(with = "timestamp")] at: Time } },
				"custom serde codecs are not supported",
			),
			(
				quote! { enum Event { At(#[serde(serialize_with = "timestamp")] Time) } },
				"custom serde codecs are not supported",
			),
			(
				quote! { struct Index { users: Option<HashMap<u32, User>> } },
				"non-string map keys are not supported",
			),
			(
				quote! {
					#[serde(tag = "kind")]
					enum Event { Moved(i32, i32) }
				},
				"cannot be used with tuple variants",
			),
			(
				quote! { struct Projected<T: Iterator> { value: T::Item } },
				"cannot represent a generic associated type projection",
			),
			(
				quote! { struct Event { #[tsgen(concrete)] at: u32 } },
				"#[tsgen(...)] is accepted only on the type that derives TsGen, not on a field",
			),
			(
				quote! { struct Pair(#[tsgen(concrete)] u32, u32); },
				"#[tsgen(...)] is accepted only on the type that derives TsGen, not on a field",
			),
			(
				quote! { enum Event { #[tsgen(concrete)] Started } },
				"#[tsgen(...)] is accepted only on the type that derives TsGen, not on an enum variant",
			),
			(
				quote! { struct Event { #[serde(alias = "when")] at: u32 } },
				"the derive supports rename, skip, skip_serializing, skip_deserializing, skip_serializing_if, default, and flatten, and a type with another serde field attribute needs a manual TsGen implementation",
			),
		];
		for (input, expected) in cases {
			let error = expand_ts_gen(
				syn::parse2(input.clone()).expect("parsing the input"),
				&quote! { ::primitives_rs::tsgen },
			)
			.expect_err("the derive rejects the input");
			assert!(
				error.to_string().contains(expected),
				"{input}: expected an error containing {expected:?}, got {error}"
			);
		}
	}
}
