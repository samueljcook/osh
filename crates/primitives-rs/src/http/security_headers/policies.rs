use http::HeaderValue;

/// Treatment of existing values for headers that the security policy supplies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExistingSecurityHeaders {
	/// Retain all existing values, including route-specific policies.
	Preserve,
	/// Replace all existing values with the configured value.
	Replace,
}

/// An HTTP Strict Transport Security policy for an HTTPS deployment.
///
/// Attach this policy only where the client-facing connection uses HTTPS. The
/// middleware does not infer transport security from URI or forwarding headers.
/// This configuration does not request browser preload registration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HstsConfig {
	/// The lifetime in seconds. Zero asks browsers to remove this host's HSTS policy.
	pub max_age_secs: u64,
	/// Whether the policy also covers every subdomain.
	pub include_subdomains: bool,
}

impl From<HstsConfig> for HeaderValue {
	fn from(config: HstsConfig) -> Self {
		let suffix = if config.include_subdomains {
			"; includeSubDomains"
		} else {
			""
		};
		Self::from_str(&format!("max-age={}{suffix}", config.max_age_secs))
			.expect("HSTS formatting produces a valid ASCII header value")
	}
}

/// Referrer information that browsers may send on subsequent requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReferrerPolicy {
	/// Send no referrer information. Under this policy a browser also sends
	/// `Origin: null` for a form submission, so a server that compares the
	/// `Origin` header with a list of origins rejects the page's own form
	/// submissions.
	NoReferrer,
	/// Send the referrer URL except when navigating from HTTPS to HTTP.
	NoReferrerWhenDowngrade,
	/// Send the referrer URL only on same-origin requests.
	SameOrigin,
	/// Send only the referrer's origin.
	Origin,
	/// Send only the origin, except when navigating from HTTPS to HTTP.
	StrictOrigin,
	/// Send the URL on same-origin requests and the origin on cross-origin requests.
	OriginWhenCrossOrigin,
	/// Send the URL on same-origin requests and the origin on cross-origin requests,
	/// except that HTTPS-to-HTTP requests receive no referrer information.
	StrictOriginWhenCrossOrigin,
	/// Send the referrer URL, including on HTTPS-to-HTTP requests.
	UnsafeUrl,
}

impl From<ReferrerPolicy> for HeaderValue {
	fn from(policy: ReferrerPolicy) -> Self {
		Self::from_static(match policy {
			ReferrerPolicy::NoReferrer => "no-referrer",
			ReferrerPolicy::NoReferrerWhenDowngrade => "no-referrer-when-downgrade",
			ReferrerPolicy::SameOrigin => "same-origin",
			ReferrerPolicy::Origin => "origin",
			ReferrerPolicy::StrictOrigin => "strict-origin",
			ReferrerPolicy::OriginWhenCrossOrigin => "origin-when-cross-origin",
			ReferrerPolicy::StrictOriginWhenCrossOrigin => "strict-origin-when-cross-origin",
			ReferrerPolicy::UnsafeUrl => "unsafe-url",
		})
	}
}

/// Legacy framing protection through `X-Frame-Options`.
///
/// Use CSP `frame-ancestors` for origin allowlists. A browser that enforces
/// `frame-ancestors` uses that directive instead of `X-Frame-Options`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameOptions {
	/// Prohibit framing.
	Deny,
	/// Permit framing only by same-origin ancestors.
	SameOrigin,
}

impl From<FrameOptions> for HeaderValue {
	fn from(policy: FrameOptions) -> Self {
		Self::from_static(match policy {
			FrameOptions::Deny => "DENY",
			FrameOptions::SameOrigin => "SAMEORIGIN",
		})
	}
}

/// A Cross-Origin-Opener-Policy value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoopPolicy {
	/// Use ordinary browser opener behavior without COOP isolation.
	UnsafeNone,
	/// Separate documents with a different origin or opener policy.
	SameOrigin,
	/// Retain opener relationships with popups that use `unsafe-none`.
	SameOriginAllowPopups,
	/// Separate this document from its opener while allowing it to open popups.
	NoopenerAllowPopups,
}

impl From<CoopPolicy> for HeaderValue {
	fn from(policy: CoopPolicy) -> Self {
		Self::from_static(match policy {
			CoopPolicy::UnsafeNone => "unsafe-none",
			CoopPolicy::SameOrigin => "same-origin",
			CoopPolicy::SameOriginAllowPopups => "same-origin-allow-popups",
			CoopPolicy::NoopenerAllowPopups => "noopener-allow-popups",
		})
	}
}

/// A Cross-Origin-Embedder-Policy value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoepPolicy {
	/// Permit ordinary cross-origin loading without COEP restrictions.
	UnsafeNone,
	/// Require cross-origin resources to opt in through CORS or CORP.
	RequireCorp,
	/// Omit credentials from cross-origin no-CORS requests so that those resources
	/// can load without a CORP opt-in.
	Credentialless,
}

impl From<CoepPolicy> for HeaderValue {
	fn from(policy: CoepPolicy) -> Self {
		Self::from_static(match policy {
			CoepPolicy::UnsafeNone => "unsafe-none",
			CoepPolicy::RequireCorp => "require-corp",
			CoepPolicy::Credentialless => "credentialless",
		})
	}
}

/// A Cross-Origin-Resource-Policy value for no-CORS resource requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CorpPolicy {
	/// Permit same-origin resource requests.
	SameOrigin,
	/// Permit same-site resource requests, subject to the browser's scheme checks.
	SameSite,
	/// Permit cross-origin resource requests.
	CrossOrigin,
}

impl From<CorpPolicy> for HeaderValue {
	fn from(policy: CorpPolicy) -> Self {
		Self::from_static(match policy {
			CorpPolicy::SameOrigin => "same-origin",
			CorpPolicy::SameSite => "same-site",
			CorpPolicy::CrossOrigin => "cross-origin",
		})
	}
}
