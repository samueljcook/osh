//! Focused software primitives enabled by independent Cargo features.
//!
//! Modules identify responsibilities. Equivalent TypeScript facilities in
//! `primitives-ts` use the same domain names, with slashes and kebab-case.
//!
//! | Domain | Facilities | Cargo features |
//! | --- | --- | --- |
//! | `encoding` | Canonical decimal integer text, binary-to-text encodings, and mnemonic byte encodings | unconditional (`decimal`), `byte-encoding`, `mnemonic` |
//! | `text::unicode` | Versioned normalization, full case folding, caseless keys, and character properties | `unicode` |
//! | `email` | Parsed mailboxes, hashed email identities, and application-selected normalization policies | `email` |
//! | `bytes` | Length-prefixed byte framing, lexicographic prefix bounds, and deterministic content-defined chunks | unconditional (framing), `byte-prefix-range`, `fastcdc` |
//! | `time` | Ceiling duration conversions, clamped float-second conversions, and monotonic deadlines | unconditional |
//! | `future` | Panic-catching polling wrapper for `Unpin` futures | unconditional |
//! | `db::sqlite` | Owned synchronous SQLite transactions | `sqlite-transactions` |
//! | `db::sqlite::backup` | Consistent database snapshots and offline recovery with atomic file publication | `sqlite-backup` |
//! | `db::sqlite::records` | Transactional byte records and bounded ordered pages | `sqlite-records` |
//! | `db::records` | Byte identities and key ranges for namespaced records | `record-identity`, `record-key-range` |
//! | `id` | Random text, random words, and time-sortable IDs | `id` |
//! | `random` | Secure entropy, unit float sampling, and symmetric duration jitter | `random` |
//! | `secret_bytes` | Owned variable-length and fixed-length zeroizing secret buffers | `secret-bytes` |
//! | `crypto` | Hedged protocols and individually named algorithms | `crypto`, `crypto-json`, `password-derivation`, `signatures`, `ed25519` |
//! | `crypto::totp` | RFC 6238 codes, bounded time windows, and enrollment URIs | `totp` |
//! | `rate_limit` | Token buckets and bounded per-key concurrency admission | `rate-limit` |
//! | `http` | Form data, cookies, CSRF, CSP hashes, conditional responses, and security headers | `form-data`, `form-data-multipart`, `cookie-headers`, `cookies`, `csrf`, `csp`, `conditional-responses`, `security-headers` |
//! | `http::media_types` | Media type parsing and the media type of a file name's extension | `media-types`, `media-types-by-extension` |
//! | `http::range_requests`, `http::file_bodies` | Byte range requests and response bodies that read a file's byte range | `range-requests`, `file-bodies` |
//! | `http::sse` | Bounded framing, incremental decoding, and owned server streams | `sse` |
//! | `serialization::json` | Bounded JSON encoding and decoding | `json`, `json-secret` |
//! | `serialization::versioned_state` | Explicit versioned codecs and JSON state documents | `versioned-state` |
//! | `io::bounded_reads` | Bounded synchronous and async reads | `io`, `io-async` |
//! | `io::cooperative` | Synchronous I/O cancellation and byte progress | `io` |
//! | `fs` | Directory-relative files, atomic writes, copies, and moves | `fs`, `fs-json`, `fs-secret` |
//! | `os::credential_store` | Bounded byte secrets in explicit OS credential backends | `credential-store-secret-service`, `credential-store-macos-keychain` |
//! | `os::app_dirs` | Native user/application locations and ordered XDG config/data search paths | `app-dirs` |
//! | `processes` | Child processes, local locks, and shutdown signals | `child-process`, `child-process-http`, `process-lock`, `shutdown-signals` |
//! | `async_work::memoized_tasks` | Input-keyed async tasks and bounded passive observation | `tasks` |
//! | `html` | HTML construction, context-specific escaping, and name syntax | `html` |
//! | `path_matcher` | Flat and nested path matching | `path-matcher` |
//! | `tsgen` | Serde-aware TypeScript declarations | `tsgen` |
//! | `browser::theme` | Cookie-free pre-paint theme initialization | `browser-theme` |
//!
//! Features enable only their documented dependency closure. Native Rust UTF-8,
//! Result, and comparison facilities remain appropriate without library wrappers.
#![deny(missing_docs)]
#![forbid(unsafe_code)]

#[cfg(feature = "email")]
pub mod email;

#[cfg(feature = "unicode")]
pub mod text;

#[cfg(feature = "rate-limit")]
pub mod rate_limit;

#[cfg(feature = "browser-theme")]
pub mod browser;

pub mod bytes;
pub mod future;
pub mod time;

#[cfg(any(feature = "sqlite-transactions", feature = "record-identity"))]
pub mod db;

#[cfg(feature = "__internal")]
#[allow(unused_macros)]
macro_rules! internal_modules {
	(private; $($name:ident),* $(,)?) => {
		$(#[doc(hidden)] pub mod $name;)*
	};
	(crate; $($name:ident),* $(,)?) => {
		$(#[doc(hidden)] pub mod $name;)*
	};
}

#[cfg(not(feature = "__internal"))]
#[allow(unused_macros)]
macro_rules! internal_modules {
	(private; $($name:ident),* $(,)?) => {
		$(mod $name;)*
	};
	(crate; $($name:ident),* $(,)?) => {
		$(pub(crate) mod $name;)*
	};
}

#[cfg(any(feature = "credential-store", feature = "app-dirs"))]
pub mod os;

#[cfg(feature = "random")]
pub mod random;

#[cfg(feature = "secret-bytes")]
pub mod secret_bytes;

#[cfg(feature = "io")]
pub mod io;

#[cfg(any(feature = "json", feature = "versioned-state"))]
pub mod serialization;

/// Misuse-resistant cryptographic constructions.
#[cfg(feature = "crypto")]
pub mod crypto;

/// Sortable, random-alphabet, and random-word identifiers.
#[cfg(feature = "id")]
pub mod id;

#[cfg(feature = "tasks")]
pub mod async_work;
#[cfg(feature = "path-matcher")]
pub mod path_matcher;

// The TsGen derive macro resolves its generated trait paths through this crate name.
#[cfg(feature = "tsgen")]
extern crate self as primitives_rs;

/// Serde-aware Rust-to-TypeScript contract generation.
#[cfg(feature = "tsgen")]
pub mod tsgen;

/// Context-specific HTML escaping.
#[cfg(feature = "html")]
pub mod html;

pub mod encoding;
#[cfg(any(feature = "id", feature = "mnemonic"))]
mod english_words;
/// Filesystem operations with explicit publication and access policies.
#[cfg(all(feature = "fs", any(unix, windows)))]
pub mod fs;
#[cfg(any(
	feature = "sse",
	feature = "cookie-headers",
	feature = "cookies",
	feature = "csrf",
	feature = "csp",
	feature = "conditional-responses",
	feature = "security-headers",
	feature = "form-data",
	feature = "media-types",
	feature = "range-requests",
	feature = "file-bodies"
))]
pub mod http;

#[cfg(any(
	feature = "process-lock",
	feature = "child-process",
	feature = "shutdown-signals"
))]
pub mod processes;
