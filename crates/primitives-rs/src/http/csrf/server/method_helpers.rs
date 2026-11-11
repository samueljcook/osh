//! HTTP method classification for CSRF verification.

/// Reports whether an HTTP method is safe for CSRF verification.
pub fn is_csrf_safe_method(method: &str) -> bool {
	matches!(method, "GET" | "HEAD" | "OPTIONS" | "TRACE")
}
