use std::cell::RefCell;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use geom::{Camera, InfiniteFrustrum, Intersect3, Vec3, AABB3};
use goryak::{
    blur_bg, fixed_spacer, mincolumn, minrow, on_secondary_container, padxy, secondary_container,
    textc,
};
use simulation::map::{Map, ProjectFilter, ProjectKind, Road, RoadID, TurnID, TurnKind};
use simulation::transportation::traffic_metrics::{road_network_revision, RoadSampler};
use simulation::Simulation;
use yakui::{colored_box_container, reflow, Alignment, Color, Dim2, Pivot, Vec4};

use super::view;
use crate::rendering::immediate::ImmediateDraw;
use crate::uiworld::UiWorld;

pub(super) const AVAILABLE: bool = true;

const SAMPLE_INTERVAL: Duration = Duration::from_millis(500);
const SAMPLE_EXPIRY: Duration = Duration::from_millis(1500);
const SAMPLE_BUDGET: Duration = Duration::from_millis(2);
const ROADS_PER_BATCH: usize = 64;
const MAX_BATCHES_PER_FRAME: usize = 2;
const EMA_ALPHA: f32 = 0.3;
const ROAD_LIFT: f32 = 0.55;
const NO_DATA: [f32; 4] = [0.35, 0.38, 0.42, 1.0];

#[derive(Default)]
struct TrafficOverlay {
    revision: (u64, u64),
    road_bounds: Option<AABB3>,
    projection: [f32; 16],
    visible: Vec<RoadID>,
    samplers: Vec<RoadSampler>,
    next_road: Option<usize>,
    history: BTreeMap<RoadID, RecentTraffic>,
    last_sample: Option<Instant>,
}

struct RecentTraffic {
    rel_speed: f32,
    observed_at: Instant,
    points: Vec<Vec3>,
    turns: Vec<TurnID>,
}

fn road_turns<'a>(map: &'a Map, road: &'a Road) -> impl Iterator<Item = TurnID> + 'a {
    [road.src, road.dst]
        .into_iter()
        .filter_map(|id| map.get(id))
        .flat_map(|intersection| intersection.turns())
        .filter(|turn| {
            turn.kind == TurnKind::Driving
                && road
                    .lanes_iter()
                    .any(|(lane, _)| lane == turn.id.src || lane == turn.id.dst)
        })
        .map(|turn| turn.id)
}

thread_local! {
    static TRAFFIC: RefCell<TrafficOverlay> = RefCell::default();
}

pub(super) fn draw(
    sim: &Simulation,
    map: &Map,
    uiw: &UiWorld,
    draw: &mut ImmediateDraw,
    rebuild: bool,
) {
    let camera = uiw.camera().camera;
    TRAFFIC.with_borrow_mut(|traffic| {
        traffic.update(sim, map, &camera, draw, rebuild, Instant::now());
    });
}

impl TrafficOverlay {
    fn update(
        &mut self,
        sim: &Simulation,
        map: &Map,
        camera: &Camera,
        draw: &mut ImmediateDraw,
        rebuild: bool,
        now: Instant,
    ) {
        let revision = road_network_revision(map);
        if self.revision.0 != revision.0 {
            *self = Self::default();
        }
        let roads_changed = self.revision != revision;
        if roads_changed {
            self.revision = revision;
            self.road_bounds = road_geometry_bounds(map);
            self.history.retain(|&id, recent| {
                map.get(id).is_some_and(|road| {
                    road.interfaced_points.as_slice() == recent.points
                        && road_turns(map, road).eq(recent.turns.iter().copied())
                })
            });
        }
        let projection = *camera.proj_cache.as_ref();
        let view_changed = rebuild || roads_changed || self.projection != projection;
        if view_changed {
            self.projection = projection;
            let visible = visible_roads(camera, map, self.road_bounds);
            if roads_changed || self.visible != visible {
                self.samplers.clear();
                self.next_road = None;
                self.visible = visible;
            }
            self.history
                .retain(|road, _| self.visible.binary_search(road).is_ok());
        }

        if view_changed {
            draw.clear_persistent();
            for road in self.visible.iter().filter_map(|&id| map.get(id)) {
                let color = self
                    .history
                    .get(&road.id)
                    .map_or(NO_DATA, |recent| speed_color(recent.rel_speed));
                let points: Vec<_> = road
                    .interfaced_points
                    .iter()
                    .map(|point| point.up(ROAD_LIFT))
                    .collect();
                let width = (road.width * 0.40).clamp(3.0, 9.0);
                draw.polyline(points, width, false)
                    .color(color)
                    .persistent();
            }
        }

        if self.next_road.is_none()
            && !self.visible.is_empty()
            && self
                .last_sample
                .is_none_or(|last| now - last >= SAMPLE_INTERVAL)
        {
            self.last_sample = Some(now);
            self.next_road = Some(0);
        }

        let start = Instant::now();
        let mut previous = Duration::ZERO;
        self.sample_frame(sim, map, draw, now, || {
            let elapsed = start.elapsed();
            let batch_cost = elapsed - previous;
            previous = elapsed;
            elapsed + batch_cost >= SAMPLE_BUDGET
        });
    }

    fn sample_frame(
        &mut self,
        sim: &Simulation,
        map: &Map,
        draw: &mut ImmediateDraw,
        now: Instant,
        mut out_of_budget: impl FnMut() -> bool,
    ) {
        for _ in 0..MAX_BATCHES_PER_FRAME {
            let Some(start) = self.next_road else {
                break;
            };
            if out_of_budget() {
                break;
            }
            let end = (start + ROADS_PER_BATCH).min(self.visible.len());
            let roads = &self.visible[start..end];
            let batch = start / ROADS_PER_BATCH;
            if batch == self.samplers.len() {
                self.samplers
                    .push(RoadSampler::new(map, roads.iter().copied()));
            }
            let observed = self.samplers[batch].sample(sim);
            for (index, &id) in (start..end).zip(roads) {
                if self
                    .history
                    .get(&id)
                    .is_some_and(|recent| now - recent.observed_at >= SAMPLE_EXPIRY)
                {
                    self.history.remove(&id);
                }
                if let Some(observed) = observed.get(&id) {
                    if let Some(recent) = self.history.get_mut(&id) {
                        recent.rel_speed += EMA_ALPHA * (observed.rel_speed - recent.rel_speed);
                        recent.observed_at = now;
                    } else if let Some(road) = map.get(id) {
                        self.history.insert(
                            id,
                            RecentTraffic {
                                rel_speed: observed.rel_speed,
                                observed_at: now,
                                points: road.interfaced_points.as_slice().to_vec(),
                                turns: road_turns(map, road).collect(),
                            },
                        );
                    }
                }
                let color = self
                    .history
                    .get(&id)
                    .map_or(NO_DATA, |recent| speed_color(recent.rel_speed));
                draw.persistent_orders[index].color = color.into();
            }
            self.next_road = (end < self.visible.len()).then_some(end);
        }
    }
}

fn road_geometry_bounds(map: &Map) -> Option<AABB3> {
    map.roads()
        .values()
        .map(|road| {
            let bounds = road.interfaced_points.bbox();
            AABB3::new(bounds.ll.up(ROAD_LIFT), bounds.ur.up(ROAD_LIFT))
        })
        .reduce(AABB3::union)
}

fn visible_roads(camera: &Camera, map: &Map, road_bounds: Option<AABB3>) -> Vec<RoadID> {
    let Some(bounds) = road_bounds.and_then(|bounds| view::geometry_bounds(camera, bounds)) else {
        return Vec::new();
    };
    let frustum = InfiniteFrustrum::from_reversez_invviewproj(camera.eye(), camera.inv_proj_cache);
    let mut roads: Vec<_> = map
        .spatial_map()
        .query(bounds, ProjectFilter::ROAD)
        .filter_map(|kind| match kind {
            ProjectKind::Road(id) => map.get(id),
            _ => None,
        })
        .filter(|road| {
            road.interfaced_points.segments().any(|segment| {
                let from = segment.src.up(ROAD_LIFT);
                let to = segment.dst.up(ROAD_LIFT);
                frustum.intersects(&AABB3::new(from.min(to), from.max(to)))
            })
        })
        .map(|road| road.id)
        .collect();
    roads.sort_unstable();
    roads.dedup();
    roads
}

fn speed_color(speed: f32) -> [f32; 4] {
    const RED: [f32; 3] = [0.92, 0.045, 0.07];
    const AMBER: [f32; 3] = [0.98, 0.50, 0.025];
    const GREEN: [f32; 3] = [0.025, 0.78, 0.24];

    let (from, to, t) = if speed <= 0.5 {
        (RED, AMBER, speed * 2.0)
    } else {
        (AMBER, GREEN, speed * 2.0 - 1.0)
    };
    let channel = |i: usize| from[i] + (to[i] - from[i]) * t;
    [channel(0), channel(1), channel(2), 1.0]
}

pub(super) fn legend(_uiw: &UiWorld) {
    reflow(
        Alignment::TOP_LEFT,
        Pivot::TOP_LEFT,
        Dim2::pixels(12.0, 112.0),
        || {
            blur_bg(secondary_container().with_alpha(0.94), 8.0, || {
                padxy(10.0, 6.0, || {
                    mincolumn(4.0, || {
                        let color = on_secondary_container();
                        textc(color, "Speed relative to limit");
                        minrow(0.0, || {
                            for step in 0..=10 {
                                swatch(speed_color(step as f32 / 10.0), 16.0);
                            }
                        });
                        minrow(0.0, || {
                            textc(color, "Stopped");
                            fixed_spacer((64.0, 0.0));
                            textc(color, "At limit");
                        });
                        minrow(6.0, || {
                            swatch(NO_DATA, 12.0);
                            textc(color, "No recent data");
                        });
                    });
                });
            });
        },
    );
}

fn swatch([r, g, b, a]: [f32; 4], width: f32) {
    colored_box_container(Color::from_linear(Vec4::new(r, g, b, a)), || {
        fixed_spacer((width, 9.0));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::{overlays::OverlayMode, Tool};
    use crate::rendering::OrbitCamera;
    use geom::{vec2, vec3, Camera, Radians, Transform};
    use scenarios::{builders::grid_city, ScenarioCtx};
    use simulation::map::{LaneKind, LanePatternBuilder, LotKind, MapProject, RoadID};
    use simulation::transportation::{spawn_driving_vehicle, VehicleKind};
    use simulation::{SimulationOptions, VehicleID};

    fn city() -> (ScenarioCtx, RoadID, VehicleID) {
        let mut ctx = ScenarioCtx::new(SimulationOptions {
            terrain_size: 4,
            save_replay: false,
            ..SimulationOptions::LEGACY
        });
        let road = grid_city(&mut ctx, vec2(700.0, 700.0), 2, 1, 160.0).roads[0];
        let pos = {
            let map = ctx.sim.map();
            let lane = map
                .get(road)
                .unwrap()
                .lanes_iter()
                .find(|&(_, kind)| kind == LaneKind::Driving)
                .unwrap()
                .0;
            map.get(lane).unwrap().points.point_along(55.0)
        };
        let car =
            spawn_driving_vehicle(&mut ctx.sim, VehicleKind::Car, Transform::new(pos)).unwrap();
        (ctx, road, car)
    }

    fn camera(x: f32, y: f32) -> OrbitCamera {
        let mut camera = Camera::new(vec3(x, y, 0.0), 1600.0, 900.0);
        camera.pitch = Radians(1.3);
        camera.dist = 200.0;
        camera.update();
        OrbitCamera::from_camera(camera)
    }

    fn traffic_ui() -> UiWorld {
        let mut uiw = UiWorld::default();
        uiw.insert(Tool::Hand);
        uiw.insert(OverlayMode::Traffic);
        uiw.insert(ImmediateDraw::default());
        uiw.insert(camera(780.0, 700.0));
        uiw
    }

    #[test]
    fn p4e_traffic_overlay_keeps_history_on_lot_dispatch() {
        let (ctx, road, _) = city();
        let uiw = traffic_ui();
        super::super::draw(&ctx.sim, &uiw);
        let observed_at = TRAFFIC.with_borrow(|traffic| traffic.history[&road].observed_at);
        {
            let mut map = ctx.sim.write::<Map>();
            let lot = map.lots().keys().next().unwrap();
            map.set_lot_kind(lot, LotKind::Residential);
        }
        super::super::draw(&ctx.sim, &uiw);
        assert_eq!(
            TRAFFIC.with_borrow(|traffic| traffic.history.get(&road).map(|v| v.observed_at)),
            Some(observed_at)
        );

        let pattern = LanePatternBuilder::default().build();
        let ground = |pos| MapProject {
            pos,
            kind: ProjectKind::Ground,
        };
        ctx.sim
            .write::<Map>()
            .make_connection(
                ground(vec3(1600.0, 1600.0, 0.0)),
                ground(vec3(1700.0, 1600.0, 0.0)),
                None,
                &pattern,
            )
            .unwrap();
        super::super::draw(&ctx.sim, &uiw);
        assert_eq!(
            TRAFFIC.with_borrow(|traffic| traffic.history.get(&road).map(|v| v.observed_at)),
            Some(observed_at)
        );

        {
            let mut map = ctx.sim.write::<Map>();
            let src = map.get(road).unwrap().src;
            let pos = map.get(src).unwrap().pos;
            map.make_connection(
                MapProject {
                    pos,
                    kind: ProjectKind::Intersection(src),
                },
                ground(pos - vec3(0.0, 100.0, 0.0)),
                None,
                &pattern,
            )
            .unwrap();
            assert!(map.get(road).is_some());
        }
        super::super::draw(&ctx.sim, &uiw);
        assert!(TRAFFIC.with_borrow(|traffic| !traffic.history.contains_key(&road)));
    }

    #[test]
    fn p4e_traffic_overlay_includes_a_visible_elevated_road() {
        let (ctx, _, _) = city();
        let mut map = ctx.sim.write::<Map>();
        let ground = |pos| MapProject {
            pos,
            kind: ProjectKind::Ground,
        };
        let (_, bridge) = map
            .make_connection(
                ground(vec3(900.0, 850.0, 100.0)),
                ground(vec3(900.0, 950.0, 100.0)),
                None,
                &LanePatternBuilder::default().build(),
            )
            .unwrap();
        let mut camera = Camera::new(vec3(900.0, 900.0, 95.0), 1600.0, 900.0);
        camera.yaw = Radians(0.0);
        camera.pitch = Radians(0.28);
        camera.dist = 35.0;
        camera.update();

        assert!(visible_roads(&camera, &map, road_geometry_bounds(&map)).contains(&bridge));
    }

    #[test]
    fn p4e_traffic_overlay_refreshes_when_the_camera_or_topology_changes() {
        let (ctx, road, _) = city();
        super::super::reset_world_cache();
        let uiw = traffic_ui();
        let orders = || {
            super::super::draw(&ctx.sim, &uiw);
            uiw.read::<ImmediateDraw>().persistent_orders.len()
        };
        assert_eq!(orders(), 1);
        *uiw.camera_mut() = camera(1500.0, 1500.0);
        assert_eq!(orders(), 0);
        *uiw.camera_mut() = camera(780.0, 700.0);
        assert_eq!(orders(), 1);
        let last_sample = Instant::now() - SAMPLE_INTERVAL;
        TRAFFIC.with_borrow_mut(|traffic| traffic.last_sample = Some(last_sample));
        *uiw.write::<OverlayMode>() = OverlayMode::Off;
        assert_eq!(orders(), 0);
        assert_eq!(
            TRAFFIC.with_borrow(|traffic| traffic.last_sample),
            Some(last_sample)
        );
        *uiw.write::<OverlayMode>() = OverlayMode::Traffic;
        assert_eq!(orders(), 1);
        ctx.sim.write::<Map>().remove_road(road).unwrap();
        assert_eq!(orders(), 0);
    }

    #[test]
    fn p4e_traffic_overlay_sampling_preserves_hashes_after_resume() {
        let (mut sampled, road, _) = city();
        let (mut control, _, _) = city();
        let uiw = traffic_ui();
        super::super::reset_world_cache();
        let paused = sampled.sim.hashes();
        assert_eq!(paused, control.sim.hashes());
        super::super::draw(&sampled.sim, &uiw);
        assert_eq!(uiw.read::<ImmediateDraw>().persistent_orders.len(), 1);
        let first_sample = TRAFFIC.with_borrow(|traffic| traffic.history[&road].observed_at);
        assert_eq!(sampled.sim.hashes(), paused);
        for step in 1..=20 {
            sampled.run_ticks(1);
            control.run_ticks(1);
            TRAFFIC.with_borrow_mut(|traffic| {
                traffic.update(
                    &sampled.sim,
                    &sampled.sim.map(),
                    &uiw.camera().camera,
                    &mut uiw.write::<ImmediateDraw>(),
                    false,
                    first_sample + step * SAMPLE_INTERVAL,
                );
            });
        }
        assert!(TRAFFIC.with_borrow(|traffic| traffic.last_sample.unwrap() > first_sample));
        assert_eq!(sampled.sim.hashes(), control.sim.hashes());
    }

    #[test]
    fn p4e_traffic_overlay_enforces_cadence_smooths_and_expires() {
        let (mut ctx, road, car) = city();
        let camera = camera(780.0, 700.0).camera;
        let mut traffic = TrafficOverlay::default();
        let mut draw = ImmediateDraw::default();
        let now = Instant::now();
        let refresh =
            |traffic: &mut TrafficOverlay, sim: &Simulation, draw: &mut ImmediateDraw, time| {
                traffic.update(sim, &sim.map(), &camera, draw, false, time);
            };

        refresh(&mut traffic, &ctx.sim, &mut draw, now);
        assert_eq!(traffic.history[&road].rel_speed, 0.0);
        ctx.sim.world_mut_unchecked().vehicles[car].speed.0 = 50.0;
        refresh(&mut traffic, &ctx.sim, &mut draw, now + SAMPLE_INTERVAL / 2);
        assert_eq!(traffic.history[&road].rel_speed, 0.0);
        refresh(&mut traffic, &ctx.sim, &mut draw, now + SAMPLE_INTERVAL);
        assert_eq!(traffic.history[&road].rel_speed, EMA_ALPHA);

        ctx.sim.world_mut_unchecked().vehicles.remove(car).unwrap();
        refresh(&mut traffic, &ctx.sim, &mut draw, now + SAMPLE_EXPIRY);
        assert_eq!(traffic.history[&road].rel_speed, EMA_ALPHA);
        refresh(
            &mut traffic,
            &ctx.sim,
            &mut draw,
            now + SAMPLE_EXPIRY + SAMPLE_INTERVAL,
        );
        assert!(!traffic.history.contains_key(&road));
        assert_eq!(<[f32; 4]>::from(draw.persistent_orders[0].color), NO_DATA);
    }

    #[test]
    fn p4_fix_final_traffic_updates_only_a_batch_of_roads_per_frame() {
        let mut ctx = ScenarioCtx::new(SimulationOptions {
            terrain_size: 4,
            save_replay: false,
            ..SimulationOptions::LEGACY
        });
        let roads = grid_city(&mut ctx, vec2(150.0, 150.0), 12, 12, 120.0).roads;
        for &road in &roads {
            let pos = {
                let map = ctx.sim.map();
                let lane = map
                    .get(road)
                    .unwrap()
                    .lanes_iter()
                    .find(|&(_, kind)| kind == LaneKind::Driving)
                    .unwrap()
                    .0;
                map.get(lane).unwrap().points.point_along(55.0)
            };
            spawn_driving_vehicle(&mut ctx.sim, VehicleKind::Car, Transform::new(pos)).unwrap();
        }
        let mut camera = camera(810.0, 810.0).camera;
        camera.dist = 2_800.0;
        camera.update();
        let mut traffic = TrafficOverlay::default();
        let mut draw = ImmediateDraw::default();
        let now = Instant::now();
        let map = ctx.sim.map();
        traffic.update(&ctx.sim, &map, &camera, &mut draw, false, now);
        let max_roads = ROADS_PER_BATCH * MAX_BATCHES_PER_FRAME;
        assert!(traffic.visible.len() > max_roads);
        assert!((1..=max_roads).contains(&traffic.history.len()));
        let expected = simulation::transportation::traffic_metrics::sample(
            &ctx.sim,
            traffic.visible.iter().copied(),
        );

        let before = traffic.next_road.unwrap();
        let mut budget_checks = 0;
        traffic.sample_frame(&ctx.sim, &map, &mut draw, now, || {
            budget_checks += 1;
            budget_checks > 1
        });
        assert_eq!(budget_checks, 2);
        assert_eq!(traffic.next_road, Some(before + ROADS_PER_BATCH));
        assert_eq!(
            traffic.history.len(),
            traffic.visible[..before + ROADS_PER_BATCH]
                .iter()
                .filter(|id| expected.contains_key(id))
                .count()
        );
        for _ in 0..traffic.visible.len().div_ceil(ROADS_PER_BATCH) {
            traffic.update(&ctx.sim, &map, &camera, &mut draw, false, now);
        }
        assert!(traffic.history.keys().eq(expected.keys()));
        assert!(traffic.next_road.is_none());
    }

    #[test]
    #[ignore = "runs the release perf_10k city"]
    fn p4e_traffic_sampler_perf_10k() {
        let (scenario, options) = scenarios::builtin::PERF_10K;
        let mut ctx = ScenarioCtx::new(options);
        (scenario.run)(&mut ctx).unwrap();
        let uiw = traffic_ui();
        let mut camera = camera(12_000.0, 8_000.0).camera;
        camera.dist = 3_400.0;
        camera.update();
        *uiw.camera_mut() = OrbitCamera::from_camera(camera);

        let mut timings = Vec::new();
        for ticks in [0, 300, 300] {
            ctx.run_ticks(ticks);
            super::super::draw(&ctx.sim, &uiw);
            let roads = TRAFFIC.with_borrow(|traffic| traffic.visible.clone());
            let observed = simulation::transportation::traffic_metrics::sample(
                &ctx.sim,
                roads.iter().copied(),
            );
            assert!(!observed.is_empty());
            let vehicles: u32 = observed.values().map(|road| road.samples).sum();
            println!(
                "tick {}, residents {}, visible roads {}, sampled vehicles {}",
                ctx.sim.get_tick(),
                ctx.sim.world().humans.len(),
                roads.len(),
                vehicles,
            );
            let map = ctx.sim.map();
            let mut draw = uiw.write::<ImmediateDraw>();
            let now = Instant::now();
            TRAFFIC.with_borrow_mut(|traffic| {
                traffic.history.clear();
                traffic.samplers.clear();
                traffic.next_road = Some(0);
                traffic.last_sample = Some(now);
            });
            for _ in 0..128 {
                TRAFFIC.with_borrow_mut(|traffic| {
                    if traffic.next_road.is_none() {
                        assert_eq!(observed.len(), traffic.history.len());
                        assert!(observed.iter().all(|(id, speed)| {
                            (traffic.history[id].rel_speed - speed.rel_speed).abs() < f32::EPSILON
                        }));
                        traffic.next_road = Some(0);
                    }
                    let start = Instant::now();
                    traffic.update(&ctx.sim, &map, &camera, &mut draw, false, now);
                    timings.push(start.elapsed().as_secs_f64() * 1000.0);
                });
            }
        }
        timings.sort_unstable_by(f64::total_cmp);
        let peak = *timings.last().unwrap();
        println!(
            "{} sampling frames, median {:.3} ms, p95 {:.3} ms, p99 {:.3} ms, peak {:.3} ms",
            timings.len(),
            timings[timings.len() / 2],
            timings[(timings.len() * 95).div_ceil(100) - 1],
            timings[(timings.len() * 99).div_ceil(100) - 1],
            peak,
        );
        assert!(peak <= 2.0, "sampler exceeded the 2 ms wall-clock budget");
    }
}
