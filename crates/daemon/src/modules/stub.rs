use proto::ApiError;
use serde_json::{Value, json};

use crate::module::{Module, ModuleCtx, ModuleRequest, ModuleSpec, RecoverOutcome};
use crate::store::JournalEntry;

/// Placeholder for a module whose task is not done yet: it declares the real module's spec
/// (commands, events, tables) and answers every command with `NOT_IMPLEMENTED`. It never
/// changes anything, so it never writes journal entries.
pub struct StubModule {
    spec: &'static ModuleSpec,
}

impl StubModule {
    pub fn boxed(spec: &'static ModuleSpec) -> Box<dyn Module> {
        Box::new(StubModule { spec })
    }
}

impl Module for StubModule {
    fn spec(&self) -> &'static ModuleSpec {
        self.spec
    }

    fn handle(&mut self, req: &ModuleRequest<'_>, _ctx: &ModuleCtx) -> Result<Value, ApiError> {
        Err(ApiError::not_implemented(self.spec.name, &format!("{}.{}", self.spec.name, req.cmd)))
    }

    fn status(&self, _ctx: &ModuleCtx) -> Value {
        json!({ "implemented": false })
    }

    fn recover(&mut self, entry: &JournalEntry, _ctx: &ModuleCtx) -> Result<RecoverOutcome, ApiError> {
        Err(ApiError::not_implemented(self.spec.name, &format!("recover({})", entry.key)))
    }
}
