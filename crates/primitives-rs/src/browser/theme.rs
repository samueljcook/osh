//! Cookie-free light/dark/system theme initialization for browser documents.
//!
//! [`pre_paint_script`] emits JavaScript text for an inline script before
//! stylesheets. It reads local storage and applies the `light`/`dark` root class,
//! plus `system` when that preference is selected, while preserving other classes.
//! The matching `primitives-ts/browser/theme` installation handles ongoing changes.
//! Both sides must receive the same storage key and default preference.
//!
//! The script does not write storage or cookies. Missing, invalid, or inaccessible
//! storage uses the application's explicit default preference. The HTML response
//! need not vary by theme. Applications retain their CSS, controls, script element,
//! and CSP authorization. The `csp` feature provides
//! `http::csp::csp_sha256_inline_content_hash_source`, which returns the hash
//! source of the script text as a browser builds it from an inline
//! `<script>` element.

use crate::html::escaping::encode_json_string_for_html_script;

/// The selected theme preference; `System` follows the browser's color scheme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemePreference {
	/// Always use the light theme.
	Light,
	/// Always use the dark theme.
	Dark,
	/// Follow the browser's preferred color scheme.
	System,
}

impl ThemePreference {
	/// Returns the local-storage representation shared with the TypeScript API.
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Light => "light",
			Self::Dark => "dark",
			Self::System => "system",
		}
	}
}

/// Configuration shared with the browser's ongoing theme installation.
pub struct ThemeConfig<'a> {
	/// The local-storage key that contains `light`, `dark`, or `system`.
	pub storage_key: &'a str,
	/// The preference to use when storage is missing, invalid, or inaccessible.
	pub default_preference: ThemePreference,
}

/// Emits HTML-script-safe JavaScript text, without a surrounding script element.
///
/// The storage key is encoded as a script-safe JSON literal. The script is
/// independent of each request's cookies and can be shared in cached HTML.
///
/// ```
/// use primitives_rs::browser::theme::{ThemeConfig, ThemePreference, pre_paint_script};
/// let script = pre_paint_script(ThemeConfig {
///     storage_key: "app.theme",
///     default_preference: ThemePreference::System,
/// });
/// assert!(script.contains("app.theme"));
/// assert!(!script.contains("</script>"));
/// ```
pub fn pre_paint_script(config: ThemeConfig<'_>) -> String {
	let storage_key = encode_json_string_for_html_script(config.storage_key);
	let preference = encode_json_string_for_html_script(config.default_preference.as_str());
	format!(
		r#"(function(){{
const storageKey={storage_key};
let preference={preference};
try{{const stored=window.localStorage.getItem(storageKey);if(stored==="light"||stored==="dark"||stored==="system")preference=stored;}}catch{{}}
const resolved=preference==="system"?(window.matchMedia("(prefers-color-scheme: dark)").matches?"dark":"light"):preference;
const classes=window.document.documentElement.classList;
classes.remove("light","dark","system");classes.add(preference);if(preference==="system")classes.add(resolved);
}})();"#
	)
}
