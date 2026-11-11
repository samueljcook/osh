use std::fmt::Display;

use serde::ser::{self, Error as _, Serialize, Serializer};

/// A value that serializes as the wrapped value does, through the same
/// serializer, and fails at the first `f32` or `f64` that is NaN or an
/// infinity.
///
/// JSON has no form for NaN or an infinity, and `serde_json` writes either of
/// them as `null`, so a float field would reach its reader as `null`. A value
/// wrapped in `FiniteFloats` fails to serialize instead, with a custom error
/// of the serializer. It works with every serializer, such as
/// `serde_json::to_writer`, `serde_json::to_value`,
/// `serde_json::value::to_raw_value`, and this module's serialization
/// functions.
///
/// The check runs in the serialization pass itself, so the wrapped value's
/// `Serialize` implementation runs once and the checked values are the
/// written values. The check covers floats at every depth, including map
/// keys and the contents of `Option`, newtype structs, and enum variants.
///
/// ```
/// use primitives_rs::serialization::json::FiniteFloats;
///
/// assert_eq!(serde_json::to_string(&FiniteFloats(&[1.5])).unwrap(), "[1.5]");
/// assert!(serde_json::to_string(&FiniteFloats(&[f64::NAN])).is_err());
/// ```
pub struct FiniteFloats<'a, T: ?Sized>(pub &'a T);

impl<T: Serialize + ?Sized> Serialize for FiniteFloats<'_, T> {
	fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		self.0.serialize(FiniteFloatSerializer(serializer))
	}
}

const NON_FINITE_FLOAT_MSG: &str = "a float is NaN or an infinity, which JSON cannot represent";

/// Forwards every call to its serializer and fails on a non-finite float.
struct FiniteFloatSerializer<S>(S);

/// Forwards every call to a compound serializer and checks each value that
/// the compound holds.
struct FiniteFloatCompound<C>(C);

impl<S: Serializer> Serializer for FiniteFloatSerializer<S> {
	type Ok = S::Ok;
	type Error = S::Error;
	type SerializeSeq = FiniteFloatCompound<S::SerializeSeq>;
	type SerializeTuple = FiniteFloatCompound<S::SerializeTuple>;
	type SerializeTupleStruct = FiniteFloatCompound<S::SerializeTupleStruct>;
	type SerializeTupleVariant = FiniteFloatCompound<S::SerializeTupleVariant>;
	type SerializeMap = FiniteFloatCompound<S::SerializeMap>;
	type SerializeStruct = FiniteFloatCompound<S::SerializeStruct>;
	type SerializeStructVariant = FiniteFloatCompound<S::SerializeStructVariant>;

	fn serialize_bool(self, value: bool) -> Result<S::Ok, S::Error> {
		self.0.serialize_bool(value)
	}

	fn serialize_i8(self, value: i8) -> Result<S::Ok, S::Error> {
		self.0.serialize_i8(value)
	}

	fn serialize_i16(self, value: i16) -> Result<S::Ok, S::Error> {
		self.0.serialize_i16(value)
	}

	fn serialize_i32(self, value: i32) -> Result<S::Ok, S::Error> {
		self.0.serialize_i32(value)
	}

	fn serialize_i64(self, value: i64) -> Result<S::Ok, S::Error> {
		self.0.serialize_i64(value)
	}

	fn serialize_i128(self, value: i128) -> Result<S::Ok, S::Error> {
		self.0.serialize_i128(value)
	}

	fn serialize_u8(self, value: u8) -> Result<S::Ok, S::Error> {
		self.0.serialize_u8(value)
	}

	fn serialize_u16(self, value: u16) -> Result<S::Ok, S::Error> {
		self.0.serialize_u16(value)
	}

	fn serialize_u32(self, value: u32) -> Result<S::Ok, S::Error> {
		self.0.serialize_u32(value)
	}

	fn serialize_u64(self, value: u64) -> Result<S::Ok, S::Error> {
		self.0.serialize_u64(value)
	}

	fn serialize_u128(self, value: u128) -> Result<S::Ok, S::Error> {
		self.0.serialize_u128(value)
	}

	fn serialize_f32(self, value: f32) -> Result<S::Ok, S::Error> {
		if !value.is_finite() {
			return Err(S::Error::custom(NON_FINITE_FLOAT_MSG));
		}
		self.0.serialize_f32(value)
	}

	fn serialize_f64(self, value: f64) -> Result<S::Ok, S::Error> {
		if !value.is_finite() {
			return Err(S::Error::custom(NON_FINITE_FLOAT_MSG));
		}
		self.0.serialize_f64(value)
	}

	fn serialize_char(self, value: char) -> Result<S::Ok, S::Error> {
		self.0.serialize_char(value)
	}

	fn serialize_str(self, value: &str) -> Result<S::Ok, S::Error> {
		self.0.serialize_str(value)
	}

	fn serialize_bytes(self, value: &[u8]) -> Result<S::Ok, S::Error> {
		self.0.serialize_bytes(value)
	}

	fn serialize_none(self) -> Result<S::Ok, S::Error> {
		self.0.serialize_none()
	}

	fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<S::Ok, S::Error> {
		self.0.serialize_some(&FiniteFloats(value))
	}

	fn serialize_unit(self) -> Result<S::Ok, S::Error> {
		self.0.serialize_unit()
	}

	fn serialize_unit_struct(self, name: &'static str) -> Result<S::Ok, S::Error> {
		self.0.serialize_unit_struct(name)
	}

	fn serialize_unit_variant(
		self,
		name: &'static str,
		variant_index: u32,
		variant: &'static str,
	) -> Result<S::Ok, S::Error> {
		self.0.serialize_unit_variant(name, variant_index, variant)
	}

	fn serialize_newtype_struct<T: Serialize + ?Sized>(
		self,
		name: &'static str,
		value: &T,
	) -> Result<S::Ok, S::Error> {
		self.0.serialize_newtype_struct(name, &FiniteFloats(value))
	}

	fn serialize_newtype_variant<T: Serialize + ?Sized>(
		self,
		name: &'static str,
		variant_index: u32,
		variant: &'static str,
		value: &T,
	) -> Result<S::Ok, S::Error> {
		self.0
			.serialize_newtype_variant(name, variant_index, variant, &FiniteFloats(value))
	}

	fn serialize_seq(self, length: Option<usize>) -> Result<Self::SerializeSeq, S::Error> {
		self.0.serialize_seq(length).map(FiniteFloatCompound)
	}

	fn serialize_tuple(self, length: usize) -> Result<Self::SerializeTuple, S::Error> {
		self.0.serialize_tuple(length).map(FiniteFloatCompound)
	}

	fn serialize_tuple_struct(
		self,
		name: &'static str,
		length: usize,
	) -> Result<Self::SerializeTupleStruct, S::Error> {
		self.0
			.serialize_tuple_struct(name, length)
			.map(FiniteFloatCompound)
	}

	fn serialize_tuple_variant(
		self,
		name: &'static str,
		variant_index: u32,
		variant: &'static str,
		length: usize,
	) -> Result<Self::SerializeTupleVariant, S::Error> {
		self.0
			.serialize_tuple_variant(name, variant_index, variant, length)
			.map(FiniteFloatCompound)
	}

	fn serialize_map(self, length: Option<usize>) -> Result<Self::SerializeMap, S::Error> {
		self.0.serialize_map(length).map(FiniteFloatCompound)
	}

	fn serialize_struct(
		self,
		name: &'static str,
		length: usize,
	) -> Result<Self::SerializeStruct, S::Error> {
		self.0
			.serialize_struct(name, length)
			.map(FiniteFloatCompound)
	}

	fn serialize_struct_variant(
		self,
		name: &'static str,
		variant_index: u32,
		variant: &'static str,
		length: usize,
	) -> Result<Self::SerializeStructVariant, S::Error> {
		self.0
			.serialize_struct_variant(name, variant_index, variant, length)
			.map(FiniteFloatCompound)
	}

	fn collect_str<T: Display + ?Sized>(self, value: &T) -> Result<S::Ok, S::Error> {
		self.0.collect_str(value)
	}

	fn is_human_readable(&self) -> bool {
		self.0.is_human_readable()
	}
}

impl<C: ser::SerializeSeq> ser::SerializeSeq for FiniteFloatCompound<C> {
	type Ok = C::Ok;
	type Error = C::Error;

	fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), C::Error> {
		self.0.serialize_element(&FiniteFloats(value))
	}

	fn end(self) -> Result<C::Ok, C::Error> {
		self.0.end()
	}
}

impl<C: ser::SerializeTuple> ser::SerializeTuple for FiniteFloatCompound<C> {
	type Ok = C::Ok;
	type Error = C::Error;

	fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), C::Error> {
		self.0.serialize_element(&FiniteFloats(value))
	}

	fn end(self) -> Result<C::Ok, C::Error> {
		self.0.end()
	}
}

impl<C: ser::SerializeTupleStruct> ser::SerializeTupleStruct for FiniteFloatCompound<C> {
	type Ok = C::Ok;
	type Error = C::Error;

	fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), C::Error> {
		self.0.serialize_field(&FiniteFloats(value))
	}

	fn end(self) -> Result<C::Ok, C::Error> {
		self.0.end()
	}
}

impl<C: ser::SerializeTupleVariant> ser::SerializeTupleVariant for FiniteFloatCompound<C> {
	type Ok = C::Ok;
	type Error = C::Error;

	fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), C::Error> {
		self.0.serialize_field(&FiniteFloats(value))
	}

	fn end(self) -> Result<C::Ok, C::Error> {
		self.0.end()
	}
}

impl<C: ser::SerializeMap> ser::SerializeMap for FiniteFloatCompound<C> {
	type Ok = C::Ok;
	type Error = C::Error;

	fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), C::Error> {
		self.0.serialize_key(&FiniteFloats(key))
	}

	fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), C::Error> {
		self.0.serialize_value(&FiniteFloats(value))
	}

	fn end(self) -> Result<C::Ok, C::Error> {
		self.0.end()
	}
}

impl<C: ser::SerializeStruct> ser::SerializeStruct for FiniteFloatCompound<C> {
	type Ok = C::Ok;
	type Error = C::Error;

	fn serialize_field<T: Serialize + ?Sized>(
		&mut self,
		key: &'static str,
		value: &T,
	) -> Result<(), C::Error> {
		self.0.serialize_field(key, &FiniteFloats(value))
	}

	fn skip_field(&mut self, key: &'static str) -> Result<(), C::Error> {
		self.0.skip_field(key)
	}

	fn end(self) -> Result<C::Ok, C::Error> {
		self.0.end()
	}
}

impl<C: ser::SerializeStructVariant> ser::SerializeStructVariant for FiniteFloatCompound<C> {
	type Ok = C::Ok;
	type Error = C::Error;

	fn serialize_field<T: Serialize + ?Sized>(
		&mut self,
		key: &'static str,
		value: &T,
	) -> Result<(), C::Error> {
		self.0.serialize_field(key, &FiniteFloats(value))
	}

	fn skip_field(&mut self, key: &'static str) -> Result<(), C::Error> {
		self.0.skip_field(key)
	}

	fn end(self) -> Result<C::Ok, C::Error> {
		self.0.end()
	}
}
