//! Built-in modules, one directory each.
//!
//! Adding a module: create `modules/<name>/mod.rs` with a `static SPEC: ModuleSpec` and
//! `pub fn create() -> Box<dyn Module>`, then add `<name>,` to the list below. Nothing else
//! changes: protocol, daemon loop, CLI and page all pick it up from its spec.

pub mod settings;
mod stub;

pub use stub::StubModule;

/// What `status` says about a change the administrator layer refused.
pub const NEEDS_ADMIN_NOTICE: &str = "这一项需要管理员权限，没生效 (needs administrator rights, not applied)";

macro_rules! register_modules {
    ($($name:ident),* $(,)?) => {
        $(pub mod $name;)*

        /// Every built-in module, in start order (stopped in reverse).
        pub fn builtin() -> Vec<Box<dyn crate::module::Module>> {
            vec![$($name::create()),*]
        }
    };
}

register_modules! {
    mode,
    power,
    network,
    resume,
    resource,
    task,
    permission,
    exit_guard,
    agents,
    files,
    scope,
    example,
}
