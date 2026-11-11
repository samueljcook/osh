use super::{JsonError, SerializeOptions, serialize_to_writer};
use serde::Serialize;
use std::io::{self, Write};
#[cfg(feature = "json-secret")]
use zeroize::Zeroize;

#[derive(Clone, Copy)]
pub(super) enum BufferZeroization {
	Disabled,
	#[cfg(feature = "json-secret")]
	Enabled,
}

pub(super) fn serialize<T: Serialize + ?Sized>(
	value: &T,
	options: SerializeOptions<'_>,
	zeroization: BufferZeroization,
) -> Result<Vec<u8>, JsonError> {
	let mut buffer = OutputBuffer {
		bytes: Vec::new(),
		max_size: options.limits.max_size,
		zeroization,
		allocation: AllocationState::Available,
	};
	let result = serialize_to_writer(&mut buffer, value, options);
	if matches!(buffer.allocation, AllocationState::Failed) {
		return Err(JsonError::AllocationFailed);
	}
	result?;
	Ok(std::mem::take(&mut buffer.bytes))
}

enum AllocationState {
	Available,
	Failed,
}

struct OutputBuffer {
	bytes: Vec<u8>,
	max_size: usize,
	zeroization: BufferZeroization,
	allocation: AllocationState,
}
impl OutputBuffer {
	fn reserve(&mut self, required: usize) -> Result<(), std::collections::TryReserveError> {
		if required <= self.bytes.capacity() {
			return Ok(());
		}
		let capacity = required
			.max(self.bytes.capacity().saturating_mul(2))
			.max(256)
			.min(self.max_size);
		#[cfg(feature = "json-secret")]
		if matches!(self.zeroization, BufferZeroization::Enabled) {
			let mut replacement = Vec::new();
			replacement.try_reserve_exact(capacity)?;
			replacement.extend_from_slice(&self.bytes);
			self.bytes.zeroize();
			self.bytes = replacement;
			return Ok(());
		}
		self.bytes.try_reserve_exact(capacity - self.bytes.len())
	}
}
impl Write for OutputBuffer {
	fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
		let required = self
			.bytes
			.len()
			.checked_add(bytes.len())
			.filter(|size| *size <= self.max_size)
			.ok_or_else(|| io::Error::other("JSON buffer exceeds its size limit"))?;
		if self.reserve(required).is_err() {
			self.allocation = AllocationState::Failed;
			return Err(io::Error::other("JSON buffer allocation failed"));
		}
		self.bytes.extend_from_slice(bytes);
		Ok(bytes.len())
	}
	fn flush(&mut self) -> io::Result<()> {
		Ok(())
	}
}
impl Drop for OutputBuffer {
	fn drop(&mut self) {
		match self.zeroization {
			BufferZeroization::Disabled => {}
			#[cfg(feature = "json-secret")]
			BufferZeroization::Enabled => self.bytes.zeroize(),
		}
	}
}
