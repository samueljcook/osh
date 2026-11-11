//! The check that a JSON body has an object wherever its input type declares
//! one.
//!
//! serde's derived `Deserialize` for a struct also accepts a JSON array and
//! assigns its elements to the fields in declaration order. Vorma's generated
//! client never sends that form, and a client that sent it would silently
//! change meaning when the struct's field order changed, so an API endpoint
//! rejects a JSON body that has an array where its input type declares an
//! object.

use std::collections::HashMap;

use primitives_rs::tsgen::{FieldPresence, TsGen, TypeGraph, TypePhase, TypeRef, TypeRegistry};
use serde::de::{self, DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};

/// The positions at which an input type's JSON form is an object, derived once
/// from the type's TypeScript declarations.
pub(crate) struct JsonObjectPositions {
	defs: HashMap<String, TypeRef>,
	root: TypeRef,
}

impl JsonObjectPositions {
	/// The object positions of `T`'s deserialized form, or `None` when `T`
	/// declares no object so that no body of `T` needs the check. `None`
	/// also represents a type with declarations that cannot be collected, which fails the
	/// app's loading before any request arrives.
	pub(crate) fn of<T: TsGen>() -> Option<Self> {
		let mut registry = TypeRegistry::default();
		T::collect_type_defs_for(TypePhase::Deserialize, &mut registry).ok()?;
		let graph = TypeGraph::new(T::type_ref_for(TypePhase::Deserialize), registry).ok()?;
		let positions = Self {
			defs: graph
				.type_defs
				.into_iter()
				.map(|def| (def.key().to_owned(), def.type_ref().clone()))
				.collect(),
			root: graph.root,
		};
		positions
			.declares_object(&positions.root, &mut Vec::new())
			.then_some(positions)
	}

	/// Whether `body` is JSON with no array where the type declares an object.
	/// Every other mismatch is left to the type's own `Deserialize`.
	///
	/// At a union, such as an enum, the check looks only at the variants that
	/// the value has the shape of, and it admits the value
	/// when one of them admits it or when none fits so that the type's own
	/// `Deserialize` reports the mismatch. A tag selects one variant of a tagged
	/// enum. The empty object of a flattened `Option` counts only when no other
	/// variant fits. In an untagged enum with variants that fit the same value, a
	/// body that one variant admits is admitted, although serde decodes the
	/// first variant that it can, which may be another one. When a key repeats
	/// in one object, every occurrence must be admitted, and the last
	/// occurrence decides whether the object has a variant's shape.
	///
	/// The check reads the body once and builds no tree of its values. It
	/// checks each value against every type position that applies to it at
	/// once, and it skips a value that no type position declares with
	/// `IgnoredAny`, as the type's own `Deserialize` skips an unknown field,
	/// so the memory that it uses grows with the nesting depth of the type,
	/// not with the size of the body.
	pub(crate) fn admits(&self, body: &[u8]) -> bool {
		let mut deserializer = serde_json::Deserializer::from_slice(body);
		let root = [&self.root];
		let check = ValueCheck {
			positions: self,
			goals: &root,
		};
		match check.deserialize(&mut deserializer) {
			Ok(verdicts) => deserializer.end().is_ok() && verdicts[0].admits,
			Err(_) => false,
		}
	}

	/// Whether `type_ref` reaches an object. `named` holds the keys of the named
	/// types that enclose the current position.
	fn declares_object<'a>(&'a self, type_ref: &'a TypeRef, named: &mut Vec<&'a str>) -> bool {
		match type_ref {
			TypeRef::Parameter(_) | TypeRef::Documented { .. } => {
				unreachable!("TypeGraph contains concrete undocumented expressions")
			}
			TypeRef::Named { key, .. } => {
				if named.contains(&key.as_str()) {
					return false;
				}
				let Some(def) = self.defs.get(key) else {
					return false;
				};
				named.push(key);
				let declares = self.declares_object(def, named);
				named.pop();
				declares
			}
			TypeRef::Object(_) => true,
			TypeRef::Intersection(parts) | TypeRef::Union(parts) | TypeRef::Tuple(parts) => {
				parts.iter().any(|part| self.declares_object(part, named))
			}
			TypeRef::Nullable(inner) | TypeRef::Array(inner) | TypeRef::Record(_, inner) => {
				self.declares_object(inner, named)
			}
			TypeRef::Undefined
			| TypeRef::Null
			| TypeRef::Unknown
			| TypeRef::Bool
			| TypeRef::String
			| TypeRef::Number
			| TypeRef::Integer
			| TypeRef::WideInteger { .. }
			| TypeRef::FormData
			| TypeRef::Blob
			| TypeRef::StringLiteral(_)
			| TypeRef::Raw(_) => false,
		}
	}
}

/// The verdict of one type position for one JSON value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Verdict {
	/// Whether the value has the JSON shape that the position decodes: the
	/// same kind of JSON value and, for an object, every field that the
	/// position requires, with the value of each required field with a
	/// string-literal type, such as an enum's tag. An array also has the shape
	/// of an object because serde decodes a struct from an array.
	fits: bool,
	/// Whether the value has no array where the position declares an object.
	admits: bool,
}

const ADMITTED: Verdict = Verdict {
	fits: true,
	admits: true,
};

/// The kind of a JSON value, with the text of a string.
enum ValueKind<'a> {
	Null,
	Bool,
	Number,
	String(&'a str),
	Array,
	Object,
}

/// The check of one JSON value against several type positions at once. Its
/// result has one verdict for each goal, in the goals' order.
struct ValueCheck<'p> {
	positions: &'p JsonObjectPositions,
	goals: &'p [&'p TypeRef],
}

impl<'p> ValueCheck<'p> {
	/// The positions that read the value itself: the goals with their named
	/// types, unions, intersections, and nullable types resolved, each once.
	fn collect_leaves(&self) -> Vec<&'p TypeRef> {
		let mut leaves = Vec::new();
		let mut expanded_names = Vec::new();
		for goal in self.goals {
			self.collect_leaves_of(goal, &mut leaves, &mut expanded_names);
		}
		leaves
	}

	fn collect_leaves_of(
		&self,
		type_ref: &'p TypeRef,
		leaves: &mut Vec<&'p TypeRef>,
		expanded_names: &mut Vec<&'p str>,
	) {
		match type_ref {
			TypeRef::Parameter(_) | TypeRef::Documented { .. } => {
				unreachable!("TypeGraph contains concrete undocumented expressions")
			}
			TypeRef::Named { key, .. } => {
				if expanded_names.contains(&key.as_str()) {
					return;
				}
				expanded_names.push(key);
				if let Some(def) = self.positions.defs.get(key) {
					self.collect_leaves_of(def, leaves, expanded_names);
				}
			}
			TypeRef::Union(members) | TypeRef::Intersection(members) => {
				for member in members {
					self.collect_leaves_of(member, leaves, expanded_names);
				}
			}
			TypeRef::Nullable(inner) => self.collect_leaves_of(inner, leaves, expanded_names),
			leaf => push_distinct(leaves, leaf),
		}
	}

	/// The verdicts of the goals from the verdicts of the leaves.
	fn evaluate_goals(&self, value: &ValueKind<'_>, leaves: &LeafVerdicts<'p>) -> Vec<Verdict> {
		let mut evaluation = GoalEvaluation {
			positions: self.positions,
			value,
			leaves,
			entered_names: Vec::new(),
			named_verdicts: Vec::new(),
		};
		self.goals
			.iter()
			.map(|goal| evaluation.evaluate(goal))
			.collect()
	}
}

impl<'de> DeserializeSeed<'de> for ValueCheck<'_> {
	type Value = Vec<Verdict>;

	fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Vec<Verdict>, D::Error> {
		deserializer.deserialize_any(self)
	}
}

/// The leaves of a value check with their verdicts.
struct LeafVerdicts<'p> {
	leaves: Vec<&'p TypeRef>,
	verdicts: Vec<Verdict>,
}

impl LeafVerdicts<'_> {
	fn find(&self, leaf: &TypeRef) -> Verdict {
		self.leaves
			.iter()
			.position(|candidate| std::ptr::eq(*candidate, leaf))
			.map_or(ADMITTED, |index| self.verdicts[index])
	}
}

impl<'p> ValueCheck<'p> {
	/// Checks a value that has no nested values.
	fn check_scalar<E>(self, value: ValueKind<'_>) -> Result<Vec<Verdict>, E> {
		let leaves = self.collect_leaves();
		let verdicts = leaves
			.iter()
			.map(|leaf| check_leaf_without_children(leaf, &value))
			.collect();
		Ok(self.evaluate_goals(&value, &LeafVerdicts { leaves, verdicts }))
	}
}

/// The verdict of a leaf position for a value, from the value's kind alone.
/// For an array or an object, the caller then adds the verdicts of the nested
/// values to the leaves that read them.
fn check_leaf_without_children(leaf: &TypeRef, value: &ValueKind<'_>) -> Verdict {
	let fits = match leaf {
		TypeRef::Object(_) => matches!(value, ValueKind::Object | ValueKind::Array),
		TypeRef::Array(_) | TypeRef::Tuple(_) => matches!(value, ValueKind::Array),
		TypeRef::Record(_, _) => matches!(value, ValueKind::Object),
		TypeRef::StringLiteral(literal) => {
			matches!(value, ValueKind::String(text) if *text == literal.as_str())
		}
		TypeRef::String => matches!(value, ValueKind::String(_)),
		TypeRef::Number | TypeRef::Integer | TypeRef::WideInteger { .. } => {
			matches!(value, ValueKind::Number)
		}
		TypeRef::Bool => matches!(value, ValueKind::Bool),
		TypeRef::Undefined | TypeRef::Null => matches!(value, ValueKind::Null),
		TypeRef::Unknown | TypeRef::FormData | TypeRef::Blob | TypeRef::Raw(_) => true,
		TypeRef::Named { .. }
		| TypeRef::Union(_)
		| TypeRef::Intersection(_)
		| TypeRef::Nullable(_)
		| TypeRef::Parameter(_)
		| TypeRef::Documented { .. } => unreachable!("a leaf is not a composite position"),
	};
	Verdict {
		fits,
		admits: !(matches!(leaf, TypeRef::Object(_)) && matches!(value, ValueKind::Array)),
	}
}

fn push_distinct<'p>(list: &mut Vec<&'p TypeRef>, type_ref: &'p TypeRef) {
	if !list
		.iter()
		.any(|existing| std::ptr::eq(*existing, type_ref))
	{
		list.push(type_ref);
	}
}

/// The evaluation of the goals of one value from its leaves' verdicts.
struct GoalEvaluation<'v, 'p> {
	positions: &'p JsonObjectPositions,
	value: &'v ValueKind<'v>,
	leaves: &'v LeafVerdicts<'p>,
	/// The keys of the named types that the evaluation entered at this value,
	/// so that a cycle of named types that does not move into a nested value
	/// ends. A position inside such a cycle fits and is admitted.
	entered_names: Vec<&'p str>,
	/// The verdict of each named type that the evaluation completed.
	named_verdicts: Vec<(&'p str, Verdict)>,
}

impl<'p> GoalEvaluation<'_, 'p> {
	fn evaluate(&mut self, type_ref: &'p TypeRef) -> Verdict {
		match type_ref {
			TypeRef::Parameter(_) | TypeRef::Documented { .. } => {
				unreachable!("TypeGraph contains concrete undocumented expressions")
			}
			TypeRef::Named { key, .. } => {
				if let Some((_, verdict)) = self
					.named_verdicts
					.iter()
					.find(|(completed, _)| *completed == key.as_str())
				{
					return *verdict;
				}
				if self.entered_names.contains(&key.as_str()) {
					return ADMITTED;
				}
				let Some(def) = self.positions.defs.get(key) else {
					return ADMITTED;
				};
				self.entered_names.push(key);
				let verdict = self.evaluate(def);
				self.entered_names.pop();
				self.named_verdicts.push((key, verdict));
				verdict
			}
			TypeRef::Union(variants) => {
				let verdicts: Vec<(&TypeRef, Verdict)> = variants
					.iter()
					.map(|variant| (variant, self.evaluate(variant)))
					.collect();
				let mut fitting: Vec<_> = verdicts
					.iter()
					.filter(|(_, verdict)| verdict.fits)
					.collect();
				// An empty object is the form of a flattened `None`, which serde
				// decodes only when the other variants fail.
				if fitting.iter().any(|(variant, _)| !is_empty_object(variant)) {
					fitting.retain(|(variant, _)| !is_empty_object(variant));
				}
				Verdict {
					fits: !fitting.is_empty(),
					admits: fitting.is_empty() || fitting.iter().any(|(_, verdict)| verdict.admits),
				}
			}
			TypeRef::Intersection(parts) => {
				let verdicts: Vec<Verdict> = parts.iter().map(|part| self.evaluate(part)).collect();
				Verdict {
					fits: verdicts.iter().all(|verdict| verdict.fits),
					admits: verdicts.iter().all(|verdict| verdict.admits),
				}
			}
			TypeRef::Nullable(inner) => match self.value {
				ValueKind::Null => ADMITTED,
				_ => self.evaluate(inner),
			},
			leaf => self.leaves.find(leaf),
		}
	}
}

/// What one occurrence of a field gave the object leaf that declares it.
#[derive(Clone, Copy)]
struct FieldOccurrence {
	/// Whether the last occurrence has the field type's shape.
	fits: bool,
	/// Whether every occurrence is admitted.
	admits: bool,
}

impl<'de, 'p> Visitor<'de> for ValueCheck<'p> {
	type Value = Vec<Verdict>;

	fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter.write_str("a JSON value")
	}

	fn visit_bool<E: de::Error>(self, _value: bool) -> Result<Vec<Verdict>, E> {
		self.check_scalar(ValueKind::Bool)
	}

	fn visit_i64<E: de::Error>(self, _value: i64) -> Result<Vec<Verdict>, E> {
		self.check_scalar(ValueKind::Number)
	}

	fn visit_u64<E: de::Error>(self, _value: u64) -> Result<Vec<Verdict>, E> {
		self.check_scalar(ValueKind::Number)
	}

	fn visit_f64<E: de::Error>(self, _value: f64) -> Result<Vec<Verdict>, E> {
		self.check_scalar(ValueKind::Number)
	}

	fn visit_str<E: de::Error>(self, value: &str) -> Result<Vec<Verdict>, E> {
		self.check_scalar(ValueKind::String(value))
	}

	fn visit_unit<E: de::Error>(self) -> Result<Vec<Verdict>, E> {
		self.check_scalar(ValueKind::Null)
	}

	fn visit_none<E: de::Error>(self) -> Result<Vec<Verdict>, E> {
		self.check_scalar(ValueKind::Null)
	}

	fn visit_seq<A: SeqAccess<'de>>(self, mut items: A) -> Result<Vec<Verdict>, A::Error> {
		let leaves = self.collect_leaves();
		let mut verdicts: Vec<Verdict> = leaves
			.iter()
			.map(|leaf| check_leaf_without_children(leaf, &ValueKind::Array))
			.collect();
		let mut index = 0;
		loop {
			let mut goals: Vec<&TypeRef> = Vec::new();
			for leaf in &leaves {
				match leaf {
					TypeRef::Array(item) => push_distinct(&mut goals, item),
					TypeRef::Tuple(items) => {
						if let Some(item) = items.get(index) {
							push_distinct(&mut goals, item);
						}
					}
					_ => {}
				}
			}
			let item_verdicts = if goals.is_empty() {
				items.next_element::<IgnoredAny>()?.map(|_| Vec::new())
			} else {
				items.next_element_seed(ValueCheck {
					positions: self.positions,
					goals: &goals,
				})?
			};
			let Some(item_verdicts) = item_verdicts else {
				break;
			};
			for (leaf, verdict) in leaves.iter().zip(&mut verdicts) {
				let item = match leaf {
					TypeRef::Array(item) => Some(&**item),
					TypeRef::Tuple(items) => items.get(index),
					_ => None,
				};
				if let Some(item) = item {
					let position = goals
						.iter()
						.position(|goal| std::ptr::eq(*goal, item))
						.expect("each item position is a goal");
					verdict.admits &= item_verdicts[position].admits;
				}
			}
			index += 1;
		}
		Ok(self.evaluate_goals(&ValueKind::Array, &LeafVerdicts { leaves, verdicts }))
	}

	fn visit_map<A: MapAccess<'de>>(self, mut entries: A) -> Result<Vec<Verdict>, A::Error> {
		let leaves = self.collect_leaves();
		let mut verdicts: Vec<Verdict> = leaves
			.iter()
			.map(|leaf| check_leaf_without_children(leaf, &ValueKind::Object))
			.collect();
		// For each object leaf, what each of its fields received.
		let mut occurrences: Vec<Vec<Option<FieldOccurrence>>> = leaves
			.iter()
			.map(|leaf| match leaf {
				TypeRef::Object(fields) => vec![None; fields.len()],
				_ => Vec::new(),
			})
			.collect();
		while let Some(name) = entries.next_key_seed(FieldNameSeed { leaves: &leaves })? {
			let mut goals: Vec<&TypeRef> = Vec::new();
			for leaf in &leaves {
				match leaf {
					TypeRef::Object(fields) => {
						for field in fields.iter().filter(|field| Some(field.name()) == name) {
							push_distinct(&mut goals, field.type_ref());
						}
					}
					TypeRef::Record(_, item) => push_distinct(&mut goals, item),
					_ => {}
				}
			}
			if goals.is_empty() {
				entries.next_value::<IgnoredAny>()?;
				continue;
			}
			let value_verdicts = entries.next_value_seed(ValueCheck {
				positions: self.positions,
				goals: &goals,
			})?;
			let find = |type_ref: &TypeRef| {
				let position = goals
					.iter()
					.position(|goal| std::ptr::eq(*goal, type_ref))
					.expect("each field position is a goal");
				value_verdicts[position]
			};
			for ((leaf, verdict), fields) in leaves.iter().zip(&mut verdicts).zip(&mut occurrences)
			{
				match leaf {
					TypeRef::Object(declared) => {
						for (field, occurrence) in declared.iter().zip(fields.iter_mut()) {
							if Some(field.name()) != name {
								continue;
							}
							let value_verdict = find(field.type_ref());
							*occurrence = Some(FieldOccurrence {
								fits: value_verdict.fits,
								admits: occurrence.is_none_or(|earlier| earlier.admits)
									&& value_verdict.admits,
							});
						}
					}
					TypeRef::Record(_, item) => verdict.admits &= find(item).admits,
					_ => {}
				}
			}
		}
		for ((leaf, verdict), fields) in leaves.iter().zip(&mut verdicts).zip(&occurrences) {
			let TypeRef::Object(declared) = leaf else {
				continue;
			};
			*verdict = Verdict {
				fits: declared.iter().zip(fields).all(|(field, occurrence)| {
					match (field.presence(), field.type_ref(), occurrence) {
						(FieldPresence::Optional, _, _) => true,
						(FieldPresence::Required, _, None) => false,
						(FieldPresence::Required, TypeRef::StringLiteral(_), Some(occurrence)) => {
							occurrence.fits
						}
						(FieldPresence::Required, _, Some(_)) => true,
					}
				}),
				admits: fields.iter().flatten().all(|occurrence| occurrence.admits),
			};
		}
		Ok(self.evaluate_goals(&ValueKind::Object, &LeafVerdicts { leaves, verdicts }))
	}
}

/// Reads an object key as the name of a field that one of the object leaves
/// declares, or as `None` for a key that none declares, without keeping the
/// key's text.
struct FieldNameSeed<'v, 'p> {
	leaves: &'v [&'p TypeRef],
}

impl<'de, 'p> DeserializeSeed<'de> for FieldNameSeed<'_, 'p> {
	type Value = Option<&'p str>;

	fn deserialize<D: Deserializer<'de>>(
		self,
		deserializer: D,
	) -> Result<Option<&'p str>, D::Error> {
		deserializer.deserialize_str(self)
	}
}

impl<'de, 'p> Visitor<'de> for FieldNameSeed<'_, 'p> {
	type Value = Option<&'p str>;

	fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter.write_str("an object key")
	}

	fn visit_str<E: de::Error>(self, key: &str) -> Result<Option<&'p str>, E> {
		Ok(self.leaves.iter().find_map(|leaf| match leaf {
			TypeRef::Object(fields) => fields
				.iter()
				.map(|field| field.name())
				.find(|name| *name == key),
			_ => None,
		}))
	}
}

fn is_empty_object(type_ref: &TypeRef) -> bool {
	matches!(type_ref, TypeRef::Object(fields) if fields.is_empty())
}
