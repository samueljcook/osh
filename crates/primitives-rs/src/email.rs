//! Parsed email addresses and delivery mailboxes, and hashed email identities with explicit
//! application selection.
//!
//! [`normalize_email`] trims surrounding whitespace, parses one mailbox (with an optional
//! display name), lowercases its ASCII local part, and converts its domain to lowercase IDNA
//! ASCII. The returned mailbox preserves the trimmed input for delivery and display.
//! Only the BLAKE3 digest of the normalized address is exposed as its identity.
//! Display names do not contribute to the normalized identity. Local parts must contain
//! 1 to 64 ASCII letters, digits, `.`, `_`, `+`, or `-`, with no leading, trailing, or
//! consecutive dots. Quoted local parts, RFC comments, recipient groups, internationalized
//! local parts, single-label domains, and domain literals are rejected. DNS labels and
//! complete addresses must fit DNS and SMTP length limits. Parsing does not establish mailbox
//! existence or ownership.
//!
//! For `gmail.com` and `googlemail.com`, normalization removes local-part dots and the
//! first `+` suffix and uses `gmail.com`. Other domains retain dots and `+` suffixes,
//! including custom domains hosted by Google. This policy treats local-part case as
//! insignificant; applications that require different identity rules can supply another
//! [`EmailNormalizer`].
//!
//! [`EmailAddress`] parses an address alone, without a display name, a comment, or other text
//! that a mailbox can carry.

/// An input that an email parser or the selected normalization policy rejects.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidEmail;

impl std::fmt::Display for InvalidEmail {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter.write_str("invalid email address")
	}
}

impl std::error::Error for InvalidEmail {}

/// A BLAKE3 digest of an application's normalized email identity.
///
/// Anyone can recover a guessable address from its unkeyed digest by hashing
/// candidate addresses, so `Debug` output omits the digest bytes.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct EmailIdentity([u8; 32]);

impl EmailIdentity {
	/// Hashes a policy's temporary canonical identity as UTF-8 bytes.
	pub fn from_normalized(normalized: &str) -> Self {
		Self(*blake3::hash(normalized.as_bytes()).as_bytes())
	}

	/// Returns the 32-byte identity digest.
	pub fn as_bytes(&self) -> &[u8; 32] {
		&self.0
	}
}

impl std::fmt::Debug for EmailIdentity {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter.write_str("EmailIdentity([redacted])")
	}
}

/// A parsed delivery mailbox and its independently normalized identity digest.
#[derive(Clone, Eq, PartialEq)]
pub struct NormalizedEmail {
	mailbox: String,
	identity: EmailIdentity,
}

impl NormalizedEmail {
	/// Parses one mailbox and associates an application-selected identity digest.
	/// Surrounding whitespace is removed. Controls other than horizontal tabs are rejected.
	pub fn parse(input: &str, identity: EmailIdentity) -> Result<Self, InvalidEmail> {
		let input = parse_mailbox_input(input)?;
		Ok(Self {
			mailbox: input.to_owned(),
			identity,
		})
	}

	/// Returns the parsed, trimmed input mailbox for delivery and display.
	pub fn mailbox(&self) -> &str {
		&self.mailbox
	}

	/// Returns the normalized identity digest for comparisons and indexing.
	pub fn identity(&self) -> EmailIdentity {
		self.identity
	}
}

impl std::fmt::Debug for NormalizedEmail {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter.write_str("NormalizedEmail([redacted])")
	}
}

/// An email address alone: a local part, one `@`, and a domain.
///
/// The accepted grammar is an RFC 5322 `addr-spec` with a local part and a domain that are both
/// `dot-atom-text`, with the RFC 6532 extension to UTF-8:
///
/// ```text
/// address       = dot-atom-text "@" dot-atom-text
/// dot-atom-text = 1*atext *("." 1*atext)
/// atext         = ALPHA / DIGIT / "!" / "#" / "$" / "%" / "&" / "'" / "*" / "+" / "-" /
///                 "/" / "=" / "?" / "^" / "_" / "`" / "{" / "|" / "}" / "~" /
///                 any non-ASCII character that is not whitespace, not a control character,
///                 and not an invisible formatting character of the address part
/// ```
///
/// Neither part can contain a bidirectional control character (U+061C, U+200E, U+200F, U+202A
/// to U+202E, and U+2066 to U+2069), a zero-width or invisible formatting character (U+00AD,
/// U+180E, U+200B, U+2060 to U+2064, and U+FEFF), or a tag character (U+E0001 and U+E0020 to
/// U+E007F) because these characters are invisible or reorder the displayed address.
/// The local part also cannot contain U+200C ZERO WIDTH NON-JOINER or U+200D ZERO WIDTH JOINER.
/// The domain can contain those two characters because internationalized domain names of some
/// scripts need them.
///
/// The input must match this grammar exactly, without surrounding whitespace. The grammar
/// therefore excludes display names, angle brackets, comments, quoted local parts, domain
/// literals, groups, and lists of addresses. It does not limit lengths, require a domain with
/// more than one label, or convert an internationalized domain to its ASCII form; a normalizer
/// such as [`normalize_email`] applies those rules. The parsed address keeps the input's
/// spelling.
#[derive(Clone, Eq, PartialEq)]
pub struct EmailAddress(String);

impl EmailAddress {
	/// Parses an input that consists of exactly one address in the accepted grammar.
	///
	/// ```
	/// use primitives_rs::email::{EmailAddress, InvalidEmail};
	///
	/// let address = EmailAddress::parse("First.Last+tag@Example.com")?;
	/// assert_eq!(address.as_str(), "First.Last+tag@Example.com");
	/// assert_eq!(EmailAddress::parse("Name <a@example.com>"), Err(InvalidEmail));
	/// # Ok::<(), InvalidEmail>(())
	/// ```
	pub fn parse(input: &str) -> Result<Self, InvalidEmail> {
		let is_dot_atom_text = |text: &str, part: AddressPart| {
			text.split('.').all(|atom| {
				!atom.is_empty()
					&& atom.chars().all(|ch| {
						ch.is_ascii_alphanumeric()
							|| "!#$%&'*+-/=?^_`{|}~".contains(ch)
							|| (!ch.is_ascii()
								&& !ch.is_whitespace() && !ch.is_control()
								&& !is_invisible_format_character(ch, part))
					})
			})
		};
		match input.split_once('@') {
			Some((local, domain))
				if is_dot_atom_text(local, AddressPart::Local)
					&& is_dot_atom_text(domain, AddressPart::Domain) =>
			{
				Ok(Self(input.to_owned()))
			}
			_ => Err(InvalidEmail),
		}
	}

	/// Returns the address in the input's spelling.
	pub fn as_str(&self) -> &str {
		&self.0
	}
}

/// The part of an address that contains a character.
#[derive(Clone, Copy)]
enum AddressPart {
	Local,
	Domain,
}

/// Reports whether `ch` is an invisible formatting character that `part` of an
/// [`EmailAddress`] cannot contain: in either part, a bidirectional control character, a
/// zero-width or invisible formatting character, or a tag character, and in the local part
/// also U+200C ZERO WIDTH NON-JOINER and U+200D ZERO WIDTH JOINER.
fn is_invisible_format_character(ch: char, part: AddressPart) -> bool {
	let in_every_part = matches!(
		ch,
		// Bidirectional control characters.
		'\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
		// Zero-width and invisible formatting characters.
		| '\u{00AD}' | '\u{180E}' | '\u{200B}' | '\u{2060}'..='\u{2064}' | '\u{FEFF}'
		// Tag characters.
		| '\u{E0001}' | '\u{E0020}'..='\u{E007F}'
	);
	in_every_part
		|| match part {
			AddressPart::Local => matches!(ch, '\u{200C}' | '\u{200D}'),
			AddressPart::Domain => false,
		}
}

impl std::fmt::Debug for EmailAddress {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter.write_str("EmailAddress([redacted])")
	}
}

/// Application-selected email identity rules.
///
/// Implementations must preserve the entered mailbox for delivery and display and
/// derive a deterministic identity digest from a temporary canonical value.
/// Equivalent inputs must produce the same identity. Implementations must perform
/// no I/O. Applications must keep the policy consistent across instances and stored
/// identities; changing identity mappings can require an application migration.
/// Functions and closures with this signature implement the trait directly.
pub trait EmailNormalizer: Send + Sync {
	/// Parses a mailbox and derives its identity according to this policy.
	fn normalize_email(&self, input: &str) -> Result<NormalizedEmail, InvalidEmail>;
}

impl<F> EmailNormalizer for F
where
	F: Fn(&str) -> Result<NormalizedEmail, InvalidEmail> + Send + Sync,
{
	fn normalize_email(&self, input: &str) -> Result<NormalizedEmail, InvalidEmail> {
		self(input)
	}
}

/// Parses a delivery mailbox and hashes its normalized identity using this module's policy.
/// The canonical address is temporary and is not returned.
///
/// ```
/// use primitives_rs::email::{EmailIdentity, normalize_email};
///
/// let email = normalize_email(" Person <First.Last+tag@GoogleMail.com> ")?;
/// assert_eq!(email.mailbox(), "Person <First.Last+tag@GoogleMail.com>");
/// assert_eq!(email.identity(), EmailIdentity::from_normalized("firstlast@gmail.com"));
/// # Ok::<(), primitives_rs::email::InvalidEmail>(())
/// ```
pub fn normalize_email(input: &str) -> Result<NormalizedEmail, InvalidEmail> {
	let input = parse_mailbox_input(input)?;
	let (original_local, original_domain) = mailbox_parts(input)?;
	if original_local.is_empty()
		|| original_local.len() > 64
		|| !original_local
			.bytes()
			.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-'))
		|| original_local.starts_with('.')
		|| original_local.ends_with('.')
		|| original_local.contains("..")
	{
		return Err(InvalidEmail);
	}
	let mut local = original_local.to_ascii_lowercase();
	let mut domain = idna::domain_to_ascii_strict(original_domain).map_err(|_| InvalidEmail)?;
	if !domain.contains('.') || domain.ends_with('.') || local.len() + 1 + domain.len() > 254 {
		return Err(InvalidEmail);
	}
	if domain == "gmail.com" || domain == "googlemail.com" {
		domain = "gmail.com".to_owned();
		if let Some(index) = local.find('+') {
			local.truncate(index);
		}
		local.retain(|ch| ch != '.');
	}
	if local.is_empty() {
		return Err(InvalidEmail);
	}
	Ok(NormalizedEmail {
		mailbox: input.to_owned(),
		identity: EmailIdentity::from_normalized(&format!("{local}@{domain}")),
	})
}

fn parse_mailbox_input(input: &str) -> Result<&str, InvalidEmail> {
	let input = input.trim();
	if input.chars().any(|ch| ch.is_control() && ch != '\t') {
		return Err(InvalidEmail);
	}
	input
		.parse::<lettre::message::Mailbox>()
		.map_err(|_| InvalidEmail)?;
	Ok(input)
}

enum QuotePosition {
	Unquoted,
	Quoted,
	Escaped,
}

enum AddressPosition {
	Bare,
	Opened { start: usize },
	Closed { start: usize, end: usize },
}

fn mailbox_parts(input: &str) -> Result<(&str, &str), InvalidEmail> {
	let mut address_position = AddressPosition::Bare;
	let mut quote_position = QuotePosition::Unquoted;
	for (index, ch) in input.char_indices() {
		match (&quote_position, ch) {
			(QuotePosition::Escaped, _) => quote_position = QuotePosition::Quoted,
			(QuotePosition::Quoted, '\\') => quote_position = QuotePosition::Escaped,
			(QuotePosition::Quoted, '"') => quote_position = QuotePosition::Unquoted,
			(QuotePosition::Quoted, _) => {}
			(QuotePosition::Unquoted, '"') => quote_position = QuotePosition::Quoted,
			(QuotePosition::Unquoted, '<') => {
				address_position = match address_position {
					AddressPosition::Bare => AddressPosition::Opened { start: index + 1 },
					AddressPosition::Opened { .. } | AddressPosition::Closed { .. } => {
						return Err(InvalidEmail);
					}
				};
			}
			(QuotePosition::Unquoted, '>') => {
				address_position = match address_position {
					AddressPosition::Opened { start } => {
						AddressPosition::Closed { start, end: index }
					}
					AddressPosition::Bare | AddressPosition::Closed { .. } => {
						return Err(InvalidEmail);
					}
				};
			}
			(QuotePosition::Unquoted, _) => {}
		}
	}
	if !matches!(quote_position, QuotePosition::Unquoted) {
		return Err(InvalidEmail);
	}
	let address = match address_position {
		AddressPosition::Closed { start, end } => {
			if end + 1 != input.len() {
				return Err(InvalidEmail);
			}
			input.get(start..end).ok_or(InvalidEmail)?
		}
		AddressPosition::Bare => input,
		AddressPosition::Opened { .. } => return Err(InvalidEmail),
	};
	if address.chars().any(char::is_whitespace) {
		return Err(InvalidEmail);
	}
	let (local, domain) = address.split_once('@').ok_or(InvalidEmail)?;
	if domain.is_empty() || domain.contains('@') {
		return Err(InvalidEmail);
	}
	Ok((local, domain))
}
