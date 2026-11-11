#[cfg(feature = "form-data-multipart")]
use super::UploadedFile;
use super::{FormData, FormValue};
#[cfg(feature = "form-data-multipart")]
use serde::Deserialize;
use serde::de::value::{MapDeserializer, SeqDeserializer, StringDeserializer};
use serde::de::{self, DeserializeOwned, Deserializer, IntoDeserializer, Visitor};
use serde::forward_to_deserialize_any;
#[cfg(feature = "form-data-multipart")]
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;

/// Why a form's parts do not deserialize as the input type.
///
/// Messages never contain submitted values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormDeserializeError {
	/// The input type requires a field that the form does not contain.
	MissingField {
		/// The field's name in the input type.
		field: &'static str,
	},
	/// A single-valued field received several parts.
	MultipleValues,
	/// A text field received a file.
	UnexpectedFile,
	/// An uploaded-file field received text.
	UnexpectedText,
	/// Text does not parse as its field's type, or the input type rejected the form.
	InvalidValue,
}

impl fmt::Display for FormDeserializeError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::MissingField { field } => {
				write!(formatter, "form input is missing field `{field}`")
			}
			Self::MultipleValues => {
				formatter.write_str("a single-valued form field received several parts")
			}
			Self::UnexpectedFile => formatter.write_str("a text form field received a file"),
			Self::UnexpectedText => formatter.write_str("a file form field received text"),
			Self::InvalidValue => formatter.write_str("form input does not match its input type"),
		}
	}
}

impl std::error::Error for FormDeserializeError {}

impl de::Error for FormDeserializeError {
	/// Serde messages can quote submitted values, so they are discarded.
	fn custom<M: fmt::Display>(_message: M) -> Self {
		Self::InvalidValue
	}

	fn missing_field(field: &'static str) -> Self {
		Self::MissingField { field }
	}
}

impl FormData {
	/// Consumes the form and deserializes its fields into a struct or map.
	///
	/// Scalar fields require exactly one value. Sequences receive every value
	/// with the same name in request order. Missing optional fields are `None`;
	/// present empty text is `Some("")`. Missing sequences need `#[serde(default)]`.
	/// Text scalars use Rust's `FromStr`; booleans accept `true` and `false`.
	/// Unit enum variants, newtypes, and custom Serde validation are supported.
	/// Nested objects and nested sequences have no form encoding here. Names
	/// containing dots or brackets are literal names. Unknown fields are ignored
	/// unless the destination denies them. Ignored and rejected files are dropped.
	/// Custom Serde error messages are discarded because they can contain input.
	pub fn into_deserialized<T: DeserializeOwned>(self) -> Result<T, FormDeserializeError> {
		let mut fields: BTreeMap<String, Vec<FormValue>> = BTreeMap::new();
		for entry in self.entries {
			fields.entry(entry.name).or_default().push(entry.value);
		}
		T::deserialize(MapDeserializer::new(fields.into_iter().map(
			|(name, values)| {
				(
					name,
					FieldParts {
						values,
						allow_sequence: true,
					},
				)
			},
		)))
	}
}

#[cfg(feature = "form-data-multipart")]
thread_local! {
	/// The uploaded file that [`FieldParts`] hands to [`UploadedFile`]'s
	/// visitor. Serde's data model has no value for an open file, so the
	/// deserializer stores the file here and the visitor takes it in the same
	/// synchronous call.
	static PENDING_UPLOADED_FILE: RefCell<FileTransferState> = const { RefCell::new(FileTransferState::Vacant) };
}

#[cfg(feature = "form-data-multipart")]
enum FileTransferState {
	Vacant,
	Available(UploadedFile),
}

/// The newtype-struct name through which [`UploadedFile`] asks for a file part.
#[cfg(feature = "form-data-multipart")]
const UPLOADED_FILE_TOKEN: &str = "$primitives_rs::UploadedFile";

/// Every part submitted under one field name.
struct FieldParts {
	values: Vec<FormValue>,
	allow_sequence: bool,
}

impl FieldParts {
	fn into_single(self) -> Result<FormValue, FormDeserializeError> {
		let [part] = <[FormValue; 1]>::try_from(self.values)
			.map_err(|_| FormDeserializeError::MultipleValues)?;
		Ok(part)
	}

	fn into_text(self) -> Result<String, FormDeserializeError> {
		match self.into_single()? {
			FormValue::Text(text) => Ok(text),
			#[cfg(feature = "form-data-multipart")]
			FormValue::File(_) => Err(FormDeserializeError::UnexpectedFile),
		}
	}
}

impl IntoDeserializer<'_, FormDeserializeError> for FieldParts {
	type Deserializer = Self;

	fn into_deserializer(self) -> Self {
		self
	}
}

macro_rules! deserialize_parsed_text {
	($($method:ident => $visit:ident,)*) => {$(
		fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, FormDeserializeError> {
			let text = self.into_text()?;
			visitor.$visit(text.parse().map_err(|_| FormDeserializeError::InvalidValue)?)
		}
	)*};
}

impl<'de> Deserializer<'de> for FieldParts {
	type Error = FormDeserializeError;

	fn deserialize_any<V: Visitor<'de>>(
		self,
		visitor: V,
	) -> Result<V::Value, FormDeserializeError> {
		visitor.visit_string(self.into_text()?)
	}

	deserialize_parsed_text! {
		deserialize_bool => visit_bool,
		deserialize_i8 => visit_i8,
		deserialize_i16 => visit_i16,
		deserialize_i32 => visit_i32,
		deserialize_i64 => visit_i64,
		deserialize_i128 => visit_i128,
		deserialize_u8 => visit_u8,
		deserialize_u16 => visit_u16,
		deserialize_u32 => visit_u32,
		deserialize_u64 => visit_u64,
		deserialize_u128 => visit_u128,
		deserialize_f32 => visit_f32,
		deserialize_f64 => visit_f64,
		deserialize_char => visit_char,
	}

	fn deserialize_option<V: Visitor<'de>>(
		self,
		visitor: V,
	) -> Result<V::Value, FormDeserializeError> {
		visitor.visit_some(self)
	}

	fn deserialize_seq<V: Visitor<'de>>(
		self,
		visitor: V,
	) -> Result<V::Value, FormDeserializeError> {
		if !self.allow_sequence {
			return Err(FormDeserializeError::InvalidValue);
		}
		let els = self.values.into_iter().map(|part| Self {
			values: vec![part],
			allow_sequence: false,
		});
		visitor.visit_seq(SeqDeserializer::new(els))
	}

	fn deserialize_newtype_struct<V: Visitor<'de>>(
		self,
		name: &'static str,
		visitor: V,
	) -> Result<V::Value, FormDeserializeError> {
		#[cfg(feature = "form-data-multipart")]
		if name == UPLOADED_FILE_TOKEN {
			let FormValue::File(file) = self.into_single()? else {
				return Err(FormDeserializeError::UnexpectedText);
			};
			let previous = PENDING_UPLOADED_FILE
				.with(|pending| pending.replace(FileTransferState::Available(file)));
			let _scope = PendingFileScope(previous);
			return visitor.visit_unit();
		}
		let _ = name;
		visitor.visit_newtype_struct(self)
	}

	fn deserialize_enum<V: Visitor<'de>>(
		self,
		_name: &'static str,
		_variants: &'static [&'static str],
		visitor: V,
	) -> Result<V::Value, FormDeserializeError> {
		visitor.visit_enum(StringDeserializer::new(self.into_text()?))
	}

	fn deserialize_ignored_any<V: Visitor<'de>>(
		self,
		visitor: V,
	) -> Result<V::Value, FormDeserializeError> {
		visitor.visit_unit()
	}

	forward_to_deserialize_any! {
		str string bytes byte_buf unit unit_struct tuple tuple_struct map struct identifier
	}
}

#[cfg(feature = "form-data-multipart")]
impl<'de> Deserialize<'de> for UploadedFile {
	fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
		deserializer.deserialize_newtype_struct(UPLOADED_FILE_TOKEN, UploadedFileVisitor)
	}
}

#[cfg(feature = "form-data-multipart")]
struct UploadedFileVisitor;

#[cfg(feature = "form-data-multipart")]
impl<'de> Visitor<'de> for UploadedFileVisitor {
	type Value = UploadedFile;

	fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("a file part of form input")
	}

	fn visit_unit<E: de::Error>(self) -> Result<UploadedFile, E> {
		match PENDING_UPLOADED_FILE.with(|pending| {
			std::mem::replace(&mut *pending.borrow_mut(), FileTransferState::Vacant)
		}) {
			FileTransferState::Available(file) => Ok(file),
			FileTransferState::Vacant => Err(E::custom(
				"an uploaded file deserializes only from form input",
			)),
		}
	}
}

// Serde invokes this visitor synchronously. Restoring the prior slot isolates
// nested conversions and drops an unclaimed file on return, error, or panic.
#[cfg(feature = "form-data-multipart")]
struct PendingFileScope(FileTransferState);
#[cfg(feature = "form-data-multipart")]
impl Drop for PendingFileScope {
	fn drop(&mut self) {
		let previous = std::mem::replace(&mut self.0, FileTransferState::Vacant);
		PENDING_UPLOADED_FILE.with(|pending| pending.replace(previous));
	}
}
