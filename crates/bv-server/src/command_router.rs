use std::sync::Arc;

use bv_wire::raw::{BinaryRequest, Command};

use crate::backend::{Backend, QueryResult, SolveContext};

/// Routes expression simplification to a dedicated simplifier backend while
/// keeping solve/optimization requests on the normal solver backend.
#[derive(Clone)]
pub struct CommandRouterBackend {
    simplifier: Arc<dyn Backend>,
    solver: Arc<dyn Backend>,
}

impl CommandRouterBackend {
    pub fn new(simplifier: Arc<dyn Backend>, solver: Arc<dyn Backend>) -> Self {
        Self { simplifier, solver }
    }
}

impl Backend for CommandRouterBackend {
    fn name(&self) -> &'static str {
        "command-router"
    }

    fn supports_qfbvsmtrs_text_fallback(&self) -> bool {
        self.solver.supports_qfbvsmtrs_text_fallback()
    }

    fn handle(&self, request: &BinaryRequest) -> bv_wire::Result<QueryResult> {
        match request.envelope.command {
            Command::Simplify => self
                .simplifier
                .handle(request)
                .map(|result| result.attributed_to(self.simplifier.name())),
            Command::Solve | Command::Minimize | Command::Maximize => self
                .solver
                .handle(request)
                .map(|result| result.attributed_to(self.solver.name())),
        }
    }

    fn handle_with_context(
        &self,
        request: &BinaryRequest,
        context: &SolveContext,
    ) -> bv_wire::Result<QueryResult> {
        match request.envelope.command {
            Command::Simplify => self
                .simplifier
                .handle_with_context(request, context)
                .map(|result| result.attributed_to(self.simplifier.name())),
            Command::Solve | Command::Minimize | Command::Maximize => self
                .solver
                .handle_with_context(request, context)
                .map(|result| result.attributed_to(self.solver.name())),
        }
    }
}
