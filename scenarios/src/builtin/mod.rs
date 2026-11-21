pub mod determinism;
pub mod growth_basic;
pub mod perf;
pub mod smoke;

use crate::Scenario;
use simulation::SimulationOptions;

pub const SCENARIOS: &[(Scenario, SimulationOptions)] = &[
    { #[path = "budget_day.rs"] mod budget_day; budget_day::SCENARIO },
    growth_basic::SCENARIO,
    (
        Scenario {
            name: "smoke",
            run: smoke::run,
        },
        smoke::OPTIONS,
    ),
    (
        Scenario {
            name: "determinism",
            run: determinism::run,
        },
        smoke::OPTIONS,
    ),
    (
        Scenario {
            name: "perf",
            run: perf::run,
        },
        perf::OPTIONS,
    ),
];
