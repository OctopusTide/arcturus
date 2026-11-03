//! Shared by the daemon, the CLI and (later) the page and the tray: product names, the wire
//! protocol, the error-code table, the command catalog and a client that runs the protocol
//! over any byte stream. Portable: nothing here knows which OS or transport is underneath.
//!
//! See `docs/INTERFACES.md` for the human-readable version of everything here.

pub mod catalog;
pub mod admin;
pub mod client;
pub mod commands;
pub mod error;
pub mod names;
pub mod protocol;

pub use catalog::{ArgKind, Section};
pub use error::{ApiError, EXIT_OK, ErrorCode, subcode};
pub use protocol::{ClientInfo, PROTOCOL_VERSION, Request, Response};
pub use serde_json::Value;
