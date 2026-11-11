//! HTML element construction, context-specific escaping, and attribute composition.
//!
//! ```
//! use primitives_rs::html::{Child, El};
//!
//! let html = El::new("p")
//!     .set_attr("class", "notice")
//!     .add_child(Child::Text("A < B".into()))
//!     .render()?;
//! assert_eq!(html, "<p class=\"notice\">A &lt; B</p>");
//! # Ok::<(), primitives_rs::html::Error>(())
//! ```
//!
//! Raw-content variants require authored content. This builder checks output
//! contexts; it does not sanitize arbitrary HTML or enforce HTML content models.

mod elements;
pub use elements::{AttrValue, Attrs, Child, El, Error, PreparedEl};
pub mod escaping;
pub mod names;
