use crate::map::{Map, RoadID};
use crate::map_dynamic::transit::elapsed_ticks;
use crate::utils::resources::Resources;
use crate::World;
use common::FastMap;
use prototypes::TICKS_PER_MINUTE;

const BUCKETS: usize = 10;
pub const ROAD_HISTORY_TICKS: u64 = BUCKETS as u64 * TICKS_PER_MINUTE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoadUserKind {
    Car,
    Truck,
    Bus,
    Pedestrian,
}

#[derive(Default)]
struct Bucket {
    minute: Option<u64>,
    entries: FastMap<RoadID, [u32; 4]>,
}

#[derive(Default)]
pub struct RoadThroughput {
    buckets: [Bucket; BUCKETS],
    map_identity: Option<u64>,
    first_tick: u64,
    now: u64,
}

impl RoadThroughput {
    pub(crate) fn begin_tick(&mut self, map: &Map, tick: u64) {
        let identity = Some(map.identity());
        if self.map_identity != identity {
            *self = Self {
                map_identity: identity,
                first_tick: tick.saturating_sub(1),
                ..Self::default()
            };
        }
        self.now = tick;
    }

    pub fn record_entry(&mut self, road: RoadID, kind: RoadUserKind, tick: u64) {
        let minute = tick / TICKS_PER_MINUTE;
        let bucket = &mut self.buckets[(minute % BUCKETS as u64) as usize];
        if bucket.minute != Some(minute) {
            bucket.entries.clear();
            bucket.minute = Some(minute);
        }
        let count = &mut bucket.entries.entry(road).or_default()[kind as usize];
        *count = count.saturating_add(1);
    }

    /// Recent entries in car, truck, bus and pedestrian order.
    /// Includes the current partial minute and the preceding nine whole minutes.
    pub fn recent(&self, road: RoadID, now: u64) -> [u32; 4] {
        let minute = now / TICKS_PER_MINUTE;
        let mut totals = [0u32; 4];
        for bucket in &self.buckets {
            if bucket.is_recent(minute) {
                if let Some(entries) = bucket.entries.get(&road) {
                    for (total, count) in totals.iter_mut().zip(entries) {
                        *total = total.saturating_add(*count);
                    }
                }
            }
        }
        totals
    }

    pub fn prune(&mut self, map: &Map, now: u64) {
        let minute = now / TICKS_PER_MINUTE;
        for bucket in &mut self.buckets {
            if bucket.is_recent(minute) {
                bucket
                    .entries
                    .retain(|road, _| map.roads().contains_key(*road));
            } else {
                bucket.entries.clear();
                bucket.minute = None;
            }
        }
    }

    /// Counts and elapsed history since the current world began recording.
    pub fn current_window(&self, road: RoadID) -> ([u32; 4], u64) {
        (
            self.recent(road, self.now),
            self.now
                .saturating_sub(self.first_tick)
                .min(ROAD_HISTORY_TICKS),
        )
    }
}

impl Bucket {
    fn is_recent(&self, now_minute: u64) -> bool {
        self.minute
            .is_some_and(|minute| minute <= now_minute && now_minute - minute < BUCKETS as u64)
    }
}

pub fn road_throughput_update(_world: &mut World, resources: &mut Resources) {
    let now = elapsed_ticks(resources).0;
    if now % TICKS_PER_MINUTE == 0 {
        let map = resources.read::<Map>();
        resources.write::<RoadThroughput>().prune(&map, now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::{LanePatternBuilder, MapProject, ProjectFilter, ProjectKind};
    use crate::map_dynamic::transit::elapsed_ticks;
    use crate::tests::TestCtx;
    use crate::world_command::WorldCommand;
    use crate::Simulation;
    use common::saveload::{Bincode, Encoder};
    use geom::vec3;
    use prototypes::{GameTime, Tick, TICKS_PER_MINUTE};

    #[test]
    fn u5_window_uses_elapsed_ticks_and_resets_on_load() {
        let mut ctx = TestCtx::new();
        ctx.tick_fast();
        let minute = TICKS_PER_MINUTE;
        let road = RoadID::default();
        let now = elapsed_ticks(&ctx.g.resources).0;
        ctx.g
            .write::<RoadThroughput>()
            .record_entry(road, RoadUserKind::Car, now);

        for tick in [0, 10_000_000] {
            ctx.apply(&[WorldCommand::SetGameTime(GameTime::new(Tick(tick)))]);
            assert_eq!(elapsed_ticks(&ctx.g.resources).0, now);
            assert_eq!(
                ctx.g.read::<RoadThroughput>().recent(road, now),
                [1, 0, 0, 0]
            );
        }

        let resumed: Simulation = Bincode::decode(&Bincode::encode(&ctx.g).unwrap()).unwrap();
        assert_eq!(resumed.read::<RoadThroughput>().recent(road, now), [0; 4]);

        let mut through = ctx.g.write::<RoadThroughput>();
        through.record_entry(road, RoadUserKind::Truck, 2 * minute);
        through.record_entry(road, RoadUserKind::Bus, 2 * minute);
        through.record_entry(road, RoadUserKind::Pedestrian, 2 * minute);
        assert_eq!(through.recent(road, 10 * minute - 1), [1, 1, 1, 1]);
        assert_eq!(through.recent(road, 10 * minute), [0, 1, 1, 1]);
        assert_eq!(through.recent(road, 12 * minute), [0; 4]);
        through.record_entry(road, RoadUserKind::Truck, 12 * minute);
        assert_eq!(through.recent(road, 12 * minute), [0, 1, 0, 0]);
    }

    #[test]
    fn u5_prunes_split_and_demolished_roads_on_bucket_boundary() {
        let mut ctx = TestCtx::new();
        let a = vec3(60.0, 150.0, 0.0);
        let b = vec3(280.0, 150.0, 0.0);
        let c = vec3(440.0, 310.0, 0.0);
        ctx.build_roads(&[a, b, c]);
        let start = (a + b) * 0.5;
        let end = (b + c) * 0.5;
        let (split, demolished) = {
            let map = ctx.g.map();
            let ProjectKind::Road(split) = map.project(start, 0.0, ProjectFilter::ROAD).kind else {
                panic!("missing road to split");
            };
            let ProjectKind::Road(demolished) = map.project(end, 0.0, ProjectFilter::ROAD).kind
            else {
                panic!("missing road to demolish");
            };
            (split, demolished)
        };
        let base = ctx.g.get_tick();
        let tick = elapsed_ticks(&ctx.g.resources).0;
        let boundary = ((tick / TICKS_PER_MINUTE) + 1) * TICKS_PER_MINUTE;
        for road in [split, demolished] {
            ctx.g
                .write::<RoadThroughput>()
                .record_entry(road, RoadUserKind::Car, tick);
        }
        {
            let mut map = ctx.g.map_mut();
            let a = MapProject {
                pos: start,
                kind: ProjectKind::Road(split),
            };
            let b = MapProject {
                pos: start + vec3(0.0, 170.0, 0.0),
                kind: ProjectKind::Ground,
            };
            map.make_connection(a, b, None, &LanePatternBuilder::default().build())
                .unwrap();
            map.remove_road(demolished).unwrap();
            assert!(map.get(split).is_none());
            assert!(map.get(demolished).is_none());
        }
        let (world, resources) = ctx.g.world_res();
        for now in [boundary - 1, boundary] {
            *resources.write::<GameTime>() = GameTime::new(Tick(base + now - tick));
            road_throughput_update(world, resources);
            let expected = if now == boundary {
                [0; 4]
            } else {
                [1, 0, 0, 0]
            };
            for road in [split, demolished] {
                assert_eq!(
                    resources.read::<RoadThroughput>().recent(road, now),
                    expected
                );
            }
        }
    }
}
