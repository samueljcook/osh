//! Document contributions: what view loaders and task middleware contribute to
//! the head and to the `html`, `head`, and `body` attributes, through the
//! document methods of [`crate::Render`] and [`crate::Continue`].
//!
//! Ordinary strings are data. Vorma escapes them for their output context:
//! text for element content and attribute escaping for attribute values.
//! Source and markup pass through only in an explicit `Dangerous*` form, and
//! each form is valid only in its own context, so a bare string never becomes
//! JavaScript, CSS, or markup, and an escape hatch never disables an
//! unrelated protection. [`HeadChild::Json`] supplies the whole content of a
//! script data block. Every attribute value, including a raw one, still
//! receives quotation and attribute escaping, so its contents cannot add
//! attributes.
//!
//! Preparation turns contributed content into lowercase names, attribute
//! values as text, and serialized inner HTML. Head deduplication, the initial
//! document, and the navigation payload all read prepared content, so the two
//! renderings agree.

pub use primitives_rs::html::{AttrValue, Child as HeadChild, El as HeadEl};
pub(crate) use primitives_rs::html::{Attrs as AttrSettings, PreparedEl};
use std::collections::HashMap;
use std::fmt;

/// One contribution to the head, as a result's `head_items` method returns
/// it: what one call of `add_head_el`, `remove_head_el`, or
/// `add_head_dangerous_raw_html` contributed.
#[derive(Clone, Debug, PartialEq)]
pub enum HeadItem {
	/// An element that `add_head_el` added.
	El(HeadEl),
	/// An identity that `remove_head_el` removes from the elements that were
	/// placed before it.
	Remove(HeadEl),
	/// Markup that `add_head_dangerous_raw_html` added, which passes through
	/// without escaping or deduplication.
	///
	/// The markup has a key: the lowercase hexadecimal text of the first 16
	/// bytes of the BLAKE3 hash of the markup's UTF-8 bytes, 32 characters, so
	/// two items with the same markup have the same key. A document writes the
	/// item as the comment `<!--vorma-raw:KEY-->`, the markup unchanged, and
	/// the comment `<!--/vorma-raw:KEY-->`, with nothing between the comments
	/// and the markup, and a navigation payload carries the item as
	/// `{"raw_html": "…", "key": "KEY"}`. The browser client recognizes the
	/// item by its key because a script in the markup can change the markup
	/// in the browser's DOM.
	DangerousRawHtml(String),
}

/// One handler's document contribution. Nested loaders' attributes merge one
/// attribute at a time, from parent to child; head items are appended in order
/// and then deduplicated.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct DocumentParts {
	pub(crate) html_attrs: Vec<(String, AttrValue)>,
	pub(crate) head_attrs: Vec<(String, AttrValue)>,
	pub(crate) head_items: Vec<HeadItem>,
	pub(crate) body_attrs: Vec<(String, AttrValue)>,
}

/// How a head deduplication rule matches one attribute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttrMatch {
	/// The attribute has exactly this value.
	Exact(String),
	/// The attribute is present with any value.
	Wildcard,
}

impl From<&str> for AttrMatch {
	fn from(value: &str) -> Self {
		Self::Exact(value.to_owned())
	}
}

impl From<String> for AttrMatch {
	fn from(value: String) -> Self {
		Self::Exact(value)
	}
}

/// What a later element with a head identity replaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeadReplacement {
	/// A later element with the identity replaces the earlier one.
	LatestEl,
	/// The elements with the identity from the latest handler that contributes
	/// any replace all earlier ones, and all of that handler's own elements
	/// stay. The replacing set takes the place of the first element with the
	/// identity.
	LatestContribution,
}

/// One head identity in addition to the built-in identities. Structured
/// elements with this tag and matching attributes occupy one position, and
/// the rule's [`HeadReplacement`] says what a later element replaces there.
///
/// A handler removes an identity's elements with `remove_head_el`, which
/// accepts only an element that matches a built-in identity or one of these
/// rules.
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use]
pub struct HeadDedupeRule {
	pub(crate) tag: String,
	pub(crate) replacement: HeadReplacement,
	pub(crate) attrs: Vec<(String, AttrMatch)>,
}

impl HeadDedupeRule {
	/// Creates a rule that matches every element with this tag.
	pub fn new(tag: impl Into<String>, replacement: HeadReplacement) -> Self {
		Self {
			tag: tag.into(),
			replacement,
			attrs: Vec::new(),
		}
	}

	/// Sets the requirement for one attribute. A string requires exactly that
	/// value, and [`AttrMatch::Wildcard`] requires the attribute with any
	/// value.
	pub fn set_attr(mut self, name: impl Into<String>, matcher: impl Into<AttrMatch>) -> Self {
		self.attrs.push((name.into(), matcher.into()));
		self
	}
}

/// Contributed document content that is invalid for its output context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DocumentError {
	/// Where the content was contributed, such as `head_children[2].children[0]`.
	pub(crate) path: String,
	pub(crate) msg: String,
}

impl fmt::Display for DocumentError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(formatter, "{}: {}", self.path, self.msg)
	}
}

impl std::error::Error for DocumentError {}

/// A prepared head item. Raw HTML passes through without deduplication.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PreparedHeadItem {
	El(PreparedEl),
	RawHtml(String),
}

/// A change to the head: placing an item, or removing an identity that
/// earlier contributions placed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HeadChange {
	Place(PreparedHeadItem),
	/// The element carries an identity's tag and attributes.
	Remove(PreparedEl),
}

/// A head change with the position of the contribution that made it, in
/// plan order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContributedHeadChange {
	contribution: usize,
	change: HeadChange,
}

/// Prepared document contributions, merged in plan order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PreparedDocument {
	pub(crate) html_attrs: AttrSettings,
	pub(crate) head_attrs: AttrSettings,
	pub(crate) body_attrs: AttrSettings,
	head_changes: Vec<ContributedHeadChange>,
	/// How many handlers' contributions the document merges.
	contribution_count: usize,
}

impl PreparedDocument {
	/// Merges a later contribution: attributes one at a time, head changes
	/// appended with the later contributions numbered after the earlier ones.
	pub(crate) fn merge(&mut self, later: Self) {
		self.html_attrs.merge(later.html_attrs);
		self.head_attrs.merge(later.head_attrs);
		self.body_attrs.merge(later.body_attrs);
		let offset = self.contribution_count;
		self.head_changes
			.extend(
				later
					.head_changes
					.into_iter()
					.map(|change| ContributedHeadChange {
						contribution: change.contribution + offset,
						change: change.change,
					}),
			);
		self.contribution_count += later.contribution_count;
	}

	pub(crate) fn head_changes(&self) -> &[ContributedHeadChange] {
		&self.head_changes
	}
}

impl DocumentParts {
	/// Checks every value against its output context and serializes the
	/// content that does not depend on other contributions. A removal must
	/// name one of `head_rules`' identities.
	pub(crate) fn prepare(self, head_rules: &HeadRules) -> Result<PreparedDocument, DocumentError> {
		let mut head_changes = Vec::with_capacity(self.head_items.len());
		for (index, item) in self.head_items.into_iter().enumerate() {
			let path = format!("head[{index}]");
			let change = match item {
				HeadItem::DangerousRawHtml(html) => {
					HeadChange::Place(PreparedHeadItem::RawHtml(html))
				}
				HeadItem::El(el) => HeadChange::Place(PreparedHeadItem::El(prepare_el(el, &path)?)),
				HeadItem::Remove(el) => HeadChange::Remove(prepare_removal(el, head_rules, &path)?),
			};
			head_changes.push(ContributedHeadChange {
				contribution: 0,
				change,
			});
		}
		Ok(PreparedDocument {
			html_attrs: prepare_document_attrs("html", self.html_attrs)?,
			head_attrs: prepare_document_attrs("head", self.head_attrs)?,
			body_attrs: prepare_document_attrs("body", self.body_attrs)?,
			head_changes,
			contribution_count: 1,
		})
	}
}

fn document_error(path: String, msg: impl Into<String>) -> DocumentError {
	DocumentError {
		path,
		msg: msg.into(),
	}
}

fn prepare_document_attrs(
	el: &str,
	attrs: Vec<(String, AttrValue)>,
) -> Result<AttrSettings, DocumentError> {
	AttrSettings::parse(attrs)
		.map_err(|error| document_error(format!("{el}_{}", error.path), error.msg))
}

fn prepare_el(el: HeadEl, path: &str) -> Result<PreparedEl, DocumentError> {
	el.prepare().map_err(|error| {
		document_error(
			format!(
				"{path}{}",
				error.path.strip_prefix("el").unwrap_or(&error.path)
			),
			error.msg,
		)
	})
}

/// A removal's element: only the tag and the attributes that the matched
/// identity names, without children.
fn prepare_removal(
	el: HeadEl,
	head_rules: &HeadRules,
	path: &str,
) -> Result<PreparedEl, DocumentError> {
	if !el.children().is_empty() {
		return Err(document_error(
			format!("{path}.children"),
			"an element to remove carries no children",
		));
	}
	let prepared = prepare_el(el, path)?;
	let Some(rule) = head_rules.find_rule(&prepared) else {
		return Err(document_error(
			path.to_owned(),
			format!(
				"<{}> with these attributes matches no head identity; declare a HeadDedupeRule to make it removable",
				prepared.tag()
			),
		));
	};
	if let Some((name, _)) = prepared.attrs().iter().find(|(name, _)| {
		!rule
			.attrs
			.iter()
			.any(|(identity_name, _)| identity_name == name)
	}) {
		return Err(document_error(
			format!("{path}.attrs.{name}"),
			format!(
				"the {name} attribute is not part of the head identity of <{}>",
				prepared.tag()
			),
		));
	}
	Ok(prepared)
}

pub(crate) fn render_el(el: &PreparedEl) -> String {
	el.render()
}

/// A raw head HTML item is written between two comments that carry its key.
pub(crate) fn render_head_item(item: &PreparedHeadItem) -> String {
	match item {
		PreparedHeadItem::El(el) => render_el(el),
		PreparedHeadItem::RawHtml(html) => {
			let key = create_raw_head_html_key(html);
			format!("<!--vorma-raw:{key}-->{html}<!--/vorma-raw:{key}-->")
		}
	}
}

/// The key of a raw head HTML item: the lowercase hexadecimal text of the
/// first 16 bytes of the BLAKE3 hash of the item's HTML.
pub(crate) fn create_raw_head_html_key(html: &str) -> String {
	blake3::hash(html.as_bytes()).to_hex()[..32].to_owned()
}

/// The deduplicated head: the title, the meta elements, and every other item
/// in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PreparedHead {
	pub(crate) title: Option<PreparedEl>,
	pub(crate) meta: Vec<PreparedEl>,
	pub(crate) rest: Vec<PreparedHeadItem>,
}

struct CompiledHeadRule {
	priority: usize,
	replacement: HeadReplacement,
	attrs: Vec<(String, AttrMatch)>,
}

/// The built-in head identities followed by the application's rules, by tag.
pub(crate) struct HeadRules(HashMap<String, Vec<CompiledHeadRule>>);

fn built_in_head_rules() -> Vec<HeadDedupeRule> {
	use HeadReplacement::{LatestContribution, LatestEl};
	let mut rules = vec![HeadDedupeRule::new("title", LatestEl)];
	for name in ["description", "viewport", "robots"] {
		rules.push(
			HeadDedupeRule::new("meta", LatestEl)
				.set_attr("name", AttrMatch::Exact(name.to_owned())),
		);
	}
	rules.push(HeadDedupeRule::new("meta", LatestEl).set_attr("charset", AttrMatch::Wildcard));
	rules.push(
		HeadDedupeRule::new("link", LatestContribution)
			.set_attr("rel", AttrMatch::Exact("icon".to_owned())),
	);
	rules.push(
		HeadDedupeRule::new("link", LatestEl)
			.set_attr("rel", AttrMatch::Exact("canonical".to_owned())),
	);
	for property in [
		"og:title",
		"og:description",
		"og:url",
		"og:type",
		"og:locale",
		"og:site_name",
		"og:determiner",
	] {
		rules.push(
			HeadDedupeRule::new("meta", LatestEl)
				.set_attr("property", AttrMatch::Exact(property.to_owned())),
		);
	}
	rules
}

/// What identifies a structured head element: the rule that matches it, or
/// its whole shape when no rule matches.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum HeadIdentity {
	Rule(usize),
	Shape(ElShape),
}

/// A head identity retains its position after removal and owns elements only while occupied.
enum IdentitySlot {
	Vacant,
	Occupied {
		contribution: usize,
		els: Vec<PreparedEl>,
	},
}

enum HeadSlot {
	Raw(String),
	Identity(IdentitySlot),
}

impl HeadRules {
	/// Compiles the built-in rules and then the application's, preserving
	/// priority within each tag.
	pub(crate) fn new(application_rules: &[HeadDedupeRule]) -> Self {
		let mut by_tag: HashMap<String, Vec<CompiledHeadRule>> = HashMap::new();
		let rules = built_in_head_rules()
			.into_iter()
			.chain(application_rules.iter().cloned());
		for (priority, rule) in rules.enumerate() {
			by_tag
				.entry(rule.tag.to_ascii_lowercase())
				.or_default()
				.push(CompiledHeadRule {
					priority,
					replacement: rule.replacement,
					attrs: rule
						.attrs
						.into_iter()
						.map(|(name, value)| (name.to_ascii_lowercase(), value))
						.collect(),
				});
		}
		Self(by_tag)
	}

	fn find_rule(&self, el: &PreparedEl) -> Option<&CompiledHeadRule> {
		self.0.get(el.tag())?.iter().find(|rule| {
			rule.attrs.iter().all(|(name, expected)| {
				el.attrs()
					.iter()
					.find(|(actual_name, _)| actual_name == name)
					.is_some_and(|(_, actual)| match expected {
						AttrMatch::Exact(expected) => actual == expected,
						AttrMatch::Wildcard => true,
					})
			})
		})
	}

	/// Applies the head changes in order, then classifies the result.
	///
	/// A structured element takes the position of its identity. A
	/// `LatestEl` identity holds one element, which a later element
	/// replaces. A `LatestContribution` identity holds the elements of one
	/// contribution, which a later contribution's element replaces; the later
	/// contribution's other elements with the identity join it. A removal
	/// empties the identity and keeps its
	/// position. An element that no rule matches is identified by its whole
	/// shape. Raw HTML passes through in order.
	pub(crate) fn deduplicate(&self, changes: &[ContributedHeadChange]) -> PreparedHead {
		let mut slots: Vec<HeadSlot> = Vec::with_capacity(changes.len());
		let mut slot_by_identity: HashMap<HeadIdentity, usize> = HashMap::new();
		for ContributedHeadChange {
			contribution,
			change,
		} in changes
		{
			match change {
				HeadChange::Place(PreparedHeadItem::RawHtml(html)) => {
					slots.push(HeadSlot::Raw(html.clone()));
				}
				HeadChange::Place(PreparedHeadItem::El(el)) => {
					let (identity, replacement) = match self.find_rule(el) {
						Some(rule) => (HeadIdentity::Rule(rule.priority), rule.replacement),
						None => (
							HeadIdentity::Shape(ElShape::of(el)),
							HeadReplacement::LatestEl,
						),
					};
					match slot_by_identity.get(&identity) {
						None => {
							slot_by_identity.insert(identity, slots.len());
							slots.push(HeadSlot::Identity(IdentitySlot::Occupied {
								contribution: *contribution,
								els: vec![el.clone()],
							}));
						}
						Some(&index) => {
							let HeadSlot::Identity(slot) = &mut slots[index] else {
								unreachable!("an identity maps to an identity slot");
							};
							match slot {
								IdentitySlot::Occupied {
									contribution: own,
									els,
								} if replacement == HeadReplacement::LatestContribution
									&& *own == *contribution =>
								{
									els.push(el.clone())
								}
								_ => {
									*slot = IdentitySlot::Occupied {
										contribution: *contribution,
										els: vec![el.clone()],
									}
								}
							}
						}
					}
				}
				HeadChange::Remove(el) => {
					let Some(rule) = self.find_rule(el) else {
						continue;
					};
					if let Some(&index) = slot_by_identity.get(&HeadIdentity::Rule(rule.priority)) {
						let HeadSlot::Identity(slot) = &mut slots[index] else {
							unreachable!("an identity maps to an identity slot");
						};
						*slot = IdentitySlot::Vacant;
					}
				}
			}
		}
		let mut head = PreparedHead::default();
		let items = slots.into_iter().flat_map(|slot| match slot {
			HeadSlot::Raw(html) => vec![PreparedHeadItem::RawHtml(html)],
			HeadSlot::Identity(IdentitySlot::Occupied { els, .. }) => {
				els.into_iter().map(PreparedHeadItem::El).collect()
			}
			HeadSlot::Identity(IdentitySlot::Vacant) => Vec::new(),
		});
		for item in items {
			match item {
				PreparedHeadItem::El(el) if el.tag() == "title" => head.title = Some(el),
				PreparedHeadItem::El(el) if el.tag() == "meta" => head.meta.push(el),
				item => head.rest.push(item),
			}
		}
		head
	}
}

/// A structural identity for an element, independent of attribute order.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ElShape {
	tag: String,
	attrs: Vec<(String, String)>,
	inner_html: String,
}

impl ElShape {
	fn of(el: &PreparedEl) -> Self {
		let mut attrs = el.attrs().to_vec();
		attrs.sort_by(|left, right| left.0.cmp(&right.0));
		Self {
			tag: el.tag().to_owned(),
			attrs,
			inner_html: el.inner_html().to_owned(),
		}
	}
}
