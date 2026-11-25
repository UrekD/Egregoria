use crate::map::{Map, RoadID};
use crate::map_dynamic::transit::elapsed_ticks;
use crate::utils::resources::Resources;
use crate::World;
use common::FastMap;
use prototypes::TICKS_PER_MINUTE;
use std::collections::VecDeque;

pub const ROAD_HISTORY_TICKS: u64 = 10 * TICKS_PER_MINUTE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoadUserKind {
    Car,
    Truck,
    Bus,
    Pedestrian,
}

#[derive(Clone)]
struct TickEntries {
    tick: u64,
    counts: [u32; 4],
}

/// Counts changes of road on reaching a travel lane, including departures
/// from roadside parking. Turns and lane changes within one road add no entry.
#[derive(Clone, Default)]
pub struct RoadThroughput {
    entries: FastMap<RoadID, VecDeque<TickEntries>>,
    map_identity: Option<u64>,
    first_tick: u64,
    now: u64,
}

impl RoadThroughput {
    /// Carry runtime history to a detached copy of the same map. The save
    /// decoder assigns the copy a new map identity while retaining road IDs.
    pub fn for_map_copy(&self, source: &Map) -> impl FnOnce(&Map) -> Self {
        let mut history = self.clone();
        let bound_to_source = self.map_identity == Some(source.identity());
        move |copy| {
            if bound_to_source {
                history.map_identity = Some(copy.identity());
            }
            history
        }
    }

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
        let entries = self.entries.entry(road).or_default();
        if entries.back().is_none_or(|latest| latest.tick != tick) {
            entries.push_back(TickEntries {
                tick,
                counts: [0; 4],
            });
        }
        let count = &mut entries.back_mut().unwrap().counts[kind as usize];
        *count = count.saturating_add(1);
    }

    /// Entries in the last ten game minutes, in car, truck, bus and pedestrian order.
    pub fn recent(&self, road: RoadID, now: u64) -> [u32; 4] {
        let window = window_start(now)..=now;
        let mut totals = [0u32; 4];
        if let Some(entries) = self.entries.get(&road) {
            for entry in entries.iter().filter(|entry| window.contains(&entry.tick)) {
                for (total, count) in totals.iter_mut().zip(entry.counts) {
                    *total = total.saturating_add(count);
                }
            }
        }
        totals
    }

    pub fn prune(&mut self, map: &Map, now: u64) {
        let oldest = window_start(now);
        self.entries.retain(|road, entries| {
            if !map.roads().contains_key(*road) {
                return false;
            }
            entries.retain(|entry| entry.tick >= oldest);
            !entries.is_empty()
        });
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

fn window_start(now: u64) -> u64 {
    now.saturating_sub(ROAD_HISTORY_TICKS - 1)
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
    fn u5_throughput_keeps_exactly_ten_minutes_across_minute_boundaries() {
        let ctx = TestCtx::new();
        ctx.build_roads(&[vec3(70.0, 180.0, 0.0), vec3(270.0, 180.0, 0.0)]);
        let map = ctx.g.map();
        let road = map.roads().keys().next().unwrap();
        let history = ROAD_HISTORY_TICKS;
        let mut through = RoadThroughput::default();
        through.begin_tick(&map, 1);
        through.record_entry(road, RoadUserKind::Car, 1);
        through.record_entry(road, RoadUserKind::Truck, 2);
        through.begin_tick(&map, history - 1);
        assert_eq!(through.current_window(road), ([1, 1, 0, 0], history - 1));
        through.begin_tick(&map, history);
        through.record_entry(road, RoadUserKind::Bus, history);
        through.prune(&map, history);
        assert_eq!(through.current_window(road), ([1, 1, 1, 0], history));
        assert_eq!(through.recent(road, history + 1), [0, 1, 1, 0]);
        assert_eq!(through.recent(road, history + 2), [0, 0, 1, 0]);
    }

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
        assert_eq!(through.recent(road, 10 * minute), [1, 1, 1, 1]);
        assert_eq!(through.recent(road, 12 * minute), [0; 4]);
        through.record_entry(road, RoadUserKind::Truck, 12 * minute);
        assert_eq!(through.recent(road, 12 * minute), [0, 1, 0, 0]);
    }

    #[test]
    fn u5_throughput_preserves_paused_history_and_resets_on_map_replacement() {
        let mut ctx = TestCtx::new();
        ctx.build_roads(&[vec3(70.0, 180.0, 0.0), vec3(270.0, 180.0, 0.0)]);
        let road = ctx.g.map().roads().keys().next().unwrap();
        ctx.tick_fast();
        let now = elapsed_ticks(&ctx.g.resources).0;
        ctx.g
            .write::<RoadThroughput>()
            .record_entry(road, RoadUserKind::Car, now);

        // Paused commands can change the calendar without running the schedule.
        ctx.g
            .apply_commands([&WorldCommand::SetGameTime(GameTime::new(Tick(8_000_000)))]);
        assert_eq!(elapsed_ticks(&ctx.g.resources).0, now);
        assert_eq!(
            ctx.g.read::<RoadThroughput>().current_window(road),
            ([1, 0, 0, 0], 1)
        );

        // Reuse the road IDs while replacing only the map resource.
        let replacement: Map = Bincode::decode(&Bincode::encode(&*ctx.g.map()).unwrap()).unwrap();
        assert!(replacement.get(road).is_some());
        *ctx.g.map_mut() = replacement;
        ctx.tick_fast();
        assert_eq!(elapsed_ticks(&ctx.g.resources).0, now + 1);
        assert_eq!(
            ctx.g.read::<RoadThroughput>().current_window(road),
            ([0; 4], 1)
        );
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
