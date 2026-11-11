//! Content-Security-Policy hash sources.
use std::borrow::Cow;
use std::cell::RefCell;

use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::tree_builder::{
	ElemName, ElementFlags, NodeOrText, QuirksMode, TreeBuilderOpts, TreeSink,
};
use html5ever::{Attribute, LocalName, Namespace, ParseOpts, QualName};
use sha2::{Digest, Sha256};

/// Returns the Content-Security-Policy hash-source token for `content_bytes`:
/// `sha256-` followed by the standard-Base64 SHA-256 digest of the exact
/// bytes. Wrap the token in single quotes when placing it in a policy
/// directive (for example `script-src 'sha256-…'`). For the content of an
/// inline `<script>` or `<style>` element in an HTML text, use
/// [`csp_sha256_inline_content_hash_source`], which hashes the text that a
/// browser builds from that content.
pub fn csp_sha256_hash_source(content_bytes: &[u8]) -> String {
	let digest = Sha256::digest(content_bytes);
	format!(
		"sha256-{}",
		crate::encoding::binary_to_text::ByteEncoding::Base64.encode(&digest)
	)
}

/// Returns the Content-Security-Policy hash-source token for the content of
/// one inline `<script>` or `<style>` element when the caller supplies the text
/// between the element's start tag and its end tag in an HTML text.
///
/// A browser hashes the element's text as its HTML parser builds it, not the
/// text of the HTML document. Before it tokenizes the document, the parser
/// replaces each CR LF pair and each other CR with LF, and it reads each NUL
/// character in script or style text as U+FFFD. This function applies the same
/// replacements and returns [`csp_sha256_hash_source`] of the UTF-8 result:
/// `sha256-` followed by the standard-Base64 SHA-256 digest. Script and style
/// text has no character references, so the function decodes none. Wrap the
/// token in single quotes when placing it in a policy directive.
pub fn csp_sha256_inline_content_hash_source(content: &str) -> String {
	csp_sha256_hash_source(normalize_inline_text(content).as_bytes())
}

/// The text that the HTML parser builds from the content of a script or style
/// element: each CR LF pair and each other CR become LF, and each NUL becomes
/// U+FFFD.
fn normalize_inline_text(content: &str) -> Cow<'_, str> {
	if !content.contains(['\r', '\0']) {
		return Cow::Borrowed(content);
	}
	let mut text = String::with_capacity(content.len());
	let mut characters = content.chars().peekable();
	while let Some(character) = characters.next() {
		match character {
			'\r' => {
				characters.next_if_eq(&'\n');
				text.push('\n');
			}
			'\0' => text.push('\u{fffd}'),
			character => text.push(character),
		}
	}
	Cow::Owned(text)
}

/// The hash sources that a Content-Security-Policy needs for the inline
/// scripts and styles of an HTML text, as [`csp_sha256_inline_hash_sources`]
/// finds them. Each source is a token of
/// [`csp_sha256_inline_content_hash_source`], without quotes, in the order of
/// the elements in the text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CspInlineHashSources {
	/// One source for the content of each inline `<script>` element that a
	/// browser executes, for `script-src`.
	pub script_src: Vec<String>,
	/// One source for the content of each `<style>` element, for `style-src`.
	pub style_src: Vec<String>,
}

/// Parses an HTML document with scripting enabled and returns CSP hashes for its inline script
/// and style elements.
///
/// The HTML5 parser determines element context, `<noscript>` handling, character references, raw
/// script text, and end-of-document behavior. Script data blocks, external scripts, `<noscript>`
/// fallbacks, and `<template>` contents contribute no hashes. Only direct child text nodes of a
/// `<script>` or `<style>` element contribute to its inline text. Event handlers and style
/// attributes contribute no hashes.
///
/// Supply trusted HTML only. A browser that enforces a policy containing a returned script hash
/// permits matching inline script content anywhere in any document served with that policy; this
/// function does not sanitize HTML.
pub fn csp_sha256_inline_hash_sources(html: &str) -> CspInlineHashSources {
	let parse_opts = ParseOpts {
		tree_builder: TreeBuilderOpts {
			scripting_enabled: true,
			..TreeBuilderOpts::default()
		},
		..ParseOpts::default()
	};
	let sink = html5ever::parse_document(CspDomSink::new(), parse_opts).one(html);
	let nodes = sink.nodes.into_inner();
	let mut sources = CspInlineHashSources::default();
	let mut stack = vec![DOCUMENT_HANDLE];
	while let Some(node_id) = stack.pop() {
		let node = &nodes[node_id];
		for &child_id in node.children.iter().rev() {
			stack.push(child_id);
		}
		let DomNodeKind::Element(element) = &node.kind else {
			continue;
		};
		let is_svg = match &*element.name.ns {
			"http://www.w3.org/1999/xhtml" => false,
			"http://www.w3.org/2000/svg" => true,
			_ => continue,
		};
		let local_name = &*element.name.local;
		if !matches!(local_name, "script" | "style") {
			continue;
		}
		match local_name {
			"script"
				if !is_external_script(element, is_svg) && is_executed_script(element, is_svg) =>
			{
				let content = collect_direct_child_text(&nodes, node_id);
				sources
					.script_src
					.push(csp_sha256_hash_source(content.as_bytes()));
			}
			"style" => {
				let content = collect_direct_child_text(&nodes, node_id);
				sources
					.style_src
					.push(csp_sha256_hash_source(content.as_bytes()));
			}
			_ => {}
		}
	}
	sources
}

fn collect_direct_child_text(nodes: &[DomNode], element_id: usize) -> String {
	let mut content = String::new();
	for &child_id in &nodes[element_id].children {
		if let DomNodeKind::Text(text) = &nodes[child_id].kind {
			content.push_str(text);
		}
	}
	content
}

/// The JavaScript MIME type essences of the MIME Sniffing Standard.
const JAVASCRIPT_MIME_TYPE_ESSENCES: [&[u8]; 16] = [
	b"application/ecmascript",
	b"application/javascript",
	b"application/x-ecmascript",
	b"application/x-javascript",
	b"text/ecmascript",
	b"text/javascript",
	b"text/javascript1.0",
	b"text/javascript1.1",
	b"text/javascript1.2",
	b"text/javascript1.3",
	b"text/javascript1.4",
	b"text/javascript1.5",
	b"text/jscript",
	b"text/livescript",
	b"text/x-ecmascript",
	b"text/x-javascript",
];

fn find_unnamespaced_attr<'a>(element: &'a DomElement, local_name: &str) -> Option<&'a str> {
	element
		.attrs
		.iter()
		.find(|attr| attr.name.ns.is_empty() && &*attr.name.local == local_name)
		.map(|attr| &*attr.value)
}

fn is_external_script(element: &DomElement, is_svg: bool) -> bool {
	if is_svg {
		element.attrs.iter().any(|attr| {
			&*attr.name.local == "href"
				&& (attr.name.ns.is_empty() || &*attr.name.ns == "http://www.w3.org/1999/xlink")
		})
	} else {
		find_unnamespaced_attr(element, "src").is_some()
	}
}

fn is_executed_script(element: &DomElement, is_svg: bool) -> bool {
	let language = if is_svg {
		None
	} else {
		find_unnamespaced_attr(element, "language")
	};
	let type_string = match (find_unnamespaced_attr(element, "type"), language) {
		(Some(""), _) | (None, None | Some("")) => Cow::Borrowed("text/javascript"),
		(Some(value), _) => Cow::Borrowed(
			value.trim_matches(|character| matches!(character, '\t' | '\n' | '\x0c' | '\r' | ' ')),
		),
		(None, Some(value)) => Cow::Owned(format!("text/{value}")),
	};
	JAVASCRIPT_MIME_TYPE_ESSENCES
		.iter()
		.any(|mime| type_string.as_bytes().eq_ignore_ascii_case(mime))
		|| type_string.eq_ignore_ascii_case("module")
		|| (!is_svg
			&& ["importmap", "speculationrules"]
				.iter()
				.any(|kind| type_string.eq_ignore_ascii_case(kind)))
}

const DOCUMENT_HANDLE: usize = 0;

struct DomElement {
	name: QualName,
	attrs: Vec<Attribute>,
	template_contents: Option<usize>,
}

enum DomNodeKind {
	Document,
	Fragment,
	Comment,
	ProcessingInstruction,
	Element(DomElement),
	Text(String),
}

struct DomNode {
	parent: Option<usize>,
	children: Vec<usize>,
	kind: DomNodeKind,
}

struct CspDomSink {
	nodes: RefCell<Vec<DomNode>>,
}

impl CspDomSink {
	fn new() -> Self {
		Self {
			nodes: RefCell::new(vec![DomNode {
				parent: None,
				children: Vec::new(),
				kind: DomNodeKind::Document,
			}]),
		}
	}
}

fn detach_node(nodes: &mut [DomNode], node_id: usize) {
	if let Some(old_parent) = nodes[node_id].parent.take() {
		nodes[old_parent].children.retain(|&id| id != node_id);
	}
}

#[derive(Debug)]
struct OwnedElemName(QualName);

impl ElemName for OwnedElemName {
	fn ns(&self) -> &Namespace {
		&self.0.ns
	}

	fn local_name(&self) -> &LocalName {
		&self.0.local
	}
}

impl TreeSink for CspDomSink {
	type Handle = usize;
	type Output = Self;
	type ElemName<'a> = OwnedElemName;

	fn finish(self) -> Self::Output {
		self
	}

	fn parse_error(&self, _msg: Cow<'static, str>) {}

	fn get_document(&self) -> Self::Handle {
		DOCUMENT_HANDLE
	}

	fn elem_name<'a>(&'a self, target: &'a Self::Handle) -> Self::ElemName<'a> {
		let nodes = self.nodes.borrow();
		match &nodes[*target].kind {
			DomNodeKind::Element(element) => OwnedElemName(element.name.clone()),
			_ => panic!("TreeSink::elem_name requires an element handle"),
		}
	}

	fn is_mathml_annotation_xml_integration_point(&self, handle: &Self::Handle) -> bool {
		let nodes = self.nodes.borrow();
		let DomNodeKind::Element(element) = &nodes[*handle].kind else {
			return false;
		};
		if &*element.name.ns != "http://www.w3.org/1998/Math/MathML"
			|| &*element.name.local != "annotation-xml"
		{
			return false;
		}
		find_unnamespaced_attr(element, "encoding").is_some_and(|encoding| {
			encoding.eq_ignore_ascii_case("text/html")
				|| encoding.eq_ignore_ascii_case("application/xhtml+xml")
		})
	}

	fn create_element(
		&self,
		name: QualName,
		attrs: Vec<Attribute>,
		flags: ElementFlags,
	) -> Self::Handle {
		let mut nodes = self.nodes.borrow_mut();
		let template_contents = if flags.template {
			let fragment_id = nodes.len();
			nodes.push(DomNode {
				parent: None,
				children: Vec::new(),
				kind: DomNodeKind::Fragment,
			});
			Some(fragment_id)
		} else {
			None
		};
		let element_id = nodes.len();
		nodes.push(DomNode {
			parent: None,
			children: Vec::new(),
			kind: DomNodeKind::Element(DomElement {
				name,
				attrs,
				template_contents,
			}),
		});
		element_id
	}

	fn create_comment(&self, _text: StrTendril) -> Self::Handle {
		let mut nodes = self.nodes.borrow_mut();
		let id = nodes.len();
		nodes.push(DomNode {
			parent: None,
			children: Vec::new(),
			kind: DomNodeKind::Comment,
		});
		id
	}

	fn create_pi(&self, _target: StrTendril, _data: StrTendril) -> Self::Handle {
		let mut nodes = self.nodes.borrow_mut();
		let id = nodes.len();
		nodes.push(DomNode {
			parent: None,
			children: Vec::new(),
			kind: DomNodeKind::ProcessingInstruction,
		});
		id
	}

	fn append(&self, parent: &Self::Handle, child: NodeOrText<Self::Handle>) {
		let mut nodes = self.nodes.borrow_mut();
		match child {
			NodeOrText::AppendNode(child_id) => {
				detach_node(&mut nodes, child_id);
				nodes[child_id].parent = Some(*parent);
				nodes[*parent].children.push(child_id);
			}
			NodeOrText::AppendText(text) => {
				if let Some(&last_id) = nodes[*parent].children.last()
					&& let DomNodeKind::Text(existing) = &mut nodes[last_id].kind
				{
					existing.push_str(&text);
					return;
				}
				let text_id = nodes.len();
				nodes.push(DomNode {
					parent: Some(*parent),
					children: Vec::new(),
					kind: DomNodeKind::Text(text.to_string()),
				});
				nodes[*parent].children.push(text_id);
			}
		}
	}

	fn append_based_on_parent_node(
		&self,
		element: &Self::Handle,
		prev_element: &Self::Handle,
		child: NodeOrText<Self::Handle>,
	) {
		let has_parent = self.nodes.borrow()[*element].parent.is_some();
		if has_parent {
			self.append_before_sibling(element, child);
		} else {
			self.append(prev_element, child);
		}
	}

	fn append_doctype_to_document(
		&self,
		_name: StrTendril,
		_public_id: StrTendril,
		_system_id: StrTendril,
	) {
	}

	fn get_template_contents(&self, target: &Self::Handle) -> Self::Handle {
		let nodes = self.nodes.borrow();
		match &nodes[*target].kind {
			DomNodeKind::Element(element) => element
				.template_contents
				.expect("TreeSink::get_template_contents requires a template element"),
			_ => panic!("TreeSink::get_template_contents requires an element handle"),
		}
	}

	fn same_node(&self, x: &Self::Handle, y: &Self::Handle) -> bool {
		x == y
	}

	fn set_quirks_mode(&self, _mode: QuirksMode) {}

	fn append_before_sibling(&self, sibling: &Self::Handle, new_node: NodeOrText<Self::Handle>) {
		let mut nodes = self.nodes.borrow_mut();
		let Some(parent_id) = nodes[*sibling].parent else {
			return;
		};
		let Some(sibling_pos) = nodes[parent_id]
			.children
			.iter()
			.position(|&id| id == *sibling)
		else {
			return;
		};
		match new_node {
			NodeOrText::AppendNode(child_id) => {
				if child_id == *sibling {
					return;
				}
				detach_node(&mut nodes, child_id);
				let insert_pos = nodes[parent_id]
					.children
					.iter()
					.position(|&id| id == *sibling)
					.expect("sibling remains attached to parent");
				nodes[child_id].parent = Some(parent_id);
				nodes[parent_id].children.insert(insert_pos, child_id);
			}
			NodeOrText::AppendText(text) => {
				if sibling_pos > 0 {
					let prev_id = nodes[parent_id].children[sibling_pos - 1];
					if let DomNodeKind::Text(existing) = &mut nodes[prev_id].kind {
						existing.push_str(&text);
						return;
					}
				}
				let text_id = nodes.len();
				nodes.push(DomNode {
					parent: Some(parent_id),
					children: Vec::new(),
					kind: DomNodeKind::Text(text.to_string()),
				});
				nodes[parent_id].children.insert(sibling_pos, text_id);
			}
		}
	}

	fn add_attrs_if_missing(&self, target: &Self::Handle, attrs: Vec<Attribute>) {
		let mut nodes = self.nodes.borrow_mut();
		if let DomNodeKind::Element(element) = &mut nodes[*target].kind {
			for attr in attrs {
				if !element
					.attrs
					.iter()
					.any(|existing| existing.name == attr.name)
				{
					element.attrs.push(attr);
				}
			}
		}
	}

	fn remove_from_parent(&self, target: &Self::Handle) {
		detach_node(&mut self.nodes.borrow_mut(), *target);
	}

	fn reparent_children(&self, node: &Self::Handle, new_parent: &Self::Handle) {
		if node == new_parent {
			return;
		}
		let mut nodes = self.nodes.borrow_mut();
		let moved = std::mem::take(&mut nodes[*node].children);
		for &child_id in &moved {
			nodes[child_id].parent = Some(*new_parent);
		}
		nodes[*new_parent].children.extend(moved);
	}
}
