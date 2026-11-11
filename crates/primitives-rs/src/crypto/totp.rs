//! RFC 6238 TOTP with explicit algorithms, time, and verification windows.
//!
//! The implementation uses `totp-rs` for RFC token generation. This module owns
//! canonical code parsing, bounded interval enumeration, constant-time window
//! comparisons, and the matched time step that a consuming protocol can
//! atomically consume. Enrollment, rate limits, and replay storage belong to
//! that protocol. No operation reads a clock or stores consumption state.
//!
//! This interoperable protocol uses the selected RFC algorithm. It does not add
//! the Primitives hedged MAC format, which ordinary authenticator apps cannot use.

use std::fmt::{self, Write as _};
use std::num::NonZeroU64;

use crate::encoding::binary_to_text::ByteEncoding;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use subtle::{Choice, ConditionallySelectable, ConstantTimeEq};
use zeroize::{Zeroize, Zeroizing};

/// An HMAC algorithm supported by RFC 6238.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "tsgen", derive(crate::tsgen::TsGen))]
#[serde(rename_all = "snake_case")]
pub enum Algorithm {
	/// HMAC-SHA1, for compatible authenticator apps.
	Sha1,
	/// HMAC-SHA256.
	Sha256,
	/// HMAC-SHA512.
	Sha512,
}

/// The decimal code length.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "tsgen", derive(crate::tsgen::TsGen))]
#[serde(rename_all = "snake_case")]
pub enum Digits {
	/// Six decimal digits.
	Six,
	/// Seven decimal digits.
	Seven,
	/// Eight decimal digits.
	Eight,
}

impl Digits {
	/// Returns the number of decimal digits.
	pub const fn count(self) -> usize {
		match self {
			Self::Six => 6,
			Self::Seven => 7,
			Self::Eight => 8,
		}
	}
}

/// The explicit algorithm, decimal length, and time-step duration.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
	/// The HMAC algorithm.
	pub algorithm: Algorithm,
	/// The number of decimal digits.
	pub digits: Digits,
	/// The duration of one time step, in seconds.
	pub step_secs: NonZeroU64,
}

/// The adjacent time steps to consider during verification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VerificationWindow {
	/// The number of preceding time steps that may match.
	pub past_steps: u8,
	/// The number of following time steps that may match.
	pub future_steps: u8,
}

/// Canonical decimal digits with redacted debug output and zeroizing storage.
pub struct Code {
	bytes: [u8; 8],
	digits: Digits,
}

impl Code {
	/// Parses exactly the configured number of ASCII digits, including leading zeros.
	pub fn parse(text: &str, digits: Digits) -> Result<Self, Error> {
		if text.len() != digits.count() || !text.as_bytes().iter().all(u8::is_ascii_digit) {
			return Err(Error::InvalidCode);
		}
		let mut bytes = [0; 8];
		bytes[..text.len()].copy_from_slice(text.as_bytes());
		Ok(Self { bytes, digits })
	}

	/// Exposes the canonical code for display or protocol transmission.
	pub fn expose_secret(&self) -> &str {
		std::str::from_utf8(&self.bytes[..self.digits.count()])
			.expect("a parsed TOTP code contains ASCII digits")
	}
}

impl Drop for Code {
	fn drop(&mut self) {
		self.bytes.zeroize();
	}
}

impl fmt::Debug for Code {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("Code([redacted])")
	}
}

/// One generated code and its exact RFC time-step counter.
#[derive(Debug)]
pub struct StepCode {
	/// The counter, equal to Unix seconds divided by the configured step duration.
	pub step: u64,
	/// The code for that counter.
	pub code: Code,
}

/// A secret-backed TOTP generator with an explicit, fixed configuration.
pub struct Totp {
	inner: totp_rs::Totp,
	config: Config,
}

impl Totp {
	/// Copies a secret of at least 20 bytes into zeroizing storage.
	pub fn new(config: Config, secret: &[u8]) -> Result<Self, Error> {
		if secret.len() < 20 {
			return Err(Error::InvalidSecret);
		}
		let algorithm = match config.algorithm {
			Algorithm::Sha1 => totp_rs::Algorithm::SHA1,
			Algorithm::Sha256 => totp_rs::Algorithm::SHA256,
			Algorithm::Sha512 => totp_rs::Algorithm::SHA512,
		};
		let inner = totp_rs::Builder::new()
			.with_algorithm(algorithm)
			.with_digits(config.digits.count() as u8)
			.with_step_duration(config.step_secs.get())
			.with_skew(0)
			.with_secret(totp_rs::Secret::new(secret.into()))
			.build()
			.map_err(|_| Error::InvalidSecret)?;
		Ok(Self { inner, config })
	}

	/// Returns the configuration without exposing the secret.
	pub fn config(&self) -> Config {
		self.config
	}

	/// Exposes an `otpauth://` enrollment URI in zeroizing string storage.
	///
	/// Returns [`Error::InvalidProvisioningLabels`] when `labels.issuer` or `labels.account` is
	/// empty or contains `':'`.
	pub fn provisioning_uri(
		&self,
		labels: ProvisioningLabels<'_>,
	) -> Result<Zeroizing<String>, Error> {
		if labels.issuer.is_empty()
			|| labels.account.is_empty()
			|| labels.issuer.contains(':')
			|| labels.account.contains(':')
		{
			return Err(Error::InvalidProvisioningLabels);
		}
		const RESERVED: &percent_encoding::AsciiSet = &NON_ALPHANUMERIC
			.remove(b'-')
			.remove(b'_')
			.remove(b'.')
			.remove(b'~');
		let issuer = utf8_percent_encode(labels.issuer, RESERVED).to_string();
		let account = utf8_percent_encode(labels.account, RESERVED).to_string();
		let digits = match self.config.digits {
			Digits::Six => "",
			Digits::Seven => "&digits=7",
			Digits::Eight => "&digits=8",
		};
		let algorithm = match self.config.algorithm {
			Algorithm::Sha1 => "",
			Algorithm::Sha256 => "&algorithm=SHA256",
			Algorithm::Sha512 => "&algorithm=SHA512",
		};
		let period = if self.config.step_secs.get() == 30 {
			String::new()
		} else {
			format!("&period={}", self.config.step_secs)
		};
		let secret = self.inner.secret().as_bytes();
		let encoded_size = secret.len() / 5 * 8 + (secret.len() % 5 * 8).div_ceil(5);
		let capacity = "otpauth://totp/:?secret=&issuer=".len()
			+ issuer.len() * 2
			+ account.len()
			+ encoded_size
			+ digits.len()
			+ algorithm.len()
			+ period.len();
		let mut uri = Zeroizing::new(String::with_capacity(capacity));
		uri.push_str("otpauth://totp/");
		uri.push_str(&issuer);
		uri.push(':');
		uri.push_str(&account);
		uri.push_str("?secret=");
		// Five input bytes produce eight Base32 characters. Aligned chunks keep
		// encoder allocations bounded without adding padding between chunks.
		for chunk in secret.chunks(5 * 1024) {
			let encoded = ByteEncoding::Base32Unpadded.encode_zeroizing(chunk);
			uri.push_str(&encoded);
		}
		uri.push_str(digits);
		uri.push_str(algorithm);
		uri.push_str("&issuer=");
		uri.push_str(&issuer);
		uri.push_str(&period);
		debug_assert_eq!(uri.len(), capacity);
		Ok(uri)
	}

	/// Generates the code at an explicit Unix time.
	pub fn code_at(&self, unix_secs: u64) -> Code {
		let token = Zeroizing::new(self.inner.generate(unix_secs));
		let mut text = Zeroizing::new(String::with_capacity(self.config.digits.count()));
		write!(&mut *text, "{}", *token)
			.expect("formatting a TOTP token into a String does not fail");
		Code::parse(&text, self.config.digits)
			.expect("the RFC generator returns the configured decimal code length")
	}

	/// Checks every step in the window and returns the newest matching step.
	///
	/// The comparison does not stop after a match. A protocol must consume the
	/// returned step atomically and reject an already-consumed step. At the Unix
	/// epoch and maximum timestamp, the window excludes unrepresentable steps.
	pub fn verify_code(
		&self,
		code: &Code,
		unix_secs: u64,
		window: VerificationWindow,
	) -> Option<u64> {
		if code.digits != self.config.digits {
			return None;
		}
		let current = unix_secs / self.config.step_secs.get();
		let start = current.saturating_sub(u64::from(window.past_steps));
		let end = current
			.saturating_add(u64::from(window.future_steps))
			.min(u64::MAX / self.config.step_secs.get());
		let mut has_match = Choice::from(0);
		let mut matched_step = 0_u64;
		for step in start..=end {
			let candidate = self.code_at(step * self.config.step_secs.get());
			let is_match = candidate.bytes.ct_eq(&code.bytes);
			matched_step = u64::conditional_select(&matched_step, &step, is_match);
			has_match |= is_match;
		}
		if bool::from(has_match) {
			Some(matched_step)
		} else {
			None
		}
	}

	/// Enumerates all time steps intersecting an inclusive Unix-time interval.
	///
	/// The operation checks `step_limit` before allocating or generating codes.
	/// A verifier can seal keyed verifiers of these codes for preliminary rejection
	/// while retaining authoritative time and replay checks at acceptance.
	pub fn codes_in_interval(&self, interval: CodeInterval) -> Result<Vec<StepCode>, Error> {
		if interval.start_unix_secs > interval.end_unix_secs {
			return Err(Error::InvalidInterval);
		}
		let start = interval.start_unix_secs / self.config.step_secs.get();
		let end = interval.end_unix_secs / self.config.step_secs.get();
		let count = end
			.checked_sub(start)
			.and_then(|difference| difference.checked_add(1))
			.filter(|count| *count <= u64::from(interval.step_limit))
			.ok_or(Error::IntervalLimitExceeded)?;
		Ok((start..=end)
			.take(count as usize)
			.map(|step| StepCode {
				step,
				code: self.code_at(step * self.config.step_secs.get()),
			})
			.collect())
	}
}

impl fmt::Debug for Totp {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("Totp")
			.field("config", &self.config)
			.finish_non_exhaustive()
	}
}

/// An inclusive time interval and an explicit generation-work limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CodeInterval {
	/// The first Unix second to cover.
	pub start_unix_secs: u64,
	/// The last Unix second to cover.
	pub end_unix_secs: u64,
	/// The maximum number of time steps to generate.
	pub step_limit: u16,
}

/// Labels displayed by an authenticator app during enrollment.
#[derive(Clone, Copy, Debug)]
pub struct ProvisioningLabels<'a> {
	/// The nonempty application or organization name, without a colon.
	pub issuer: &'a str,
	/// The nonempty account label, without a colon.
	pub account: &'a str,
}

/// A malformed code, secret, or interval.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
	/// The code has the wrong length or contains a non-ASCII digit.
	InvalidCode,
	/// The secret does not meet the RFC generator's requirements.
	InvalidSecret,
	/// The interval ends before it starts.
	InvalidInterval,
	/// The interval contains more steps than the caller permits.
	IntervalLimitExceeded,
	/// An enrollment label cannot be encoded in the provisioning URI.
	InvalidProvisioningLabels,
}

impl fmt::Display for Error {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(match self {
			Self::InvalidCode => "invalid TOTP code",
			Self::InvalidSecret => "invalid TOTP secret",
			Self::InvalidInterval => "invalid TOTP time interval",
			Self::IntervalLimitExceeded => "TOTP interval exceeds the step limit",
			Self::InvalidProvisioningLabels => "invalid TOTP provisioning labels",
		})
	}
}

impl std::error::Error for Error {}
