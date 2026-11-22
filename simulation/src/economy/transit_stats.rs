use std::collections::BTreeMap;

use prototypes::Money;
use serde::de::{Error, SeqAccess, Visitor};
use serde::ser::SerializeTuple;
use serde::{Deserialize, Serialize};

use crate::economy::{Budget, BudgetCategory};
use crate::map_dynamic::LineID;
use crate::utils::resources::Resources;

#[derive(Default, Serialize, Deserialize)]
pub struct TransitStats {
    pub boardings: BTreeMap<LineID, u64>,
    /// Longest elapsed wait among passengers who boarded.
    pub max_wait_ticks: u64,
    pub(super) upkeep: UpkeepAccrual,
}

pub(super) enum UpkeepAccrual {
    LegacyHour(u64),
    BusTicks { tick: u64, pending: u64 },
}

impl Default for UpkeepAccrual {
    fn default() -> Self {
        Self::BusTicks {
            tick: 0,
            pending: 0,
        }
    }
}

// In 0.8.5 saves the final TransitStats field was an elapsed hour.
// An out-of-range hour introduces the tick cursor and unsettled bus-ticks.
const BUS_TICKS_FORMAT: u64 = u64::MAX;

impl Serialize for UpkeepAccrual {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match *self {
            Self::LegacyHour(hour) => serializer.serialize_u64(hour),
            Self::BusTicks { tick, pending } => {
                let mut tuple = serializer.serialize_tuple(3)?;
                tuple.serialize_element(&BUS_TICKS_FORMAT)?;
                tuple.serialize_element(&tick)?;
                tuple.serialize_element(&pending)?;
                tuple.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for UpkeepAccrual {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct AccrualVisitor;

        impl<'de> Visitor<'de> for AccrualVisitor {
            type Value = UpkeepAccrual;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an elapsed upkeep hour or a tagged bus-tick accrual")
            }

            fn visit_u64<E: Error>(self, hour: u64) -> Result<Self::Value, E> {
                Ok(UpkeepAccrual::LegacyHour(hour))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let header = seq
                    .next_element()?
                    .ok_or_else(|| Error::invalid_length(0, &self))?;
                if header != BUS_TICKS_FORMAT {
                    return self.visit_u64(header);
                }
                let tick = seq
                    .next_element()?
                    .ok_or_else(|| Error::invalid_length(1, &self))?;
                let pending = seq
                    .next_element()?
                    .ok_or_else(|| Error::invalid_length(2, &self))?;
                Ok(UpkeepAccrual::BusTicks { tick, pending })
            }
        }

        if deserializer.is_human_readable() {
            deserializer.deserialize_any(AccrualVisitor)
        } else {
            deserializer.deserialize_tuple(3, AccrualVisitor)
        }
    }
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
    fn p3c_reads_the_hour_cursor_from_an_existing_save() {
        let legacy = (BTreeMap::from([(LineID::default(), 4u64)]), 721u64, 9u64);
        let bytes = Bincode::encode(&legacy).unwrap();
        let stats: TransitStats = Bincode::decode(&bytes).unwrap();
        assert_eq!(stats.boardings, legacy.0);
        assert_eq!(stats.max_wait_ticks, legacy.1);
        assert!(matches!(stats.upkeep, UpkeepAccrual::LegacyHour(9)));
        assert_eq!(Bincode::encode(&stats).unwrap(), bytes);
    }

    #[test]
    fn p3c_upkeep_counts_ticks_when_a_bus_joins_or_leaves_at_the_boundary() {
        let mut sim = TestCtx::new_fast().g;
        let bus = spawn_driving_vehicle(
            &mut sim,
            VehicleKind::Bus,
            Transform::new(vec3(150.0, 100.0, 0.0)),
        )
        .unwrap();
        let run = BusRun {
            line: LineID::default(),
            next_stop_idx: 0,
            dwell_until: Some(Tick(u64::MAX)),
            leg_started: Tick(0),
            failures: 0,
        };
        let charge = |sim: &Simulation| sim.read::<Budget>().total(BudgetCategory::TransitUpkeep);
        for tick in 1..=2 * TICKS_PER_HOUR {
            if tick == TICKS_PER_HOUR {
                sim.write::<Transit>().runs.insert(bus, run.clone());
            }
            if tick == 2 * TICKS_PER_HOUR {
                sim.write::<Transit>().runs.remove(&bus);
            }
            let (world, resources) = sim.world_res();
            resources.write::<GameTime>().tick = Tick(INITIAL_TICK + tick);
            accrue_transit_upkeep(world, resources);
            accrue_transit_upkeep(world, resources);
            if tick == TICKS_PER_HOUR {
                assert_eq!(
                    charge(&sim),
                    -i128::from(Money::new_bucks(25).inner()) / i128::from(TICKS_PER_BUDGET_DAY)
                );
                sim = Bincode::decode(&Bincode::encode(&sim).unwrap()).unwrap();
            }
        }
        assert_eq!(charge(&sim), -i128::from(Money::new_bucks(25).inner()) / 24);
    }

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
        let hour = |sim: &mut Simulation, n: u64| {
            let (world, resources) = sim.world_res();
            for tick in ((n - 1) * TICKS_PER_HOUR + 1)..=n * TICKS_PER_HOUR {
                resources.write::<GameTime>().tick = Tick(INITIAL_TICK + tick);
                accrue_transit_upkeep(world, resources);
            }
            accrue_transit_upkeep(world, resources);
        };
        hour(&mut continuous, 1);
        let (_, resources) = continuous.world_res();
        on_board_with_fare(resources, line, fare, 12);
        let mut resumed: Simulation =
            Bincode::decode(&Bincode::encode(&continuous).unwrap()).unwrap();
        let (world, resources) = resumed.world_res();
        accrue_transit_upkeep(world, resources);

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
