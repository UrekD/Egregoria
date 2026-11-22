use std::collections::BTreeMap;

use prototypes::Money;
use serde::{Deserialize, Serialize};

use crate::economy::{Budget, BudgetCategory};
use crate::map_dynamic::LineID;
use crate::utils::resources::Resources;

#[derive(Default, Serialize, Deserialize)]
pub struct TransitStats {
    pub boardings: BTreeMap<LineID, u64>,
    /// Longest elapsed wait among passengers who boarded.
    pub max_wait_ticks: u64,
    pub(crate) last_upkeep_hour: u64,
}

/// Record ridership for one successful boarding.
pub fn on_board(stats: &mut TransitStats, line: LineID) {
    *stats.boardings.entry(line).or_default() += 1;
}

/// Post the validated fare and record the passenger's elapsed wait and boarding.
pub fn on_board_with_fare(resources: &Resources, line: LineID, fare: Money, wait_ticks: u64) {
    {
        let mut stats = resources.write::<TransitStats>();
        on_board(&mut stats, line);
        stats.max_wait_ticks = stats.max_wait_ticks.max(wait_ticks);
    }
    Budget::apply(resources, BudgetCategory::TransitFares, fare);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::economy::{
        accrue_transit_upkeep, Budget, BudgetCategory, Government, TICKS_PER_BUDGET_DAY,
    };
    use crate::map_dynamic::{BusRun, Transit};
    use crate::tests::TestCtx;
    use crate::transportation::{spawn_driving_vehicle, VehicleKind};
    use crate::world_command::WorldCommand;
    use crate::{Simulation, INITIAL_TICK};
    use common::saveload::{Bincode, Encoder};
    use geom::{vec3, Transform};
    use prototypes::{GameTime, Money, Tick, TICKS_PER_HOUR};

    #[test]
    fn p3c_fares_and_running_bus_upkeep_are_exact_across_save_load() {
        let mut continuous = TestCtx::new_fast().g;
        let opening = continuous.read::<Government>().money.inner();
        let opening_ledger = ledger_total(&continuous.read::<Budget>());
        let line = LineID::default();
        for x in [150.0, 180.0, 210.0] {
            let bus = spawn_driving_vehicle(
                &mut continuous,
                VehicleKind::Bus,
                Transform::new(vec3(x, 100.0, 0.0)),
            )
            .unwrap();
            if x < 210.0 {
                continuous.write::<Transit>().runs.insert(
                    bus,
                    BusRun {
                        line,
                        next_stop_idx: 0,
                        dwell_until: None,
                        leg_started: Tick(0),
                        failures: 0,
                    },
                );
            }
        }

        let fare = Money::new_cents(175);
        let hour = |sim: &mut Simulation, n| {
            let (world, resources) = sim.world_res();
            resources.write::<GameTime>().tick = Tick(INITIAL_TICK + n * TICKS_PER_HOUR);
            accrue_transit_upkeep(world, resources);
            accrue_transit_upkeep(world, resources);
        };
        hour(&mut continuous, 1);
        let (_, resources) = continuous.world_res();
        on_board_with_fare(resources, line, fare, 12);
        let mut resumed: Simulation =
            Bincode::decode(&Bincode::encode(&continuous).unwrap()).unwrap();

        for sim in [&mut continuous, &mut resumed] {
            for n in 2..=24 {
                hour(sim, n);
            }
            sim.apply_commands([&WorldCommand::SetGameTime(GameTime::new(Tick(0)))]);
            let (world, resources) = sim.world_res();
            accrue_transit_upkeep(world, resources);
            on_board_with_fare(resources, line, fare, 19);
            resources.write::<GameTime>().tick = Tick(1);
            on_board_with_fare(resources, line, Money::ZERO, 6);

            let budget = resources.read::<Budget>();
            assert_eq!(budget.elapsed_ticks(Tick(1)), TICKS_PER_BUDGET_DAY + 1);
            let daily_upkeep = -2 * i128::from(Money::new_bucks(25).inner());
            assert_eq!(
                budget.days()[&0][&BudgetCategory::TransitUpkeep],
                daily_upkeep
            );
            assert_eq!(
                budget.days()[&0][&BudgetCategory::TransitFares],
                2 * i128::from(fare.inner())
            );
            assert_eq!(budget.days().len(), 1);
            assert_eq!(
                i128::from(resources.read::<Government>().money.inner()),
                i128::from(opening) + ledger_total(&budget) - opening_ledger
            );
            let stats = resources.read::<TransitStats>();
            assert_eq!(stats.boardings[&line], 3);
            assert_eq!(stats.max_wait_ticks, 19);
        }
        assert_eq!(continuous.hashes(), resumed.hashes());
    }

    fn ledger_total(budget: &Budget) -> i128 {
        budget.days().values().flat_map(|day| day.values()).sum()
    }
}
