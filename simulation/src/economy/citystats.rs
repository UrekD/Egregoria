use prototypes::{Money, TICKS_PER_HOUR, TICKS_PER_MINUTE};
use serde::{Deserialize, Deserializer, Serialize};

use super::{Government, LEVEL_NAMES};
use crate::map::BuildingKind;
use crate::Simulation;

const SAMPLES_PER_LEVEL: usize = 10;

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct CityStats {
    pub population: u32,
    pub employed: u32,
    pub residences: u32,
    pub companies: u32,
    pub vehicles: u32,
    pub money: Money,
    pub history: CityStatsHistory,
}

#[derive(Default, Copy, Clone, Serialize, Deserialize)]
pub struct CityStatsSample {
    pub population: u32,
    pub employed: u32,
    pub residences: u32,
    pub companies: u32,
    pub vehicles: u32,
    pub money: Money,
}

#[derive(Default, Clone, Serialize, Deserialize)]
struct CityStatsHistoryLevel {
    samples: [Option<CityStatsSample>; SAMPLES_PER_LEVEL],
    #[serde(deserialize_with = "deserialize_history_cursor")]
    cursor: usize,
}

fn deserialize_history_cursor<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(usize::deserialize(deserializer)?.min(SAMPLES_PER_LEVEL - 1))
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct CityStatsHistory {
    levels: [CityStatsHistoryLevel; LEVEL_NAMES.len()],
}

impl CityStatsHistory {
    /// Tick intervals for the 10m, 1h, 10h, and 50h game-time levels.
    pub const INTERVALS: [u64; LEVEL_NAMES.len()] = [
        TICKS_PER_MINUTE,
        6 * TICKS_PER_MINUTE,
        TICKS_PER_HOUR,
        5 * TICKS_PER_HOUR,
    ];

    /// Recorded samples for a level, from oldest to newest.
    pub fn samples(&self, level: usize) -> impl Iterator<Item = &CityStatsSample> {
        self.levels.get(level).into_iter().flat_map(|level| {
            let (before_cursor, after_cursor) = level.samples.split_at(level.cursor);
            after_cursor.iter().chain(before_cursor).flatten()
        })
    }

    fn record(&mut self, tick: u64, sample: CityStatsSample) {
        for (level, interval) in self.levels.iter_mut().zip(Self::INTERVALS) {
            if tick % interval == 0 {
                level.samples[level.cursor] = Some(sample);
                level.cursor = (level.cursor + 1) % SAMPLES_PER_LEVEL;
            }
        }
    }
}

pub fn citystats_update(sim: &mut Simulation) {
    let tick = sim.get_tick();
    if tick % TICKS_PER_MINUTE != 0 {
        return;
    }

    let world = sim.world();
    let mut stats = sim.write::<CityStats>();
    stats.population = world.humans.len() as u32;
    stats.employed = world
        .humans
        .values()
        .filter(|human| human.work.is_some())
        .count() as u32;
    stats.residences = sim
        .map()
        .buildings()
        .values()
        .filter(|building| building.kind == BuildingKind::House)
        .count() as u32;
    stats.companies = world.companies.len() as u32;
    stats.vehicles = world.vehicles.len() as u32;
    stats.money = sim.read::<Government>().money;

    let sample = CityStatsSample {
        population: stats.population,
        employed: stats.employed,
        residences: stats.residences,
        companies: stats.companies,
        vehicles: stats.vehicles,
        money: stats.money,
    };
    stats.history.record(tick, sample);
}

#[cfg(test)]
mod tests {
    use super::{CityStats, CityStatsSample, SAMPLES_PER_LEVEL};
    use crate::economy::Government;
    use crate::map::{LanePatternBuilder, MapProject};
    use crate::tests::TestCtx;
    use crate::world_command::WorldCommand;
    use common::saveload::{Bincode, Encoder};
    use geom::vec3;
    use prototypes::{GameTime, Tick, TICKS_PER_MINUTE};

    #[test]
    fn citystats_repairs_out_of_range_history_cursor() {
        let mut stats = CityStats::default();
        stats.history.levels[0].samples[0] = Some(CityStatsSample {
            population: 1,
            ..CityStatsSample::default()
        });
        stats.history.levels[0].cursor = SAMPLES_PER_LEVEL + 1;

        let encoded = Bincode::encode(&stats).unwrap();
        let mut restored: CityStats = Bincode::decode(&encoded).unwrap();
        assert_eq!(restored.history.samples(0).count(), 1);
        restored.history.record(
            TICKS_PER_MINUTE,
            CityStatsSample {
                population: 2,
                ..CityStatsSample::default()
            },
        );
        let populations: Vec<_> = restored.history.samples(0).map(|s| s.population).collect();
        assert_eq!(populations, [1, 2]);
    }

    #[test]
    fn citystats_counts_new_resident_on_sampling_tick() {
        let mut ctx = TestCtx::new();
        ctx.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(300.0, 100.0, 0.0)]);
        let lot = ctx.g.map().lots().keys().next().unwrap();
        ctx.set_residential_demand(40.0);
        ctx.apply(&[
            WorldCommand::SetGameTime(GameTime::new(Tick(TICKS_PER_MINUTE - 1))),
            WorldCommand::MapBuildHouse(lot),
        ]);
        assert!(ctx.g.world().humans.is_empty());
        ctx.tick();
        {
            let world = ctx.g.world();
            let stats = ctx.g.read::<CityStats>();
            assert_eq!(world.humans.len(), 1);
            assert_eq!(stats.population, 1);
            assert_eq!(stats.residences, 1);
            assert_eq!(stats.employed, 0);
            assert_eq!(stats.companies, 0);
            assert_eq!(stats.vehicles as usize, world.vehicles.len());
            assert_eq!(stats.money, ctx.g.read::<Government>().money);
        }
        ctx.tick();
        assert_eq!(ctx.g.world().humans.len(), 2);
        let stats = ctx.g.read::<CityStats>();
        assert_eq!(stats.population, 1);
        let mut samples = stats.history.samples(0);
        let sample = samples.next().unwrap();
        assert_eq!(sample.population, 1);
        assert_eq!(sample.money, stats.money);
        assert!(samples.next().is_none());
    }

    #[test]
    fn citystats_counts_new_resident_without_parking() {
        let mut ctx = TestCtx::new();
        ctx.g
            .map_mut()
            .make_connection(
                MapProject::ground(vec3(100.0, 100.0, 0.0)),
                MapProject::ground(vec3(300.0, 100.0, 0.0)),
                None,
                &LanePatternBuilder::default().parking(false).build(),
            )
            .unwrap();
        let lot = ctx.g.map().lots().keys().next().unwrap();
        ctx.set_residential_demand(40.0);
        ctx.apply(&[
            WorldCommand::SetGameTime(GameTime::new(Tick(TICKS_PER_MINUTE - 1))),
            WorldCommand::MapBuildHouse(lot),
        ]);
        assert!(ctx.g.world().humans.is_empty());
        ctx.tick();

        let stats = ctx.g.read::<CityStats>();
        assert_eq!(stats.population, 1);
        assert_eq!(stats.residences, 1);
        assert_eq!(stats.employed, 0);
        assert_eq!(stats.companies, 0);
        assert_eq!(stats.vehicles, 0);
        assert_eq!(stats.money, ctx.g.read::<Government>().money);

        let mut samples = stats.history.samples(0);
        let sample = samples.next().unwrap();
        assert_eq!(sample.population, 1);
        assert_eq!(sample.money, stats.money);
        assert!(samples.next().is_none());
    }
}
