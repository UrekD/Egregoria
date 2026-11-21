pub mod determinism;
pub mod perf;
pub mod smoke;

use crate::Scenario;
use simulation::SimulationOptions;

pub const SCENARIOS: &[(Scenario, SimulationOptions)] = &[
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
