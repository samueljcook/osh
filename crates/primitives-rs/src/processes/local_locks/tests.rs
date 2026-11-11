use super::*;

struct TestDir(std::path::PathBuf);

impl TestDir {
	fn new() -> Self {
		let mut random = [0; 16];
		crate::random::secure_bytes::fill_random_bytes(&mut random).unwrap();
		let path = std::env::temp_dir().join(format!(
			"primitives-lock-test-{:032x}",
			u128::from_le_bytes(random)
		));
		std::fs::create_dir(&path).unwrap();
		Self(path)
	}
}

impl Drop for TestDir {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

#[test]
fn identity_validation_refuses_file_and_parent_replacement_after_open() {
	let root = TestDir::new();
	std::fs::create_dir(root.0.join("nested")).unwrap();
	let dir = Dir::open(&root.0).unwrap();
	let path = RelativePath::new("nested/update.lock").unwrap();
	let (parent, name) = dir.open_parent_dir(&path).unwrap();
	let options = LockOptions {
		permissions: Permissions::PlatformDefault,
	};
	let (file, _) = open_lock_file(&parent, name, options).unwrap();
	validate_lock_path(&dir, &path, &file).unwrap();
	file.try_lock().unwrap();
	std::fs::rename(root.0.join("nested"), root.0.join("moved")).unwrap();
	std::fs::create_dir(root.0.join("nested")).unwrap();
	std::fs::write(root.0.join("nested/update.lock"), b"unrelated").unwrap();
	assert!(matches!(
		validate_lock_path(&dir, &path, &file),
		Err(LockError::PathChanged)
	));
	assert_eq!(
		std::fs::read(root.0.join("nested/update.lock")).unwrap(),
		b"unrelated"
	);
	let original_path = RelativePath::new("moved/update.lock").unwrap();
	validate_lock_path(&dir, &original_path, &file).unwrap();
	std::fs::rename(
		root.0.join("moved/update.lock"),
		root.0.join("moved/original.lock"),
	)
	.unwrap();
	std::fs::write(root.0.join("moved/update.lock"), b"").unwrap();
	assert!(matches!(
		validate_lock_path(&dir, &original_path, &file),
		Err(LockError::PathChanged)
	));
}

#[test]
fn identity_validation_refuses_links_added_after_open() {
	let root = TestDir::new();
	let dir = Dir::open(&root.0).unwrap();
	let path = RelativePath::new("update.lock").unwrap();
	let (parent, name) = dir.open_parent_dir(&path).unwrap();
	let (file, _) = open_lock_file(
		&parent,
		name,
		LockOptions {
			permissions: Permissions::PlatformDefault,
		},
	)
	.unwrap();
	file.try_lock().unwrap();
	std::fs::hard_link(root.0.join("update.lock"), root.0.join("alias")).unwrap();
	assert!(matches!(
		validate_lock_path(&dir, &path, &file),
		Err(LockError::InvalidFile)
	));
	std::fs::remove_file(root.0.join("alias")).unwrap();
	validate_lock_path(&dir, &path, &file).unwrap();
	std::fs::remove_file(root.0.join("update.lock")).unwrap();
	assert!(validate_lock_path(&dir, &path, &file).is_err());
}
