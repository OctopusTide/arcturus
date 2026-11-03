//! 对话范围 (scope): which accounts see which local conversations. Optional and off by
//! default. Stub until task 17.

use crate::module::{Module, ModuleSpec, Section};
use crate::modules::StubModule;

static SPEC: ModuleSpec =
    ModuleSpec::new("scope", Section::AccountsStorage, "Conversation scope across accounts (optional)").off_by_default();

pub fn create() -> Box<dyn Module> {
    StubModule::boxed(&SPEC)
}
