mod budget_day;
pub mod determinism;
pub mod growth_basic;
pub mod perf;
pub mod services_basic;
pub mod services_neglect;
pub mod smoke;

use crate::builders::{grid_city, place_grid_power_plant, place_service_sites};
use crate::{Scenario, ScenarioCtx};
use geom::vec2;
use simulation::economy::MAX_TAX_RATE_PERMILLE;
use simulation::map::ZoneKind;
use simulation::world_command::WorldCommand;
use simulation::SimulationOptions;

pub const PERF_10K: (Scenario, SimulationOptions) = (
    Scenario {
        name: "perf_10k",
        run: perf::run_10k,
    },
    perf::OPTIONS,
);

pub const SCENARIOS: &[(Scenario, SimulationOptions)] = &[
    (
        Scenario {
            name: budget_day::SCENARIO.0.name,
            run: run_served_budget_day,
        },
        budget_day::SCENARIO.1,
    ),
    growth_basic::SCENARIO,
    services_basic::SCENARIO,
    services_neglect::SCENARIO,
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

fn run_served_budget_day(ctx: &mut ScenarioCtx) -> Result<(), String> {
    // An approach from the north keeps the power spur connected at the
    // corner while the smoke grid adds its first eastbound road.
    grid_city(ctx, vec2(700.0, 475.0), 1, 2, 225.0);
    let spur_origin = vec2(700.0, 500.0);
    place_grid_power_plant(ctx, spur_origin)?;
    place_service_sites(
        ctx,
        spur_origin,
        [
            ("water-pump", vec2(-225.0, 230.0)),
            ("sewage-outlet", vec2(-145.0, 230.0)),
            ("fire-station", vec2(-65.0, 230.0)),
        ],
    )?;
    ctx.submit(&[WorldCommand::SetTaxRate {
        zone: ZoneKind::Residential,
        rate_permille: MAX_TAX_RATE_PERMILLE,
    }]);
    ctx.run_ticks(1);
    (budget_day::SCENARIO.0.run)(ctx)
}
