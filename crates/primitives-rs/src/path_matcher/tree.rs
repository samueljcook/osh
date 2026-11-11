use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use super::language::is_javascript_whitespace;
use super::pattern::{FlatPattern, NestedPattern, Pattern, Segment};

/// One flat match with its registered value and its borrowed params and splat values.
#[derive(Debug)]
pub struct FlatMatch<'a, Value> {
	/// Selected pattern.
	pub pattern: &'a Pattern,
	/// Value registered with the pattern.
	pub value: &'a Value,
	/// Values of the dynamic segments by param name.
	pub params: BTreeMap<&'a str, &'a str>,
	/// Segments that a splat matched.
	pub splat_values: Vec<&'a str>,
	/// Whether the normalized path has a trailing slash after its segments.
	pub has_trailing_slash: bool,
}

/// One position in a nested match, with that position's own params and splat values.
#[derive(Debug)]
pub struct NestedMatch<'a, Value> {
	/// Selected pattern at this position.
	pub pattern: &'a Pattern,
	/// Value registered with this pattern.
	pub value: &'a Value,
	/// Params named by this pattern.
	pub params: BTreeMap<&'a str, &'a str>,
	/// Segments that this pattern's splat matched; only the final position can
	/// contain a splat.
	pub splat_values: Vec<&'a str>,
}

/// An immutable matcher that selects one pattern.
#[derive(Clone, Debug)]
pub struct FlatMatcher<Value> {
	tree: Tree<Value>,
}

impl<Value> FlatMatcher<Value> {
	/// Constructs a matcher from patterns and their values.
	///
	/// Two patterns occupy the same matching position when they have the same
	/// static segment text, dynamic segments in the same places, and the same
	/// final marker, if any: a trailing slash or a splat. Such patterns match
	/// the same paths, even when their parameter names differ. Returns
	/// [`DuplicatePositionError`] with every pattern that occupies the
	/// position of an earlier registered pattern.
	pub fn new(
		registrations: impl IntoIterator<Item = (FlatPattern, Value)>,
	) -> Result<Self, DuplicatePositionError> {
		let tree = Tree::build(
			registrations
				.into_iter()
				.map(|(pattern, value)| (pattern.into_pattern(), value)),
		)?;
		Ok(Self { tree })
	}

	/// Finds the most specific match after path normalization. The matcher
	/// compares and returns segments as raw text, as the module documentation
	/// describes.
	pub fn find_best_match<'a>(&'a self, path: &'a str) -> Option<FlatMatch<'a, Value>> {
		let path = NormalizedPath::new(path);
		let found = self.tree.find(
			&path.segments,
			Mode::Flat {
				has_trailing_slash: path.has_trailing_slash,
			},
		)?;
		let entry = &self.tree.entries[found.leaf];
		Some(FlatMatch {
			pattern: &entry.pattern,
			value: &entry.value,
			params: read_dynamic_params(&entry.pattern, &path.segments),
			splat_values: read_splat_values(&entry.pattern, &path.segments, found.splat_start),
			has_trailing_slash: path.has_trailing_slash,
		})
	}
}

/// An immutable matcher that selects one pattern and its registered ancestors.
#[derive(Clone, Debug)]
pub struct NestedMatcher<Value> {
	tree: Tree<Value>,
}

impl<Value> NestedMatcher<Value> {
	/// Constructs a matcher from patterns and their values.
	///
	/// Two patterns occupy the same matching position when they have the same
	/// static segment text, dynamic segments in the same places, and the same
	/// final marker, if any: an index segment or a splat. Such patterns match
	/// the same paths, even when their parameter names differ. Returns
	/// [`DuplicatePositionError`] with every pattern that occupies the
	/// position of an earlier registered pattern.
	pub fn new(
		registrations: impl IntoIterator<Item = (NestedPattern, Value)>,
	) -> Result<Self, DuplicatePositionError> {
		let tree = Tree::build(
			registrations
				.into_iter()
				.map(|(pattern, value)| (pattern.into_pattern(), value)),
		)?;
		Ok(Self { tree })
	}

	/// Finds an outermost-to-innermost chain after path normalization. The
	/// matcher compares and returns segments as raw text, as the module
	/// documentation describes.
	pub fn find_nested_matches<'a>(&'a self, path: &'a str) -> Option<Vec<NestedMatch<'a, Value>>> {
		let path = NormalizedPath::new(path);
		let found = self.tree.find(&path.segments, Mode::Nested)?;
		let mut matches = Vec::with_capacity(found.ancestors.len() + 1);
		for index in found.ancestors {
			let entry = &self.tree.entries[index];
			matches.push(NestedMatch {
				pattern: &entry.pattern,
				value: &entry.value,
				params: read_dynamic_params(&entry.pattern, &path.segments),
				splat_values: Vec::new(),
			});
		}
		let entry = &self.tree.entries[found.leaf];
		matches.push(NestedMatch {
			pattern: &entry.pattern,
			value: &entry.value,
			params: read_dynamic_params(&entry.pattern, &path.segments),
			splat_values: read_splat_values(&entry.pattern, &path.segments, found.splat_start),
		});
		Some(matches)
	}
}

/// Registered patterns occupy the matching positions of earlier registered
/// patterns, so each of them matches the same paths as an earlier pattern and
/// a matcher could select only one of the two.
#[derive(Clone, Debug)]
pub struct DuplicatePositionError {
	/// Each pattern that occupies the position of an earlier pattern, in
	/// registration order. It has at least one entry.
	pub duplicates: Vec<DuplicatePosition>,
}

/// A registered pattern that occupies the matching position of an earlier
/// registered pattern.
#[derive(Clone, Debug)]
pub struct DuplicatePosition {
	/// The pattern that was registered first at the position.
	pub earlier_pattern: Pattern,
	/// The position of the earlier pattern among the registrations that the
	/// constructor received, counted from zero.
	pub earlier_index: usize,
	/// The pattern that was registered later at the same position.
	pub later_pattern: Pattern,
	/// The position of the later pattern among the registrations that the
	/// constructor received, counted from zero.
	pub later_index: usize,
}

impl fmt::Display for DuplicatePosition {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(
			formatter,
			"pattern {:?} occupies the matching position of the earlier pattern {:?}, so both match the same paths",
			self.later_pattern.as_str(),
			self.earlier_pattern.as_str()
		)
	}
}

impl fmt::Display for DuplicatePositionError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		for (index, duplicate) in self.duplicates.iter().enumerate() {
			if index > 0 {
				formatter.write_str("; ")?;
			}
			duplicate.fmt(formatter)?;
		}
		Ok(())
	}
}

impl std::error::Error for DuplicatePositionError {}

// Arena indexes keep construction, cloning, traversal, and destruction
// independent of the pattern depth. Nodes contain no recursively owned children.
#[derive(Clone, Debug)]
struct Tree<Value> {
	nodes: Vec<Node>,
	entries: Vec<Entry<Value>>,
}

#[derive(Clone, Debug)]
struct Entry<Value> {
	pattern: Arc<Pattern>,
	value: Value,
}

// Each terminal slot holds the entry index of the pattern that ends at this
// node with that kind of ending.
#[derive(Clone, Debug, Default)]
struct Node {
	static_children: FxHashMap<String, usize>,
	dynamic_child: Option<usize>,
	// A pattern with no trailing slash, index segment, or splat.
	plain: Option<usize>,
	// A flat pattern with a trailing slash.
	trailing_slash: Option<usize>,
	// A nested pattern that ends with an index segment.
	index: Option<usize>,
	// A pattern that ends with a splat.
	splat: Option<usize>,
}

#[derive(Clone, Copy)]
enum Terminal {
	Plain,
	TrailingSlash,
	Index,
	Splat,
}

impl<Value> Tree<Value> {
	fn new() -> Self {
		Self {
			nodes: vec![Node::default()],
			entries: Vec::new(),
		}
	}

	/// A tree of every registration, or every registration that occupies the
	/// position of an earlier one.
	fn build(
		registrations: impl Iterator<Item = (Pattern, Value)>,
	) -> Result<Self, DuplicatePositionError> {
		let mut tree = Self::new();
		// The registration index of each entry.
		let mut registration_indexes = Vec::new();
		let mut duplicates = Vec::new();
		for (later_index, (pattern, value)) in registrations.enumerate() {
			let pattern = Arc::new(pattern);
			match tree.insert(Arc::clone(&pattern), value) {
				Ok(()) => registration_indexes.push(later_index),
				Err(earlier_entry) => duplicates.push(DuplicatePosition {
					earlier_pattern: Pattern::clone(&tree.entries[earlier_entry].pattern),
					earlier_index: registration_indexes[earlier_entry],
					later_pattern: Pattern::clone(&pattern),
					later_index,
				}),
			}
		}
		if duplicates.is_empty() {
			Ok(tree)
		} else {
			Err(DuplicatePositionError { duplicates })
		}
	}

	/// Inserts the pattern, or returns the entry index of the earlier pattern
	/// at its position.
	fn insert(&mut self, pattern: Arc<Pattern>, value: Value) -> Result<(), usize> {
		let mut node = 0;
		for segment in pattern.segments() {
			// The index that a new child receives.
			let next = self.nodes.len();
			let child = match segment {
				Segment::Static { value } => {
					let children = &mut self.nodes[node].static_children;
					match children.get(value) {
						Some(&child) => child,
						None => {
							children.insert(value.clone(), next);
							next
						}
					}
				}
				Segment::Dynamic { .. } => *self.nodes[node].dynamic_child.get_or_insert(next),
				Segment::Splat | Segment::Index => break,
			};
			if child == next {
				self.nodes.push(Node::default());
			}
			node = child;
		}
		let terminal = match pattern.segments().last() {
			Some(Segment::Index) => Terminal::Index,
			Some(Segment::Splat) => Terminal::Splat,
			_ if pattern.has_trailing_slash() => Terminal::TrailingSlash,
			_ => Terminal::Plain,
		};
		let slot = match terminal {
			Terminal::Plain => &mut self.nodes[node].plain,
			Terminal::TrailingSlash => &mut self.nodes[node].trailing_slash,
			Terminal::Index => &mut self.nodes[node].index,
			Terminal::Splat => &mut self.nodes[node].splat,
		};
		if let Some(index) = *slot {
			return Err(index);
		}
		*slot = Some(self.entries.len());
		self.entries.push(Entry { pattern, value });
		Ok(())
	}

	fn find(&self, segments: &[&str], mode: Mode) -> Option<TreeMatch> {
		let mut node_index = 0;
		let mut position = 0;
		let mut ancestors = Vec::new();
		let mut alternatives = Vec::new();
		loop {
			let node = &self.nodes[node_index];
			if position == segments.len() {
				let selected = match mode {
					Mode::Flat { has_trailing_slash } => has_trailing_slash
						.then_some(node.trailing_slash)
						.flatten()
						.or(node.plain)
						.or(node.splat.filter(|_| position == 0 || has_trailing_slash))
						.map(|leaf| (leaf, false)),
					Mode::Nested => node
						.index
						.or(node.splat.filter(|_| position == 0))
						.map(|leaf| (leaf, true))
						.or(node.plain.map(|leaf| (leaf, false))),
				};
				if let Some((leaf, plain_is_ancestor)) = selected {
					if plain_is_ancestor && let Some(plain) = node.plain {
						ancestors.push(plain);
					}
					return Some(TreeMatch {
						ancestors,
						leaf,
						splat_start: position,
					});
				}
			} else {
				if matches!(mode, Mode::Nested)
					&& let Some(plain) = node.plain
				{
					ancestors.push(plain);
				}
				let static_child = node.static_children.get(segments[position]).copied();
				if let Some(leaf) = node.splat {
					if static_child.is_none() && node.dynamic_child.is_none() {
						return Some(TreeMatch {
							ancestors,
							leaf,
							splat_start: position,
						});
					}
					alternatives.push(Alternative::Splat {
						leaf,
						position,
						ancestor_count: ancestors.len(),
					});
				}
				if let Some(child) = static_child {
					if let Some(dynamic) = node.dynamic_child {
						alternatives.push(Alternative::Node {
							node: dynamic,
							position: position + 1,
							ancestor_count: ancestors.len(),
						});
					}
					node_index = child;
					position += 1;
					continue;
				}
				if let Some(child) = node.dynamic_child {
					node_index = child;
					position += 1;
					continue;
				}
			}
			match alternatives.pop()? {
				Alternative::Splat {
					leaf,
					position,
					ancestor_count,
				} => {
					ancestors.truncate(ancestor_count);
					return Some(TreeMatch {
						ancestors,
						leaf,
						splat_start: position,
					});
				}
				Alternative::Node {
					node,
					position: next,
					ancestor_count,
				} => {
					ancestors.truncate(ancestor_count);
					node_index = node;
					position = next;
				}
			}
		}
	}
}

#[derive(Clone, Copy)]
enum Mode {
	Flat { has_trailing_slash: bool },
	Nested,
}

struct TreeMatch {
	ancestors: Vec<usize>,
	leaf: usize,
	splat_start: usize,
}

enum Alternative {
	Node {
		node: usize,
		position: usize,
		ancestor_count: usize,
	},
	Splat {
		leaf: usize,
		position: usize,
		ancestor_count: usize,
	},
}

struct NormalizedPath<'a> {
	segments: SmallVec<[&'a str; 8]>,
	has_trailing_slash: bool,
}

impl<'a> NormalizedPath<'a> {
	fn new(path: &'a str) -> Self {
		let trimmed = path.trim_matches(is_javascript_whitespace);
		let segments: SmallVec<_> = trimmed
			.split('/')
			.filter(|segment| !segment.is_empty())
			.collect();
		let has_trailing_slash = trimmed.ends_with('/') && !segments.is_empty();
		Self {
			segments,
			has_trailing_slash,
		}
	}
}

fn read_dynamic_params<'a>(
	pattern: &'a Pattern,
	segments: &[&'a str],
) -> BTreeMap<&'a str, &'a str> {
	pattern
		.segments()
		.iter()
		.zip(segments)
		.filter_map(|(segment, value)| match segment {
			Segment::Dynamic { param_name } => Some((param_name.as_str(), *value)),
			_ => None,
		})
		.collect()
}

fn read_splat_values<'a>(pattern: &Pattern, segments: &[&'a str], start: usize) -> Vec<&'a str> {
	if matches!(pattern.segments().last(), Some(Segment::Splat)) {
		segments[start..].to_vec()
	} else {
		Vec::new()
	}
}
