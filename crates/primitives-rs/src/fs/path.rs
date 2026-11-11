use std::path::{Component, Path, PathBuf};

/// A nonempty native relative path with no parent, root, or drive components.
///
/// Parsing normalizes repeated separators and current-dir components.
/// Non-Unicode file names remain valid. This type checks syntax; [`super::Dir`]
/// resolves each component without following symbolic links.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RelativePath(PathBuf);

impl RelativePath {
	/// Parses a native relative path.
	pub fn new(path: impl AsRef<Path>) -> Result<Self, InvalidRelativePath> {
		let path = path.as_ref();
		if path.as_os_str().as_encoded_bytes().contains(&0) {
			return Err(InvalidRelativePath);
		}
		let mut normalized = PathBuf::new();
		for component in path.components() {
			match component {
				Component::Normal(name) => normalized.push(name),
				Component::CurDir => {}
				_ => return Err(InvalidRelativePath),
			}
		}
		if normalized.as_os_str().is_empty() {
			return Err(InvalidRelativePath);
		}
		Ok(Self(normalized))
	}

	/// Returns the normalized native path.
	pub fn as_path(&self) -> &Path {
		&self.0
	}
}

impl AsRef<Path> for RelativePath {
	fn as_ref(&self) -> &Path {
		self.as_path()
	}
}

/// A path was empty or contained a root, drive, parent component, or null byte.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidRelativePath;

impl std::fmt::Display for InvalidRelativePath {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("expected a nonempty relative path without parent components or null bytes")
	}
}

impl std::error::Error for InvalidRelativePath {}
