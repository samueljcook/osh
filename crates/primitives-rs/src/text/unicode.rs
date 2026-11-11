//! Versioned Unicode transformations with explicit UTF-8 input and output limits.
//!
//! [`UnicodeVersion`] selects immutable Unicode data and transformation rules.
//! Persist the version and the chosen operation alongside stored comparison keys.
//! A new Unicode version requires an explicit selection and may require rebuilding
//! an application's indexes. Unknown versions fail to parse.
//!
//! [`UnicodeVersion::canonical_caseless_key`] implements Unicode definition D145:
//! `NFD(toCasefold(NFD(input)))`. Equal keys indicate canonical caseless equality.
//! The operation preserves accents and does not provide locale collation,
//! confusable detection, filesystem-specific equality, or a filename policy.
//! Applications choose whether transforming a particular kind of input is correct.
//!
//! Transformations return ordinary strings. They do not provide secret-memory
//! handling. ICU4X buffers combining sequences during normalization; input limits
//! bound their length, but output limits alone do not bound that scratch space.
//! Output allocation failures are returned as errors; ICU4X's internal allocations
//! retain the Rust allocator's allocation-failure behavior.
//!
//! ```
//! use primitives_rs::text::unicode::{TransformLimits, UnicodeVersion};
//!
//! let unicode = UnicodeVersion::V18_0_0;
//! let limits = TransformLimits { max_input_size: 1024, max_output_size: 4096 };
//! let first = unicode.canonical_caseless_key("Straße", limits)?;
//! let second = unicode.canonical_caseless_key("STRASSE", limits)?;
//! assert_eq!(first, second);
//! # Ok::<(), primitives_rs::text::unicode::TransformError>(())
//! ```

use icu_casemap::CaseMapper;
use icu_normalizer::{ComposingNormalizer, DecomposingNormalizer};
use icu_properties::{CodePointMapData, CodePointSetData, props};
use std::{fmt, str::FromStr};
use writeable::Writeable;

/// Unicode general categories, such as letters, marks, numbers, and controls.
pub use icu_properties::props::GeneralCategory;

/// An explicitly selected Unicode data version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum UnicodeVersion {
	/// Unicode 18.0.0, implemented with pinned ICU4X 2.4.0 algorithms and data.
	#[cfg_attr(feature = "serde", serde(rename = "18.0.0"))]
	V18_0_0,
}

/// A Unicode normalization form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
pub enum NormalizationForm {
	/// Canonical decomposition followed by canonical composition.
	Nfc,
	/// Canonical decomposition.
	Nfd,
	/// Compatibility decomposition followed by canonical composition.
	Nfkc,
	/// Compatibility decomposition.
	Nfkd,
}

/// The mapping rules for full, context-insensitive case folding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
pub enum CaseFoldMode {
	/// Unicode default full case folding (CaseFolding.txt statuses C and F).
	Default,
	/// Full case folding with Turkic mappings for dotted and dotless I.
	Turkic,
}

/// UTF-8 byte limits for a text transformation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransformLimits {
	/// The maximum input size, checked before any transformation starts.
	pub max_input_size: usize,
	/// The maximum output size. Exceeding this limit returns no partial output.
	pub max_output_size: usize,
}

/// Options for Unicode normalization.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NormalizationOptions {
	/// The normalization form to produce.
	pub form: NormalizationForm,
	/// The input and output limits.
	pub limits: TransformLimits,
}

/// Options for full case folding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaseFoldOptions {
	/// The mapping rules to apply.
	pub mode: CaseFoldMode,
	/// The input and output limits.
	pub limits: TransformLimits,
}

/// Unicode properties of one scalar value under a selected version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CharProperties {
	/// The Unicode General_Category value.
	pub general_category: GeneralCategory,
	/// Whether the scalar has the Bidi_Control property.
	pub is_bidi_control: bool,
	/// Whether the scalar has the Default_Ignorable_Code_Point property.
	pub is_default_ignorable: bool,
	/// Whether the scalar has the White_Space property.
	pub is_whitespace: bool,
}

/// An unsupported Unicode version identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unsupported Unicode version")]
pub struct UnsupportedUnicodeVersion;

/// A text transformation exceeded a resource limit or could not allocate output.
#[derive(Debug, thiserror::Error)]
pub enum TransformError {
	/// The input exceeds its UTF-8 byte limit.
	#[error("Unicode input size {size} exceeds limit {limit}")]
	InputTooLarge {
		/// The input size in UTF-8 bytes.
		size: usize,
		/// The configured input limit in UTF-8 bytes.
		limit: usize,
	},
	/// The output would exceed its UTF-8 byte limit.
	#[error("Unicode output exceeds limit {limit}")]
	OutputTooLarge {
		/// The configured output limit in UTF-8 bytes.
		limit: usize,
	},
	/// Allocating the output string failed.
	#[error("could not allocate Unicode output")]
	OutputAllocation(#[source] std::collections::TryReserveError),
}

impl fmt::Display for UnicodeVersion {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(match self {
			Self::V18_0_0 => "18.0.0",
		})
	}
}

impl FromStr for UnicodeVersion {
	type Err = UnsupportedUnicodeVersion;

	fn from_str(value: &str) -> Result<Self, Self::Err> {
		match value {
			"18.0.0" => Ok(Self::V18_0_0),
			_ => Err(UnsupportedUnicodeVersion),
		}
	}
}

impl UnicodeVersion {
	/// Normalizes text into the requested form without truncation.
	pub fn normalize(
		self,
		input: &str,
		options: NormalizationOptions,
	) -> Result<String, TransformError> {
		check_input_size(input, options.limits)?;
		match self {
			Self::V18_0_0 => match options.form {
				NormalizationForm::Nfc => collect_output(
					ComposingNormalizer::new_nfc().normalize_iter(input.chars()),
					options.limits,
				),
				NormalizationForm::Nfd => collect_output(
					DecomposingNormalizer::new_nfd().normalize_iter(input.chars()),
					options.limits,
				),
				NormalizationForm::Nfkc => collect_output(
					ComposingNormalizer::new_nfkc().normalize_iter(input.chars()),
					options.limits,
				),
				NormalizationForm::Nfkd => collect_output(
					DecomposingNormalizer::new_nfkd().normalize_iter(input.chars()),
					options.limits,
				),
			},
		}
	}

	/// Applies full case folding, which can expand one scalar into several scalars.
	///
	/// This operation does not normalize text. Turkic folding does not preserve
	/// canonical equivalence; canonical caseless keys use Unicode default folding.
	pub fn full_case_fold(
		self,
		input: &str,
		options: CaseFoldOptions,
	) -> Result<String, TransformError> {
		check_input_size(input, options.limits)?;
		match self {
			Self::V18_0_0 => collect_output(
				input
					.chars()
					.flat_map(|scalar| fold_scalar(scalar, options.mode)),
				options.limits,
			),
		}
	}

	/// Produces a Unicode D145 canonical caseless comparison key.
	///
	/// The key is `NFD(toCasefold(NFD(input)))`. The initial NFD step is required
	/// for combining U+0345 and its decompositions. The output limit applies to
	/// the final key, without an intermediate string or intermediate output limit.
	pub fn canonical_caseless_key(
		self,
		input: &str,
		limits: TransformLimits,
	) -> Result<String, TransformError> {
		check_input_size(input, limits)?;
		match self {
			Self::V18_0_0 => {
				let nfd = DecomposingNormalizer::new_nfd();
				let folded = nfd
					.normalize_iter(input.chars())
					.flat_map(|scalar| fold_scalar(scalar, CaseFoldMode::Default));
				collect_output(nfd.normalize_iter(folded), limits)
			}
		}
	}

	/// Classifies one Unicode scalar without imposing an application policy.
	pub fn char_properties(self, scalar: char) -> CharProperties {
		match self {
			Self::V18_0_0 => CharProperties {
				general_category: CodePointMapData::<GeneralCategory>::new().get(scalar),
				is_bidi_control: CodePointSetData::new::<props::BidiControl>().contains(scalar),
				is_default_ignorable: CodePointSetData::new::<props::DefaultIgnorableCodePoint>()
					.contains(scalar),
				is_whitespace: CodePointSetData::new::<props::WhiteSpace>().contains(scalar),
			},
		}
	}
}

fn check_input_size(input: &str, limits: TransformLimits) -> Result<(), TransformError> {
	if input.len() > limits.max_input_size {
		return Err(TransformError::InputTooLarge {
			size: input.len(),
			limit: limits.max_input_size,
		});
	}
	Ok(())
}

fn collect_output(
	chars: impl Iterator<Item = char>,
	limits: TransformLimits,
) -> Result<String, TransformError> {
	let mut output = String::new();
	for scalar in chars {
		if scalar.len_utf8() > limits.max_output_size - output.len() {
			return Err(TransformError::OutputTooLarge {
				limit: limits.max_output_size,
			});
		}
		output
			.try_reserve(scalar.len_utf8())
			.map_err(TransformError::OutputAllocation)?;
		output.push(scalar);
	}
	Ok(output)
}

// Unicode 18 full case folding maps each scalar to at most three scalars.
// Keeping this buffer on the stack avoids allocating once per input scalar.
struct FoldedScalar {
	scalars: [char; 3],
	len: usize,
}

impl fmt::Write for FoldedScalar {
	fn write_str(&mut self, value: &str) -> fmt::Result {
		for scalar in value.chars() {
			*self.scalars.get_mut(self.len).ok_or(fmt::Error)? = scalar;
			self.len += 1;
		}
		Ok(())
	}
}

fn fold_scalar(scalar: char, mode: CaseFoldMode) -> impl Iterator<Item = char> {
	let mut utf8 = [0; 4];
	let input = scalar.encode_utf8(&mut utf8);
	let mapper = CaseMapper::new();
	let mut folded = FoldedScalar {
		scalars: ['\0'; 3],
		len: 0,
	};
	match mode {
		CaseFoldMode::Default => mapper.fold(input).write_to(&mut folded),
		CaseFoldMode::Turkic => mapper.fold_turkic(input).write_to(&mut folded),
	}
	.expect("Unicode 18 full case folding exceeds its three-scalar bound");
	folded.scalars.into_iter().take(folded.len)
}
