//! Names the clients need to know in advance. Everything else (which commands exist, their
//! arguments) comes from the daemon at handshake time.
//!
//! Command names on the wire are `<module>.<verb>`; a command may also declare a short
//! top-level name (`protect` for `task.protect`). The daemon accepts both.

/// Handshake; must be the first request on every connection.
pub const HELLO: &str = "core.hello";
pub const STATUS: &str = "core.status";
pub const LOG: &str = "core.log";

/// The short names of the everyday commands. A daemon test checks they all exist.
pub const COMMON_SHORT_NAMES: [&str; 10] =
    ["status", "log", "tasks", "away", "back", "protect", "renew", "unprotect", "prioritize", "free"];

/// Module ids. They are stored in the database (recovery journal, settings, table prefixes),
/// so a released id must never be renamed.
pub mod module {
    /// The daemon itself (`core.hello`, `core.status`, `core.log`, `core_*` tables).
    pub const CORE: &str = "core";
    pub const POWER: &str = "power";
    pub const NETWORK: &str = "network";
    pub const RESOURCE: &str = "resource";
    pub const TASK: &str = "task";
    pub const PERMISSION: &str = "permission";
    pub const SCOPE: &str = "scope";
    pub const EXAMPLE: &str = "example";
}
