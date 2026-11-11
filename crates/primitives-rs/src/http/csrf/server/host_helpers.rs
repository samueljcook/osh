use std::net::{IpAddr, Ipv6Addr};

use http::header::HOST;
use http::uri::Authority;
use http::{HeaderMap, Request};

use super::error::Error;

const MAX_DEV_REQUEST_AUTHORITY_SIZE: usize = 1024;

pub(super) fn validate_localhost_request_authorities<B>(request: &Request<B>) -> Result<(), Error> {
	let host_header = single_host_header(request.headers())?
		.map(|host| parse_loopback_authority(host, "Host header"))
		.transpose()?;
	let uri_authority = request
		.uri()
		.authority()
		.map(|authority| parse_loopback_authority(authority.as_str(), "request URI"))
		.transpose()?;

	match (host_header, uri_authority) {
		(None, None) => Err(Error::DevModeRequestAuthorityMissing),
		(Some(_), None) | (None, Some(_)) => Ok(()),
		(Some(host), Some(uri)) if host.matches(&uri) => Ok(()),
		(Some(host), Some(uri)) => Err(Error::DevModeRequestAuthorityConflict {
			host_header: host.rendered,
			uri_authority: uri.rendered,
		}),
	}
}

fn single_host_header(headers: &HeaderMap) -> Result<Option<&str>, Error> {
	let mut values = headers.get_all(HOST).iter();
	let Some(value) = values.next() else {
		return Ok(None);
	};
	if values.next().is_some() {
		return Err(Error::DevModeDuplicateHostHeader);
	}
	value
		.to_str()
		.map(Some)
		.map_err(Error::DevModeHostHeaderDecode)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LoopbackHost {
	Localhost,
	Ip(IpAddr),
}

#[derive(Debug, Eq, PartialEq)]
struct LoopbackAuthority {
	host: LoopbackHost,
	port: Option<u16>,
	rendered: String,
}

impl LoopbackAuthority {
	fn matches(&self, other: &Self) -> bool {
		self.host == other.host && self.port == other.port
	}
}

fn parse_loopback_authority(
	rendered: &str,
	label: &'static str,
) -> Result<LoopbackAuthority, Error> {
	if rendered.len() > MAX_DEV_REQUEST_AUTHORITY_SIZE || rendered.contains('@') {
		return Err(Error::DevModeInvalidRequestAuthority { label });
	}
	let authority = rendered
		.parse::<Authority>()
		.map_err(|_| Error::DevModeInvalidRequestAuthority { label })?;
	let port_suffix = &authority.as_str()[authority.host().len()..];
	if (!port_suffix.is_empty() && !port_suffix.starts_with(':'))
		|| port_suffix == ":"
		|| (!port_suffix.is_empty() && authority.port_u16().is_none())
	{
		return Err(Error::DevModeInvalidRequestAuthority { label });
	}
	let host =
		parse_loopback_host(authority.host()).ok_or_else(|| Error::DevModeNonLocalhostHost {
			host: rendered.to_owned(),
		})?;
	Ok(LoopbackAuthority {
		host,
		port: authority.port_u16(),
		rendered: rendered.to_owned(),
	})
}

fn parse_loopback_host(host: &str) -> Option<LoopbackHost> {
	if host.eq_ignore_ascii_case("localhost") {
		return Some(LoopbackHost::Localhost);
	}
	let host = host
		.strip_prefix('[')
		.and_then(|host| host.strip_suffix(']'))
		.unwrap_or(host);
	let ip: IpAddr = host.parse().ok()?;
	if !ip_is_loopback(ip) {
		return None;
	}
	Some(LoopbackHost::Ip(normalize_mapped_ip(ip)))
}

fn normalize_mapped_ip(ip: IpAddr) -> IpAddr {
	match ip {
		IpAddr::V6(ip) => ipv6_mapped_ipv4(ip)
			.map(IpAddr::V4)
			.unwrap_or(IpAddr::V6(ip)),
		ip => ip,
	}
}

fn ip_is_loopback(ip: IpAddr) -> bool {
	match ip {
		IpAddr::V4(ip) => ip.is_loopback(),
		IpAddr::V6(ip) => {
			ip.is_loopback() || ipv6_mapped_ipv4(ip).is_some_and(|ip| ip.is_loopback())
		}
	}
}

fn ipv6_mapped_ipv4(ip: Ipv6Addr) -> Option<std::net::Ipv4Addr> {
	ip.to_ipv4_mapped()
}
