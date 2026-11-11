//! Origin and referer normalization for same-origin checks.
//!
//! Exact origins contain only a scheme, host, and optional port. Referer values
//! may also contain a path, query, and fragment. Both forms reject credentials
//! and ambiguous input that the URL parser would otherwise repair.

use super::*;
use url::{Host, Url};

pub(super) fn normalize_allowed_origins(origins: Vec<String>) -> Result<Vec<String>, Error> {
	if origins.len() > MAX_CSRF_ALLOWED_ORIGINS {
		return Err(Error::AllowedOriginCountTooLarge {
			actual: origins.len(),
			max: MAX_CSRF_ALLOWED_ORIGINS,
		});
	}
	let mut normalized = Vec::new();
	normalized
		.try_reserve_exact(origins.len())
		.map_err(|_| Error::AllocationFailed)?;
	for origin in origins {
		normalized.push(normalize_exact_origin(&origin, "AllowedOrigin")?);
	}
	normalized.sort();
	normalized.dedup();
	Ok(normalized)
}

pub(super) fn normalize_request_origin_header(
	header: &str,
	label: &'static str,
) -> Result<String, Error> {
	if label == "Referer" {
		normalize_referer_origin(header, label)
	} else {
		normalize_exact_origin(header, label)
	}
}

/// Normalizes one origin string to its canonical scheme, host, and port form.
///
/// `label` names the header or configuration field that the origin came from, and it
/// appears in the returned error.
pub fn normalize_exact_origin(origin: &str, label: &'static str) -> Result<String, Error> {
	validate_origin_size(origin, label)?;
	let authority = origin
		.split_once("://")
		.ok_or(Error::OriginContainsNonOriginParts { label })?
		.1;
	let authority = authority.strip_suffix('/').unwrap_or(authority);
	if authority.is_empty() || authority.contains(['/', '?', '#', '@']) {
		return Err(Error::OriginContainsNonOriginParts { label });
	}
	let parsed = Url::parse(origin).map_err(|source| Error::OriginParse { label, source })?;
	reject_unsupported_origin_parts(&parsed, label)?;
	normalized_scheme_host_port(&parsed, label)
}

fn normalize_referer_origin(referer: &str, label: &'static str) -> Result<String, Error> {
	validate_origin_size(referer, label)?;
	let parsed = Url::parse(referer).map_err(|source| Error::OriginParse { label, source })?;
	reject_credentials(&parsed, label)?;
	normalized_scheme_host_port(&parsed, label)
}

fn validate_origin_size(value: &str, label: &'static str) -> Result<(), Error> {
	if value.len() > MAX_CSRF_ORIGIN_SIZE {
		return Err(Error::OriginTooLarge {
			label,
			actual: value.len(),
			max: MAX_CSRF_ORIGIN_SIZE,
		});
	}
	if value
		.bytes()
		.any(|byte| byte <= 0x20 || byte == 0x7f || byte == b'\\')
	{
		return Err(Error::OriginContainsNonOriginParts { label });
	}
	let (_, authority_and_path) = value
		.split_once("://")
		.ok_or(Error::OriginContainsNonOriginParts { label })?;
	let authority = authority_and_path
		.split(['/', '?', '#'])
		.next()
		.unwrap_or_default();
	if authority.is_empty() || authority.contains('@') {
		return Err(Error::OriginContainsNonOriginParts { label });
	}
	Ok(())
}

fn normalized_scheme_host_port(parsed: &Url, label: &'static str) -> Result<String, Error> {
	let scheme = parsed.scheme().to_ascii_lowercase();
	if !matches!(scheme.as_str(), "http" | "https") {
		return Err(Error::OriginUnsupportedScheme { label, scheme });
	}
	let host = parsed.host().ok_or(Error::OriginMissingHost { label })?;
	let host = normalized_host(host);
	let mut normalized = String::new();
	normalized
		.try_reserve_exact(scheme.len() + 3 + host.len() + 6)
		.map_err(|_| Error::AllocationFailed)?;
	normalized.push_str(&scheme);
	normalized.push_str("://");
	normalized.push_str(&host);
	if let Some(port) = parsed.port() {
		normalized.push(':');
		normalized.push_str(&port.to_string());
	}
	Ok(normalized)
}

fn reject_unsupported_origin_parts(parsed: &Url, label: &'static str) -> Result<(), Error> {
	reject_credentials(parsed, label)?;
	if parsed.path() != "/"
		|| parsed.query().is_some()
		|| parsed.fragment().is_some()
		|| parsed.cannot_be_a_base()
	{
		return Err(Error::OriginContainsNonOriginParts { label });
	}
	Ok(())
}

fn reject_credentials(parsed: &Url, label: &'static str) -> Result<(), Error> {
	if !parsed.username().is_empty() || parsed.password().is_some() {
		return Err(Error::OriginContainsNonOriginParts { label });
	}
	Ok(())
}

fn normalized_host(host: Host<&str>) -> String {
	match host {
		Host::Domain(domain) => domain.to_ascii_lowercase(),
		Host::Ipv4(ip) => ip.to_string(),
		Host::Ipv6(ip) => format!("[{ip}]"),
	}
}
