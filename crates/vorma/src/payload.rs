//! The two browser payloads and the initial document. A navigation receives
//! the mutable head, the resolved document attributes, and the destination's
//! assets. An initial document expresses those facts as HTML and embeds only
//! page, build, and deployment facts. Property names are wire spellings.

use primitives_rs::{html::escaping::escape_json_text_for_html_script, path_matcher::Segment};
use serde::Serialize;
use serde::ser::{SerializeMap, Serializer};
use serde_json::value::RawValue;

use crate::build_output::ClientModule;
use crate::document::{
	AttrSettings, PreparedDocument, PreparedEl, PreparedHead, PreparedHeadItem,
	create_raw_head_html_key, render_el, render_head_item,
};
use crate::{outcome::PublicError, search::SearchShape};

/// Ordered string pairs serialized as one JSON object.
pub(crate) struct OrderedObject<'a>(pub(crate) Vec<(&'a str, &'a str)>);

impl OrderedObject<'_> {
	fn is_empty(&self) -> bool {
		self.0.is_empty()
	}
}

impl Serialize for OrderedObject<'_> {
	fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		let mut map = serializer.serialize_map(Some(self.0.len()))?;
		for (name, value) in &self.0 {
			map.serialize_entry(name, value)?;
		}
		map.end()
	}
}

#[derive(Serialize)]
pub(crate) struct CompiledPatternWire<'a> {
	/// Present when matcher encoding changed the authored spelling.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub(crate) pattern: Option<&'a str>,
	pub(crate) segments: &'a [Segment],
}

/// One matched position. Only successful positions have records.
#[derive(Serialize)]
pub(crate) struct RouteRecordWire<'a> {
	pub(crate) pattern: &'a str,
	#[serde(skip_serializing_if = "OrderedObject::is_empty")]
	pub(crate) params: OrderedObject<'a>,
	#[serde(skip_serializing_if = "<[String]>::is_empty")]
	pub(crate) splat_values: &'a [String],
	pub(crate) compiled_pattern: CompiledPatternWire<'a>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub(crate) search_shape: Option<&'a SearchShape>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub(crate) search_input: Option<&'a serde_json::Value>,
	/// The view module; null for a view without a client definition.
	pub(crate) import_url: Option<&'a str>,
	/// Absent for a position without a loader or with a loader that has output
	/// type `()`. Present as `null` when the output is JSON `null`.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub(crate) loader_output: Option<&'a RawValue>,
}

/// A failure's status and, when the failure is public, its declared error or
/// rejection. A page payload adds the failing position's index.
#[derive(Serialize)]
pub(crate) struct FailureWire<'a> {
	#[serde(skip_serializing_if = "Option::is_none")]
	pub(crate) index: Option<usize>,
	pub(crate) status: u16,
	#[serde(flatten)]
	pub(crate) public_error: Option<&'a PublicError>,
}

#[derive(Serialize)]
pub(crate) struct HeadElWire<'a> {
	tag: &'a str,
	#[serde(skip_serializing_if = "OrderedObject::is_empty")]
	attrs: OrderedObject<'a>,
	#[serde(skip_serializing_if = "str::is_empty")]
	html: &'a str,
}

#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum HeadItemWire<'a> {
	El(HeadElWire<'a>),
	RawHtml { raw_html: &'a str, key: String },
}

#[derive(Serialize)]
pub(crate) struct HeadSectionsWire<'a> {
	#[serde(skip_serializing_if = "Option::is_none")]
	title: Option<HeadElWire<'a>>,
	#[serde(skip_serializing_if = "Vec::is_empty")]
	meta: Vec<HeadElWire<'a>>,
	#[serde(skip_serializing_if = "Vec::is_empty")]
	rest: Vec<HeadItemWire<'a>>,
}

fn to_el_wire(el: &PreparedEl) -> HeadElWire<'_> {
	HeadElWire {
		tag: el.tag(),
		attrs: OrderedObject(
			el.attrs()
				.iter()
				.map(|(name, value)| (name.as_str(), value.as_str()))
				.collect(),
		),
		html: el.inner_html(),
	}
}

pub(crate) fn to_head_wire(head: &PreparedHead) -> HeadSectionsWire<'_> {
	HeadSectionsWire {
		title: head.title.as_ref().map(to_el_wire),
		meta: head.meta.iter().map(to_el_wire).collect(),
		rest: head
			.rest
			.iter()
			.map(|item| match item {
				PreparedHeadItem::El(el) => HeadItemWire::El(to_el_wire(el)),
				PreparedHeadItem::RawHtml(raw_html) => HeadItemWire::RawHtml {
					raw_html,
					key: create_raw_head_html_key(raw_html),
				},
			})
			.collect(),
	}
}

/// The resolved attributes of `html`, `head`, and `body`.
#[derive(Serialize)]
pub(crate) struct DocumentAttrsWire<'a> {
	#[serde(skip_serializing_if = "OrderedObject::is_empty")]
	html: OrderedObject<'a>,
	#[serde(skip_serializing_if = "OrderedObject::is_empty")]
	head: OrderedObject<'a>,
	#[serde(skip_serializing_if = "OrderedObject::is_empty")]
	body: OrderedObject<'a>,
}

impl DocumentAttrsWire<'_> {
	fn is_empty(&self) -> bool {
		self.html.is_empty() && self.head.is_empty() && self.body.is_empty()
	}
}

pub(crate) fn to_document_attrs_wire(document: &PreparedDocument) -> DocumentAttrsWire<'_> {
	DocumentAttrsWire {
		html: OrderedObject(document.html_attrs.resolve().collect()),
		head: OrderedObject(document.head_attrs.resolve().collect()),
		body: OrderedObject(document.body_attrs.resolve().collect()),
	}
}

/// A browser navigation payload.
#[derive(Serialize)]
pub(crate) struct NavPayload<'a> {
	pub(crate) routes: Vec<RouteRecordWire<'a>>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub(crate) failure: Option<FailureWire<'a>>,
	pub(crate) head: HeadSectionsWire<'a>,
	#[serde(skip_serializing_if = "DocumentAttrsWire::is_empty")]
	pub(crate) document_attrs: DocumentAttrsWire<'a>,
	#[serde(skip_serializing_if = "<[String]>::is_empty")]
	pub(crate) deps: &'a [String],
	#[serde(skip_serializing_if = "<[String]>::is_empty")]
	pub(crate) css_bundles: &'a [String],
	#[serde(skip_serializing_if = "<[StaticPublicAssetPreload]>::is_empty")]
	pub(crate) static_public_asset_preloads: &'a [StaticPublicAssetPreload],
}

/// The initial document's payload.
#[derive(Serialize)]
pub(crate) struct SsrPayload<'a> {
	pub(crate) routes: Vec<RouteRecordWire<'a>>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub(crate) failure: Option<FailureWire<'a>>,
	pub(crate) client_build_id: &'a str,
	#[serde(skip_serializing_if = "DocumentAttrsWire::is_empty")]
	pub(crate) document_attrs: DocumentAttrsWire<'a>,
	/// Present only in development.
	#[serde(skip_serializing_if = "std::ops::Not::not")]
	pub(crate) is_dev: bool,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub(crate) deployment_id: Option<&'a str>,
}

/// Ordered, deduplicated assets for a document and for a navigation, which
/// omits the entry's own assets.
pub(crate) struct PreparedAssets {
	pub(crate) document_dep_urls: Vec<String>,
	pub(crate) document_css_bundle_urls: Vec<String>,
	pub(crate) document_static_public_asset_preloads: Vec<StaticPublicAssetPreload>,
	pub(crate) nav_dep_urls: Vec<String>,
	pub(crate) nav_css_bundle_urls: Vec<String>,
	pub(crate) nav_static_public_asset_preloads: Vec<StaticPublicAssetPreload>,
}

pub(crate) fn prepare_assets<'a>(
	client_entry: &ClientModule,
	views: impl Iterator<Item = &'a ClientModule> + Clone,
) -> PreparedAssets {
	fn collect<'a, Asset: Clone + PartialEq + 'a>(
		entry: &[Asset],
		views: impl Iterator<Item = &'a [Asset]>,
	) -> (Vec<Asset>, Vec<Asset>) {
		let mut document: Vec<Asset> = Vec::new();
		for asset in entry {
			if !document.contains(asset) {
				document.push(asset.clone());
			}
		}
		let entry_count = document.len();
		for assets in views {
			for asset in assets {
				if !document.contains(asset) {
					document.push(asset.clone());
				}
			}
		}
		let nav = document[entry_count..].to_vec();
		(document, nav)
	}
	let (document_dep_urls, nav_dep_urls) = collect(
		&client_entry.dep_urls,
		views.clone().map(|view| view.dep_urls.as_slice()),
	);
	let (document_css_bundle_urls, nav_css_bundle_urls) = collect(
		&client_entry.css_bundle_urls,
		views.clone().map(|view| view.css_bundle_urls.as_slice()),
	);
	let get_preloads = |urls: &[String]| {
		urls.iter()
			.filter_map(|url| {
				static_public_asset_destination(url).map(|destination| StaticPublicAssetPreload {
					url: url.clone(),
					destination,
				})
			})
			.collect::<Vec<StaticPublicAssetPreload>>()
	};
	let view_preloads: Vec<Vec<StaticPublicAssetPreload>> = views
		.map(|view| get_preloads(&view.referenced_static_public_asset_urls))
		.collect();
	let (document_static_public_asset_preloads, nav_static_public_asset_preloads) = collect(
		&get_preloads(&client_entry.referenced_static_public_asset_urls),
		view_preloads.iter().map(Vec::as_slice),
	);
	PreparedAssets {
		document_dep_urls,
		document_css_bundle_urls,
		document_static_public_asset_preloads,
		nav_dep_urls,
		nav_css_bundle_urls,
		nav_static_public_asset_preloads,
	}
}

/// The request destination of a static public asset that a page preloads.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum StaticPublicAssetDestination {
	Image,
	Font,
}

impl StaticPublicAssetDestination {
	/// The destination's spelling in a preload link's `as` attribute and in a
	/// navigation payload.
	fn as_str(self) -> &'static str {
		match self {
			Self::Image => "image",
			Self::Font => "font",
		}
	}
}

impl Serialize for StaticPublicAssetDestination {
	fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		serializer.serialize_str(self.as_str())
	}
}

/// The request destination of an image or a font, which a page preloads. Any
/// other file has no destination, and a page does not preload it.
pub(crate) fn static_public_asset_destination(url: &str) -> Option<StaticPublicAssetDestination> {
	let path = url
		.split(['?', '#'])
		.next()
		.unwrap_or(url)
		.to_ascii_lowercase();
	let extension = path.rsplit_once('.').map_or("", |(_, extension)| extension);
	match extension {
		"avif" | "bmp" | "gif" | "ico" | "jpg" | "jpeg" | "png" | "svg" | "webp" => {
			Some(StaticPublicAssetDestination::Image)
		}
		"woff" | "woff2" | "ttf" | "otf" | "ttc" => Some(StaticPublicAssetDestination::Font),
		_ => None,
	}
}

/// A static public asset URL that a page preloads, with its request
/// destination. A navigation payload lists a page's preloads in the page's
/// order, each URL once, as `{"url": ..., "destination": "image" | "font"}`,
/// and the initial document writes a preload link for each of them.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub(crate) struct StaticPublicAssetPreload {
	url: String,
	destination: StaticPublicAssetDestination,
}

/// Everything the initial document renders.
pub(crate) struct DocumentInput<'a> {
	pub(crate) html_attrs: &'a AttrSettings,
	pub(crate) head_attrs: &'a AttrSettings,
	pub(crate) body_attrs: &'a AttrSettings,
	pub(crate) head: &'a PreparedHead,
	pub(crate) critical_css: &'a str,
	pub(crate) dev_head_html: Option<&'a str>,
	pub(crate) dev_body_prepend_html: Option<&'a str>,
	pub(crate) dev_body_html: Option<&'a str>,
	pub(crate) css_bundle_urls: &'a [String],
	pub(crate) module_preload_urls: Vec<&'a str>,
	pub(crate) static_public_asset_preloads: &'a [StaticPublicAssetPreload],
	pub(crate) entry_import_url: &'a str,
	pub(crate) payload_json: &'a str,
}

fn render_start_tag(tag: &str, attrs: &AttrSettings) -> String {
	let mut html = format!("<{tag}");
	html.push_str(&attrs.render());
	html.push('>');
	html
}

fn el(tag: &str, attrs: &[(&str, &str)]) -> primitives_rs::html::El {
	use primitives_rs::html::{AttrValue, El};
	let mut el = El::new(tag);
	for (name, value) in attrs {
		// Build-generated asset URLs may use any application-configured scheme.
		let value = match *name {
			"href" | "src" => AttrValue::DangerousRawUrl((*value).to_owned()),
			_ => AttrValue::Text((*value).to_owned()),
		};
		el = el.set_attr(*name, value);
	}
	el
}

fn render_asset_el(el: primitives_rs::html::El) -> String {
	el.render()
		.expect("framework element names and attribute contexts are valid")
}

/// Serializes the fixed document structure after head deduplication.
pub(crate) fn render_document(input: DocumentInput<'_>) -> String {
	let mut assets = Vec::new();
	if !input.critical_css.is_empty() {
		// Critical CSS is the build's own compiled output.
		assets.push(el("style", &[("id", "vorma-critical-css")]).add_child(
			primitives_rs::html::Child::DangerousRawCss(input.critical_css.to_owned()),
		));
	}
	for href in input.css_bundle_urls {
		assets.push(el(
			"link",
			&[
				("rel", "stylesheet"),
				("href", href),
				("data-vorma-css-bundle", href),
			],
		));
	}
	for href in &input.module_preload_urls {
		assets.push(el("link", &[("rel", "modulepreload"), ("href", href)]));
	}
	for StaticPublicAssetPreload { url, destination } in input.static_public_asset_preloads {
		let mut attrs = vec![
			("rel", "preload"),
			("href", url.as_str()),
			("as", destination.as_str()),
		];
		if *destination != StaticPublicAssetDestination::Image {
			attrs.push(("crossorigin", "anonymous"));
		}
		attrs.push(("data-vorma-static-public-asset", url));
		assets.push(el("link", &attrs));
	}
	let join = |items: Vec<String>| items.join("\n");
	let sections = [
		(
			"title",
			input.head.title.as_ref().map(render_el).unwrap_or_default(),
		),
		(
			"meta",
			join(input.head.meta.iter().map(render_el).collect()),
		),
		(
			"rest",
			join(input.head.rest.iter().map(render_head_item).collect()),
		),
		(
			"assets",
			join(assets.into_iter().map(render_asset_el).collect()),
		),
	];
	let head_html = sections
		.iter()
		.map(|(region, content)| {
			format!("<meta name=\"vorma-head\" content=\"{region}\">\n{content}")
		})
		.collect::<Vec<_>>()
		.join("\n");
	let body_html = [
		el("div", &[("id", "vorma-root")]),
		el(
			"script",
			&[("id", "vorma-data-json"), ("type", "application/json")],
		)
		.add_child(primitives_rs::html::Child::DangerousRawJs(
			escape_json_text_for_html_script(input.payload_json),
		)),
		el(
			"script",
			&[("type", "module"), ("src", input.entry_import_url)],
		),
	]
	.into_iter()
	.map(render_asset_el)
	.collect::<Vec<_>>()
	.join("\n");
	[
		"<!doctype html>\n",
		&render_start_tag("html", input.html_attrs),
		&render_start_tag("head", input.head_attrs),
		&head_html,
		input.dev_head_html.unwrap_or_default(),
		"</head>",
		&render_start_tag("body", input.body_attrs),
		input.dev_body_prepend_html.unwrap_or_default(),
		&body_html,
		input.dev_body_html.unwrap_or_default(),
		"</body></html>",
	]
	.concat()
}
