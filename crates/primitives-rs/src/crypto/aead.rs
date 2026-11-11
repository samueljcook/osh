macro_rules! aead_algorithm {
	($cipher:ty, $nonce_len:literal) => {
		/// The nonce size in bytes.
		pub const NONCE_LEN: usize = $nonce_len;
		/// The appended authentication tag size in bytes.
		pub const TAG_LEN: usize = 16;
		/// Caller-supplied inputs for authenticated encryption.
		pub struct EncryptInput<'a> {
			/// A fresh nonce according to this algorithm's nonce requirements.
			pub nonce: &'a [u8; NONCE_LEN],
			/// Bytes that are authenticated but are not stored in the ciphertext.
			pub associated_data: &'a [u8],
			/// Secret bytes to encrypt.
			pub plaintext: &'a [u8],
		}
		/// Caller-supplied inputs for authenticated decryption.
		pub struct DecryptInput<'a> {
			/// The encryption nonce.
			pub nonce: &'a [u8; NONCE_LEN],
			/// The exact associated data supplied during encryption.
			pub associated_data: &'a [u8],
			/// Ciphertext followed by the complete 16-byte authentication tag.
			pub ciphertext: &'a [u8],
		}
		/// Encrypts with an explicit key and nonce and appends the authentication tag.
		pub fn encrypt(
			key: &crate::crypto::Key32,
			input: EncryptInput<'_>,
		) -> Result<Vec<u8>, crate::crypto::AeadError> {
			use ::chacha20poly1305::aead::{AeadInOut, KeyInit};
			let capacity = input
				.plaintext
				.len()
				.checked_add(TAG_LEN)
				.ok_or(crate::crypto::AeadError::MsgTooLarge)?;
			let mut output = zeroize::Zeroizing::new(
				crate::secret_bytes::allocate_byte_buffer(capacity)
					.map_err(|_| crate::crypto::AeadError::AllocationFailed)?,
			);
			output.extend_from_slice(input.plaintext);
			let cipher = <$cipher>::new(key.expose_secret().into());
			let tag = cipher
				.encrypt_inout_detached(
					input.nonce.into(),
					input.associated_data,
					output.as_mut_slice().into(),
				)
				.map_err(|_| crate::crypto::AeadError::MsgTooLarge)?;
			output.extend_from_slice(&tag);
			Ok(std::mem::take(&mut *output))
		}
		/// Verifies the complete tag and returns a zeroizing plaintext buffer.
		pub fn decrypt(
			key: &crate::crypto::Key32,
			input: DecryptInput<'_>,
		) -> Result<crate::secret_bytes::SecretBytes, crate::crypto::AeadError> {
			use ::chacha20poly1305::aead::{AeadInOut, KeyInit};
			let message_len = input
				.ciphertext
				.len()
				.checked_sub(TAG_LEN)
				.ok_or(crate::crypto::AeadError::AuthenticationFailed)?;
			let (ciphertext, tag) = input.ciphertext.split_at(message_len);
			let tag: &[u8; TAG_LEN] = tag
				.try_into()
				.expect("the authentication tag has the fixed trailing length");
			let mut output = crate::secret_bytes::SecretBytes::from_slice(ciphertext)
				.map_err(|_| crate::crypto::AeadError::AllocationFailed)?;
			let cipher = <$cipher>::new(key.expose_secret().into());
			cipher
				.decrypt_inout_detached(
					input.nonce.into(),
					input.associated_data,
					output.expose_secret_mut().into(),
					tag.into(),
				)
				.map_err(|_| crate::crypto::AeadError::AuthenticationFailed)?;
			Ok(output)
		}
	};
}
