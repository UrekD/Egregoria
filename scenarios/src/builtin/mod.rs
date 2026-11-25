mod budget_day;
pub mod demo_cities;
pub mod determinism;
mod economy_loop;
pub mod growth_basic;
pub mod perf;
pub mod services_basic;
pub mod services_neglect;
pub mod smoke;
mod transit_basic;
mod water_basic;
pub mod water_fixtures;
mod zones2;

use crate::builders::{grid_city, place_company, place_grid_power_plant, place_service_sites};
use crate::{Scenario, ScenarioCtx};
use geom::vec2;
use simulation::SimulationOptions;

pub const PERF_10K: (Scenario, SimulationOptions) = (
    Scenario {
        name: "perf_10k",
        run: perf::run_10k,
    },
    perf::OPTIONS,
);

pub const ZONES2_L3: (Scenario, SimulationOptions) = zones2::ATTAINMENT;

pub const ZONES2_SCENARIOS: &[(Scenario, SimulationOptions)] =
    &[zones2::CITY, zones2::CONTROLLED, ZONES2_L3];

pub const SCENARIOS: &[(Scenario, SimulationOptions)] = &[
    (
        Scenario {
            name: budget_day::SCENARIO.0.name,
            run: run_served_budget_day,
        },
        budget_day::SCENARIO.1,
    ),
    economy_loop::SCENARIO,
    growth_basic::SCENARIO,
    services_basic::SCENARIO,
    services_neglect::SCENARIO,
    transit_basic::SCENARIO,
    water_basic::SCENARIO,
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
    zones2::CITY,
    zones2::CONTROLLED,
    ZONES2_L3,
];

fn run_served_budget_day(ctx: &mut ScenarioCtx) -> Result<(), String> {
    // An approach from the north keeps the power spur connected at the
    // corner while the smoke grid adds its first eastbound road.
    grid_city(ctx, vec2(700.0, 475.0), 1, 2, 225.0);
    let spur_origin = vec2(700.0, 500.0);
    let plant = place_grid_power_plant(ctx, spur_origin)?;
    let sites = place_service_sites(
        ctx,
        spur_origin,
        [
            ("water-pump", vec2(-225.0, 230.0)),
            ("sewage-outlet", vec2(-145.0, 230.0)),
            ("fire-station", vec2(-65.0, 230.0)),
        ],
    )?;
    place_company(ctx, "coal-mine", spur_origin + vec2(40.0, 75.0))
        .ok_or("could not place coal supplier for the budget fixture")?;
    (budget_day::SCENARIO.0.run)(ctx)?;
    services_basic::check_service_network(ctx, plant, &sites, "budget_day final")
}
