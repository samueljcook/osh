//! HTML construction with context-specific escaping and explicit raw-content forms.
//!
//! Ordinary strings are text. JavaScript, CSS, HTML, and JavaScript URLs require
//! a matching `DangerousRaw*` form. An SVG animation that targets a URL
//! attribute also requires `DangerousRawUrl`. These forms deliberately bypass
//! content protection; attribute values still receive quotation and escaping,
//! and the source of a `<style>` element still cannot end its element.
//!
//! Attribute sets preserve explicit removals and merge in caller-selected order.
//! No document contribution, head identity, or navigation policy applies here.

use super::escaping::{
	escape_attr_value, escape_json_text_for_html_script, escape_style_text_content,
	escape_text_content,
};
use super::names::is_valid_html_name;
use std::fmt;

/// An attribute value.
///
/// Text is literal and receives attribute escaping. Integers convert to their
/// text. Booleans do not convert because a boolean HTML attribute is on
/// whenever it is present: a boolean attribute such as `defer` takes `""` to be
/// present or [`AttrValue::Unset`] to be removed, and an enumerated attribute
/// such as `aria-hidden` takes the text `"true"` or `"false"`. Event handler
/// attributes take
/// [`AttrValue::DangerousRawJs`], `style` takes [`AttrValue::DangerousRawCss`],
/// `srcdoc` takes [`AttrValue::DangerousRawHtml`], and a URL attribute takes
/// [`AttrValue::DangerousRawUrl`] to supply a `javascript:` URL. The
/// `attributeName` of an SVG `set`, `animate`, `animateMotion`, or
/// `animateTransform` element takes [`AttrValue::DangerousRawUrl`] to name `href`
/// or `xlink:href` because the animation's `to`, `from`, and `values` text then
/// becomes that URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttrValue {
	/// Literal text.
	Text(String),
	/// Removes an earlier value of this attribute when attribute sets are merged.
	Unset,
	/// A URL that bypasses the `javascript:` URL check, or an SVG animation
	/// `attributeName` that bypasses the check against animating a URL attribute.
	DangerousRawUrl(String),
	/// Authored JavaScript for an event handler attribute.
	DangerousRawJs(String),
	/// Authored CSS for a `style` attribute.
	DangerousRawCss(String),
	/// Authored HTML for a `srcdoc` attribute.
	DangerousRawHtml(String),
}

impl From<String> for AttrValue {
	fn from(value: String) -> Self {
		Self::Text(value)
	}
}

impl From<&str> for AttrValue {
	fn from(value: &str) -> Self {
		Self::Text(value.to_owned())
	}
}

macro_rules! integer_attr_value {
	($($integer:ty),* $(,)?) => {
		$(
			impl From<$integer> for AttrValue {
				fn from(value: $integer) -> Self {
					Self::Text(value.to_string())
				}
			}
		)*
	};
}

integer_attr_value!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);

/// An HTML element, with the same shape at every depth.
#[derive(Clone, Debug, PartialEq)]
#[must_use]
pub struct El {
	tag: String,
	attrs: Vec<(String, AttrValue)>,
	children: Vec<Child>,
}

impl El {
	/// Returns the authored children.
	pub fn children(&self) -> &[Child] {
		&self.children
	}

	/// Checks output contexts and prepares this element for rendering.
	pub fn prepare(self) -> Result<PreparedEl, Error> {
		prepare_el(self, "el")
	}

	/// Checks output contexts and renders this element.
	pub fn render(self) -> Result<String, Error> {
		Ok(self.prepare()?.render())
	}

	/// Creates an element without attributes or children.
	pub fn new(tag: impl Into<String>) -> Self {
		Self {
			tag: tag.into(),
			attrs: Vec::new(),
			children: Vec::new(),
		}
	}

	/// Sets an attribute, which replaces an earlier value of that attribute.
	pub fn set_attr(mut self, name: impl Into<String>, value: impl Into<AttrValue>) -> Self {
		self.attrs.push((name.into(), value.into()));
		self
	}

	/// Adds a child after the existing ones.
	pub fn add_child(mut self, child: impl Into<Child>) -> Self {
		self.children.push(child.into());
		self
	}
}

/// A child of an HTML element: text, an element, the JSON content of a script
/// data block, complete script or style source, or markup that passes
/// through unchanged.
#[derive(Clone, Debug, PartialEq)]
pub enum Child {
	/// Text, which receives text escaping.
	Text(String),
	/// A nested element.
	El(El),
	/// The whole content of a `<script>` data block. The script's `type`
	/// remains an ordinary attribute.
	Json(serde_json::Value),
	/// Complete script source, valid only inside `<script>`.
	DangerousRawJs(String),
	/// Complete stylesheet source, valid only inside `<style>`. The renderer
	/// writes it through
	/// [`escape_style_text_content`](super::escaping::escape_style_text_content),
	/// so the source cannot end its element early and keeps its meaning as CSS.
	DangerousRawCss(String),
	/// Markup inserted unchanged.
	DangerousRawHtml(String),
}

impl From<&str> for Child {
	fn from(value: &str) -> Self {
		Self::Text(value.to_owned())
	}
}

impl From<String> for Child {
	fn from(value: String) -> Self {
		Self::Text(value)
	}
}

impl From<El> for Child {
	fn from(value: El) -> Self {
		Self::El(value)
	}
}

/// HTML content that is invalid for its output context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
	/// The path to the invalid value, such as `el.children[2]`.
	pub path: String,
	/// A description of the invalid value and its required output context.
	pub msg: String,
}

impl fmt::Display for Error {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(formatter, "{}: {}", self.path, self.msg)
	}
}

impl std::error::Error for Error {}

/// An element with checked names, attributes, and serialized children.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedEl {
	tag: String,
	/// Attribute values as text, in authored order.
	attrs: Vec<(String, String)>,
	inner_html: String,
}

impl PreparedEl {
	/// Returns the lowercase tag.
	pub fn tag(&self) -> &str {
		&self.tag
	}
	/// Returns attribute names and unescaped values in authored order.
	pub fn attrs(&self) -> &[(String, String)] {
		&self.attrs
	}
	/// Returns serialized children.
	pub fn inner_html(&self) -> &str {
		&self.inner_html
	}
	/// Serializes this element, escaping attribute values once.
	pub fn render(&self) -> String {
		render_el(self)
	}
}

/// Attribute settings in first-set order. `None` records `Unset` so that a
/// later merge can remove an earlier value, and a later setting replaces an
/// earlier one in place.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Attrs(Vec<(String, Option<String>)>);

impl Attrs {
	/// Checks attributes and retains explicit removals for later composition.
	pub fn parse(attrs: impl IntoIterator<Item = (String, AttrValue)>) -> Result<Self, Error> {
		let mut settings = Self::default();
		for (authored_name, value) in attrs {
			let name = authored_name.to_ascii_lowercase();
			let prepared = prepare_attr_value(
				ElSite {
					tag: "element",
					path: "attrs",
				},
				&name,
				value,
			)?;
			settings.set(name, prepared);
		}
		Ok(settings)
	}

	/// Serializes attributes, excluding explicit removals and escaping values once.
	pub fn render(&self) -> String {
		let mut html = String::new();
		write_attrs(&mut html, self.resolve());
		html
	}

	fn set(&mut self, name: String, value: Option<String>) {
		match self.0.iter_mut().find(|(existing, _)| *existing == name) {
			Some(entry) => entry.1 = value,
			None => self.0.push((name, value)),
		}
	}

	/// Applies later settings, preserving first-set order and explicit removals.
	pub fn merge(&mut self, later: Self) {
		for (name, value) in later.0 {
			self.set(name, value);
		}
	}

	/// The resolved attributes, without removed ones.
	pub fn resolve(&self) -> impl Iterator<Item = (&str, &str)> {
		self.0
			.iter()
			.filter_map(|(name, value)| value.as_deref().map(|value| (name.as_str(), value)))
	}
}

fn html_error(path: String, msg: impl Into<String>) -> Error {
	Error {
		path,
		msg: msg.into(),
	}
}

const VOID_ELS: &[&str] = &[
	"area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "track",
	"wbr",
];

/// Attributes with a value that is one URL. Composite grammars, such as `srcset`
/// and `ping`, are not URLs.
const URL_ATTRS: &[&str] = &[
	"action",
	"background",
	"cite",
	"codebase",
	"data",
	"formaction",
	"href",
	"longdesc",
	"manifest",
	"poster",
	"profile",
	"src",
	"xlink:href",
];

/// SVG animation elements, in lowercase, with an `attributeName` attribute that
/// selects the attribute set by their `to`, `from`, and `values` text.
const SVG_ANIMATION_ELS: &[&str] = &["set", "animate", "animatemotion", "animatetransform"];

/// Where an element is: its tag and the path that messages name.
#[derive(Clone, Copy)]
struct ElSite<'a> {
	tag: &'a str,
	path: &'a str,
}

fn is_void_el(tag: &str) -> bool {
	VOID_ELS.contains(&tag)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AttrCtx {
	Url,
	/// The `attributeName` of an SVG animation element.
	AnimationTarget,
	Js,
	Css,
	Html,
	Text,
}

impl AttrCtx {
	fn of(tag: &str, name: &str) -> Self {
		if URL_ATTRS.contains(&name) {
			Self::Url
		} else if name == "attributename" && SVG_ANIMATION_ELS.contains(&tag) {
			Self::AnimationTarget
		} else if name.starts_with("on") {
			Self::Js
		} else if name == "style" {
			Self::Css
		} else if name == "srcdoc" {
			Self::Html
		} else {
			Self::Text
		}
	}

	/// The context of the raw form that this context accepts.
	fn raw_form_ctx(self) -> Self {
		match self {
			Self::AnimationTarget => Self::Url,
			ctx => ctx,
		}
	}

	fn raw_form(self) -> &'static str {
		match self {
			Self::Url | Self::AnimationTarget => "DangerousRawUrl",
			Self::Js => "DangerousRawJs",
			Self::Css => "DangerousRawCss",
			Self::Html | Self::Text => "DangerousRawHtml",
		}
	}
}

/// Recognizes a `javascript:` URL as browsers parse one: leading C0 controls
/// and spaces are ignored, ASCII tabs and newlines inside the scheme are
/// removed, and the scheme is case-insensitive. This is React's recognizer.
fn is_javascript_url(value: &str) -> bool {
	let mut characters = value
		.trim_start_matches(|character: char| character <= '\u{1F}' || character == ' ')
		.chars()
		.filter(|character| !matches!(character, '\r' | '\n' | '\t'));
	"javascript:".chars().all(|expected| {
		characters
			.next()
			.is_some_and(|actual| actual.eq_ignore_ascii_case(&expected))
	})
}

/// Recognizes an `attributeName` value that makes an SVG animation set a URL
/// attribute: `href` or `xlink:href` in any ASCII case, after trimming ASCII
/// whitespace.
fn is_url_attr_name(value: &str) -> bool {
	let name = value.trim_ascii();
	name.eq_ignore_ascii_case("href") || name.eq_ignore_ascii_case("xlink:href")
}

/// One attribute value as text, or `None` for `Unset`. A raw form must match
/// the attribute's context, a source context requires its raw form, a URL
/// attribute refuses a `javascript:` URL unless it is `DangerousRawUrl`, and an
/// SVG animation's `attributeName` refuses to name a URL attribute unless it is
/// `DangerousRawUrl`.
fn prepare_attr_value(
	site: ElSite<'_>,
	name: &str,
	value: AttrValue,
) -> Result<Option<String>, Error> {
	let ElSite { tag: el, path } = site;
	let location = format!("{path}.{name}");
	if !is_valid_html_name(name) {
		return Err(html_error(
			location,
			format!("{name:?} is not an attribute name"),
		));
	}
	let ctx = AttrCtx::of(el, name);
	let (form_ctx, text) = match value {
		AttrValue::Unset => return Ok(None),
		AttrValue::Text(text) => (None, text),
		AttrValue::DangerousRawUrl(text) => (Some(AttrCtx::Url), text),
		AttrValue::DangerousRawJs(text) => (Some(AttrCtx::Js), text),
		AttrValue::DangerousRawCss(text) => (Some(AttrCtx::Css), text),
		AttrValue::DangerousRawHtml(text) => (Some(AttrCtx::Html), text),
	};
	match form_ctx {
		Some(form_ctx) if form_ctx == ctx.raw_form_ctx() => Ok(Some(text)),
		Some(form_ctx) => Err(html_error(
			location,
			format!(
				"{} is not valid for the {name} attribute of <{el}>",
				form_ctx.raw_form()
			),
		)),
		None if matches!(ctx, AttrCtx::Js | AttrCtx::Css | AttrCtx::Html) => Err(html_error(
			location,
			format!(
				"the {name} attribute of <{el}> takes {}, not text",
				ctx.raw_form()
			),
		)),
		None if ctx == AttrCtx::Url && is_javascript_url(&text) => Err(html_error(
			location,
			format!(
				"the {name} attribute of <{el}> cannot be a javascript: URL; use DangerousRawUrl to supply one intentionally"
			),
		)),
		None if ctx == AttrCtx::AnimationTarget && is_url_attr_name(&text) => Err(html_error(
			location,
			format!(
				"the {name} attribute of <{el}> cannot name a URL attribute; use DangerousRawUrl to supply one intentionally"
			),
		)),
		None => Ok(Some(text)),
	}
}

/// An element's attributes as text, in authored order, without `Unset` ones.
fn prepare_el_attrs(
	site: ElSite<'_>,
	attrs: Vec<(String, AttrValue)>,
) -> Result<Vec<(String, String)>, Error> {
	let mut settings = Attrs::default();
	for (authored_name, value) in attrs {
		let name = authored_name.to_ascii_lowercase();
		let prepared = prepare_attr_value(site, &name, value)?;
		settings.set(name, prepared);
	}
	Ok(settings
		.0
		.into_iter()
		.filter_map(|(name, value)| value.map(|value| (name, value)))
		.collect())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ContentCtx {
	Script,
	Style,
	RawText,
	EscapableText,
	Markup,
}

impl ContentCtx {
	fn of(tag: &str) -> Self {
		match tag {
			"script" => Self::Script,
			"style" => Self::Style,
			"xmp" | "iframe" | "noembed" | "noframes" => Self::RawText,
			"title" | "textarea" => Self::EscapableText,
			_ => Self::Markup,
		}
	}
}

fn prepare_el(el: El, path: &str) -> Result<PreparedEl, Error> {
	let tag = el.tag.to_ascii_lowercase();
	if !is_valid_html_name(&tag) {
		return Err(html_error(
			path.to_owned(),
			format!("{:?} is not an element name", el.tag),
		));
	}
	if tag == "plaintext" {
		return Err(html_error(
			path.to_owned(),
			"<plaintext> consumes the rest of the document and has no closing tag; use DangerousRawHtml to author it explicitly",
		));
	}
	let attrs = prepare_el_attrs(
		ElSite {
			tag: &tag,
			path: &format!("{path}.attrs"),
		},
		el.attrs,
	)?;
	let mut inner_html = render_children(ElSite { tag: &tag, path }, el.children)?;
	// The HTML parser removes one initial LF in these elements.
	if matches!(tag.as_str(), "textarea" | "pre" | "listing") && inner_html.starts_with('\n') {
		inner_html.insert(0, '\n');
	}
	Ok(PreparedEl {
		tag,
		attrs,
		inner_html,
	})
}

/// The serialized children of one element, checked against the element's
/// content context.
fn render_children(site: ElSite<'_>, children: Vec<Child>) -> Result<String, Error> {
	let ElSite { tag, path } = site;
	if !children.is_empty() && is_void_el(tag) {
		return Err(html_error(
			format!("{path}.children"),
			format!("<{tag}> cannot have children"),
		));
	}
	let ctx = ContentCtx::of(tag);
	let count = children.len();
	let mut rendered = String::new();
	for (index, child) in children.into_iter().enumerate() {
		let location = format!("{path}.children[{index}]");
		match child {
			Child::Json(value) => {
				if ctx != ContentCtx::Script {
					return Err(html_error(
						location,
						format!("a Json child is valid only in <script>, not <{tag}>"),
					));
				}
				if count != 1 {
					return Err(html_error(
						location,
						"a Json child must be the only child of its <script>",
					));
				}
				rendered.push_str(&escape_json_text_for_html_script(&value.to_string()));
			}
			Child::DangerousRawJs(source) => {
				if ctx != ContentCtx::Script {
					return Err(html_error(
						location,
						format!("DangerousRawJs is valid only in <script>, not <{tag}>"),
					));
				}
				rendered.push_str(&source);
			}
			Child::DangerousRawCss(source) => {
				if ctx != ContentCtx::Style {
					return Err(html_error(
						location,
						format!("DangerousRawCss is valid only in <style>, not <{tag}>"),
					));
				}
				rendered.push_str(&escape_style_text_content(&source));
			}
			Child::DangerousRawHtml(markup) => {
				if !matches!(ctx, ContentCtx::Markup | ContentCtx::RawText) {
					return Err(html_error(
						location,
						format!("DangerousRawHtml is not valid in <{tag}>"),
					));
				}
				rendered.push_str(&markup);
			}
			Child::Text(text) => match ctx {
				ContentCtx::RawText => {
					return Err(html_error(
						location,
						format!(
							"<{tag}> content takes DangerousRawHtml because HTML text escaping does not apply in this context"
						),
					));
				}
				ContentCtx::Script => {
					return Err(html_error(
						location,
						"<script> content takes DangerousRawJs or Json, not text",
					));
				}
				ContentCtx::Style => {
					return Err(html_error(
						location,
						"<style> content takes DangerousRawCss, not text",
					));
				}
				ContentCtx::EscapableText | ContentCtx::Markup => {
					rendered.push_str(&escape_text_content(&text));
				}
			},
			Child::El(el) => {
				if ctx != ContentCtx::Markup {
					return Err(html_error(
						location,
						format!("<{tag}> cannot contain elements"),
					));
				}
				rendered.push_str(&render_el(&prepare_el(el, &location)?));
			}
		}
	}
	Ok(rendered)
}

/// Serializes a prepared element; its attribute values are escaped once, here.
fn render_el(el: &PreparedEl) -> String {
	let mut html = format!("<{}", el.tag);
	write_attrs(
		&mut html,
		el.attrs
			.iter()
			.map(|(name, value)| (name.as_str(), value.as_str())),
	);
	html.push('>');
	if !is_void_el(&el.tag) {
		html.push_str(&el.inner_html);
		html.push_str("</");
		html.push_str(&el.tag);
		html.push('>');
	}
	html
}

/// Writes ` name="value"` for each attribute, escaping each value once.
fn write_attrs<'a>(html: &mut String, attrs: impl Iterator<Item = (&'a str, &'a str)>) {
	for (name, value) in attrs {
		html.push(' ');
		html.push_str(name);
		html.push_str("=\"");
		html.push_str(&escape_attr_value(value));
		html.push('"');
	}
}
