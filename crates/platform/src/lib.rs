//! OS implementations of the framework's traits: transport (`daemon::transport`), system
//! calls (`daemon::sys`), the SQLite library (`daemon::sqlite::SqliteLibrary`), file
//! locations and console signals.
//!
//! Only Windows is implemented. Another OS adds a sibling directory (`src/linux/`) exposing
//! the same items, plus one `cfg` pair below; nothing outside this crate changes.

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;
