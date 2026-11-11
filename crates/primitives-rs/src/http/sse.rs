//! Standard server-sent event framing and bounded, cancellation-aware streams.
//!
//! [`Parser`] handles arbitrary byte boundaries, UTF-8 replacement, LF/CR/CRLF,
//! event IDs, and retry fields. [`DecoderStream`] adapts an HTTP client's byte
//! stream without background reads. [`serve`] owns a bounded producer and an
//! HTTP body; dropping that body cancels and aborts the producer.
//!
//! Payload serialization and application terminal events belong to the caller.
//! The wire format is ordinary `text/event-stream`, compatible with EventSource.
mod framing;
mod parser;
mod server;
mod stream;

pub use framing::{EncodeError, Event, encode_comment, encode_event};
pub use parser::{Frame, InitialEventIdError, ParseError, Parser};
pub use server::{
	Body, BodyError, EventSender, SendError, ServerOptions, ServerOptionsError, serve,
};
pub use stream::{DecoderStream, ReadError};
