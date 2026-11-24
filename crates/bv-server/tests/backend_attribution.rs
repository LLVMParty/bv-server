//! Results carry the name of the leaf backend that produced them, through the
//! command router and the racing backend, so recordings can store it.

use std::sync::Arc;
use std::time::Duration;

use bv_server::{Backend, CommandRouterBackend, QueryResult, RacingBackend};
use bv_wire::raw::{BinaryRequest, ExprBuilder, SimplifyBlock};

struct Fixed {
    name: &'static str,
    delay_ms: u64,
    conclusive: bool,
}

impl Backend for Fixed {
    fn name(&self) -> &'static str {
        self.name
    }

    fn handle(&self, request: &BinaryRequest) -> bv_wire::Result<QueryResult> {
        std::thread::sleep(Duration::from_millis(self.delay_ms));
        Ok(match (request.target_ref(), self.conclusive) {
            (_, false) => QueryResult::unknown("gave up"),
            (Some(target_node), true) => QueryResult::simplified(SimplifyBlock {
                expression: request.expression.clone(),
                target_node,
            }),
            (None, true) => QueryResult::sat(None),
        })
    }
}

fn fixed(name: &'static str, delay_ms: u64, conclusive: bool) -> Arc<dyn Backend> {
    Arc::new(Fixed {
        name,
        delay_ms,
        conclusive,
    })
}

fn server(solvers: Vec<Arc<dyn Backend>>) -> CommandRouterBackend {
    CommandRouterBackend::new(
        fixed("simplifier", 0, true),
        Arc::new(RacingBackend::new(solvers)),
    )
}

fn requests() -> (BinaryRequest, BinaryRequest) {
    let mut builder = ExprBuilder::new();
    let x = builder.bv_var("x", 8).unwrap();
    let t = builder.bool_true().unwrap();
    builder.assert(t).unwrap();
    let solve = BinaryRequest::parse(&builder.build_solve_request(1, 0, false, false).unwrap());
    let simplify = BinaryRequest::parse(&builder.build_simplify_request(2, x).unwrap());
    (solve.unwrap(), simplify.unwrap())
}

#[test]
fn racing_winner_is_reported_through_router() {
    let backend = server(vec![fixed("slow", 500, true), fixed("fast", 0, true)]);
    let (solve, simplify) = requests();
    assert_eq!(backend.handle(&solve).unwrap().backend, Some("fast"));
    assert_eq!(
        backend.handle(&simplify).unwrap().backend,
        Some("simplifier")
    );
}

#[test]
fn inconclusive_race_reports_backend_that_answered_unknown() {
    let (solve, _) = requests();
    let result = RacingBackend::new(vec![
        fixed("quitter", 0, false),
        fixed("quitter2", 50, false),
    ])
    .handle(&solve)
    .unwrap();
    assert_eq!(result.backend, Some("quitter"));
}
