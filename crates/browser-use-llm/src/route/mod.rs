//! The runtime composition layer: `Protocol × Endpoint × Auth × Framing`.
//!
//! - [`Protocol`] / [`ProtocolStream`] — the wire-format contract (sync).
//! - [`Endpoint`] — where to send.
//! - [`Auth`] — composable header/credential builder.
//! - [`SseDecoder`] / [`SseFrame`] — byte-stream → SSE frame decoding.
//!
//! The async client/executor that drives a `Protocol` over HTTP lands in
//! `client` (WP 1.2 cont).

pub mod auth;
pub mod endpoint;
pub mod framing;
pub mod protocol;

pub use auth::Auth;
pub use endpoint::Endpoint;
pub use framing::{SseDecoder, SseFrame};
pub use protocol::{Protocol, ProtocolStream};
