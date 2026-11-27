//! Stuck-vehicle probe: load a save slot, run it headlessly and classify vehicles
//! that stand still on the road for several game minutes.
//!
//! cargo run --release -p scenarios --example stuck_vehicles -- <save-root> <slot> [hours] [minutes]
//!
//! A vehicle is stuck when it has a collider, is not parked and has not moved for `minutes`.
//! Each stuck vehicle is attributed to the head of the queue in front of it.
//! Optional environment variables for investigation:
//! - `PROBE_DIAG=1` explains routes of vehicles waiting for a reroute;
//! - `PROBE_AREA=x,y,radius` prints junctions, roads and lanes around a point at load;
//! - `PROBE_SNAP=x,y,radius,minute` prints the vehicles around a point at that minute;
//! - `PROBE_WATCH=VehicleID(…)` follows one vehicle and its driver every sample.

use geom::Vec3;
use prototypes::{game_secs_to_ticks, GameTime, SECONDS_PER_MINUTE, TICKS_PER_HOUR};
use simulation::diagnostics::elapsed_tick;
use simulation::economy::TripStats;
use simulation::map::{
    LaneKind, Map, PathKind, Pathfinder, ProjectFilter, ProjectKind, TrafficBehavior, Traversable,
    TraverseDirection, TraverseKind, TurnID,
};
use simulation::map_dynamic::ItineraryTarget;
use simulation::map_dynamic::{Itinerary, RoutingStep};
use simulation::transportation::road::calc_decision;
use simulation::transportation::{Location, TransportGrid, Vehicle, VehicleState};
use simulation::utils::scheduler::SeqSchedule;
use simulation::VehicleID;
use simulation::{Simulation, SlotId};
use std::collections::{BTreeMap, BTreeSet};

const SAMPLE_SECONDS: u64 = 30;
const SAMPLE_TICKS: u64 = game_secs_to_ticks(SAMPLE_SECONDS);

#[derive(Clone)]
struct Still {
    since: u32,
    pos: Vec3,
}

#[derive(Clone, Copy)]
struct JourneyCompletions {
    walk: u64,
    driving: u64,
    transit: u64,
}

impl From<&TripStats> for JourneyCompletions {
    fn from(stats: &TripStats) -> Self {
        Self {
            walk: stats.walk.completed,
            driving: stats.car.completed,
            transit: stats.transit.completed,
        }
    }
}

impl JourneyCompletions {
    fn report_since(self, start: Self) -> String {
        let walk = self.walk.saturating_sub(start.walk);
        let driving = self.driving.saturating_sub(start.driving);
        let transit = self.transit.saturating_sub(start.transit);
        let total = walk + driving + transit;
        format!(
            "driving-mode journeys completed (includes human-driven freight): {driving}\nall tracked journeys completed: {total} (walk={walk} driving={driving} transit={transit})"
        )
    }
}

#[derive(Default)]
struct StationaryDelay {
    peak_vehicles: usize,
    longest_seconds: u32,
}

impl StationaryDelay {
    fn sample(
        &mut self,
        still: &BTreeMap<VehicleID, Still>,
        now: u32,
        threshold: u32,
    ) -> BTreeSet<VehicleID> {
        let stuck: BTreeSet<_> = still
            .iter()
            .filter_map(|(&id, s)| {
                let seconds = now - s.since;
                self.longest_seconds = self.longest_seconds.max(seconds);
                (seconds >= threshold).then_some(id)
            })
            .collect();
        self.peak_vehicles = self.peak_vehicles.max(stuck.len());
        stuck
    }
}

#[derive(Default)]
struct SpillbackProbe {
    active: BTreeMap<TurnID, (u64, bool)>,
    longest: u64,
}

impl SpillbackProbe {
    fn sample(&mut self, sim: &Simulation, tick: u64) {
        let map = sim.map();
        let seconds = sim.read::<GameTime>().seconds;
        let mut occupied_starts = BTreeMap::new();
        let mut upstream = Vec::new();
        for (id, car) in sim.world().vehicles.iter() {
            if car.collider.is_none() || car.vehicle.state.is_parked() || car.speed.0.abs() > 0.1 {
                continue;
            }
            let Some(path) = car.it.get_travers().map(|traverse| traverse.kind) else {
                continue;
            };
            match path {
                TraverseKind::Turn(turn) => upstream.push((turn, id)),
                TraverseKind::Lane(lane) => {
                    let Some(lane_data) = map.get(lane) else {
                        continue;
                    };
                    let points = &lane_data.points;
                    let radius = car.vehicle.kind.width() * 0.5;
                    if points.first().is_close(car.trans.pos, 24.0 + radius) {
                        occupied_starts.entry(lane).or_insert(id);
                    }
                    if points.last().is_close(car.trans.pos, 5.0 + radius) {
                        let next = car
                            .it
                            .get_route()
                            .and_then(|route| route.reversed_route.last());
                        if let Some(TraverseKind::Turn(turn)) = next.map(|traverse| traverse.kind) {
                            upstream.push((turn, id));
                        }
                    }
                }
            }
        }
        let mut next_active = BTreeMap::new();
        for (turn, vehicle) in upstream {
            let Some(&blocker) = occupied_starts.get(&turn.dst).filter(|&&id| id != vehicle) else {
                continue;
            };
            let Some(source) = map.get(turn.src) else {
                continue;
            };
            let permissive = matches!(
                source.control.get_behavior(seconds),
                TrafficBehavior::GREEN | TrafficBehavior::STOP
            );
            let (start, permissive_seen) = next_active
                .entry(turn)
                .or_insert_with(|| self.active.get(&turn).copied().unwrap_or((tick, false)));
            *permissive_seen |= permissive;
            if *permissive_seen && tick - *start > self.longest {
                self.longest = tick - *start;
                if self.longest == SAMPLE_TICKS || self.longest % (SAMPLE_TICKS * 10) == 0 {
                    println!(
                        "TRACE_SPILLBACK_CANDIDATE tick={tick} start={start} turn={turn:?} upstream={vehicle:?} egress_blocker={blocker:?}"
                    );
                }
            }
        }
        self.active = next_active;
    }
}

fn braking_evidence(sim: &Simulation, id: VehicleID) -> Option<(f32, f32, Vec<String>)> {
    let car = &sim.world().vehicles[id];
    let grid = sim.read::<TransportGrid>();
    let (_, own) = grid.get(car.collider.as_ref()?.0)?;
    let map = sim.map();
    let time = sim.read::<GameTime>();
    let radius = 12.0 + (own.speed.powi(2) / (2.0 * car.vehicle.kind.deceleration())).min(100.0);
    let neighbours: Vec<_> = grid
        .query_around(car.trans.pos.xy(), radius)
        .filter_map(|(handle, pos)| Some((handle, pos, grid.get(handle)?.1)))
        .collect();
    let decide = |neighbours: &[_]| {
        // Evaluate a copy of the ordinary braking decision at this tick.
        let mut vehicle = Vehicle {
            state: VehicleState::Driving,
            wait_time: 0.0,
            ang_velocity: car.vehicle.ang_velocity,
            kind: car.vehicle.kind,
            tint: car.vehicle.tint,
            max_speed_multiplier: car.vehicle.max_speed_multiplier,
            flag: car.vehicle.flag,
        };
        calc_decision(
            id,
            &mut vehicle,
            &map,
            &time,
            &car.trans,
            own,
            &car.it,
            neighbours.iter().map(|&(_, pos, state)| (pos, state)),
        )
        .0
    };
    let with = decide(&neighbours);
    let without = decide(&[]);
    let colliders = if with == 0.0 && without > 0.0 {
        neighbours
            .iter()
            .filter(|neighbour| decide(std::slice::from_ref(*neighbour)) == 0.0)
            .map(|&(handle, pos, state)| {
                let owner = grid.collider_owner(sim.world(), handle);
                format!("{owner:?}/{:?}@{pos:?}/z={:.2}", state.group, state.height)
            })
            .collect()
    } else {
        Vec::new()
    };
    Some((with, without, colliders))
}

fn cycle_evidence(
    sim: &Simulation,
    start: VehicleID,
    blockers: &BTreeMap<VehicleID, VehicleID>,
) -> Vec<String> {
    let mut visited = BTreeSet::new();
    let mut id = start;
    let mut edges = Vec::new();
    while visited.insert(id) {
        let next = blockers[&id];
        let vehicle = &sim.world().vehicles[id];
        edges.push(format!(
            "{id:?}->{next:?} pos={:?} speed={:.2} flag={} movement={:?}",
            vehicle.trans.pos,
            vehicle.speed.0,
            vehicle.vehicle.flag,
            vehicle.it.get_travers().map(|path| path.kind)
        ));
        id = next;
    }
    edges
}

fn advance_sample(sim: &mut Simulation, sched: &mut SeqSchedule, remaining_ticks: u64) -> u64 {
    let batch = remaining_ticks.min(SAMPLE_TICKS);
    for _ in 0..batch {
        sim.tick(sched, &[]);
    }
    batch
}

fn main() {
    let mut args = std::env::args().skip(1);
    let root = std::path::absolute(args.next().expect("save root")).unwrap();
    let slot = SlotId::new(&args.next().expect("slot id")).unwrap();
    let hours: f64 = args.next().map_or(6.0, |h| h.parse().unwrap());
    let threshold_minutes: u32 = args.next().map_or(5, |m| m.parse().unwrap());
    scenarios::runner::ensure_repo_root_cwd();
    simulation::init::init();

    let mut sim = Simulation::load_from_slot(&root, &slot).expect("load slot");
    if let Ok(area) = std::env::var("PROBE_AREA") {
        let v: Vec<f32> = area.split(',').map(|x| x.parse().unwrap()).collect();
        dump_area(&sim, geom::vec2(v[0], v[1]), v[2]);
    }
    let mut sched = Simulation::schedule();
    let ticks = (hours * TICKS_PER_HOUR as f64) as u64;
    let threshold = threshold_minutes * 60;

    let mut still: BTreeMap<VehicleID, Still> = BTreeMap::new();
    let mut stationary_delay = StationaryDelay::default();
    let mut spillback = SpillbackProbe::default();
    let mut peak_zero = 0;
    // Vehicles that crossed the threshold, with the class seen at that moment.
    let mut episodes: BTreeMap<VehicleID, (String, u32)> = BTreeMap::new();
    let mut episode_classes: BTreeMap<String, usize> = BTreeMap::new();
    let mut examples: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut max_queue: BTreeMap<String, usize> = BTreeMap::new();
    let start = sim.read::<GameTime>().seconds;
    let start_completions = JourneyCompletions::from(&*sim.read::<TripStats>());
    println!(
        "TRACE_START version=1 elapsed_tick={} calendar_tick={} sample_ticks={SAMPLE_TICKS} planned_ticks={ticks} workers=1",
        elapsed_tick(&sim).0, sim.get_tick()
    );
    println!(
        "TRACE_WORKLOAD humans={} vehicles={} companies={} trains={} roads={}",
        sim.world().humans.len(),
        sim.world().vehicles.len(),
        sim.world().companies.len(),
        sim.world().trains.len(),
        sim.map().roads().len()
    );
    println!("TRACE_STOCK_TRAFFIC enabled={}", sim.map().uses_stock_traffic());

    let mut done = 0;
    while done < ticks {
        done += advance_sample(&mut sim, &mut sched, ticks - done);
        let now = sim.read::<GameTime>().seconds;
        let tick = elapsed_tick(&sim).0;
        spillback.sample(&sim, tick);
        if let Ok(snap) = std::env::var("PROBE_SNAP") {
            let v: Vec<f32> = snap.split(',').map(|x| x.parse().unwrap()).collect();
            if (now - start) / 60 == v[3] as u32 && (now - start) % 60 < SAMPLE_SECONDS as u32 {
                snapshot(&sim, geom::vec2(v[0], v[1]), v[2]);
            }
        }
        if let Ok(watch) = std::env::var("PROBE_WATCH") {
            watch_vehicle(&sim, &watch, (now - start) / 60);
        }
        let world = sim.world();
        let zero = world
            .vehicles
            .values()
            .filter(|v| v.collider.is_some() && !v.vehicle.state.is_parked() && v.speed.0 == 0.0)
            .count();
        peak_zero = peak_zero.max(zero);
        still.retain(|id, _| world.vehicles.contains_key(*id));
        for (id, v) in world.vehicles.iter() {
            let moving = v.collider.is_none()
                || v.vehicle.state.is_parked()
                || v.speed.0.abs() > 0.1
                || still
                    .get(&id)
                    .is_some_and(|s| s.pos.distance(v.trans.pos) > 0.5);
            if moving {
                still.remove(&id);
                episodes.remove(&id);
                continue;
            }
            still.entry(id).or_insert(Still {
                since: now,
                pos: v.trans.pos,
            });
        }

        let stuck = stationary_delay.sample(&still, now, threshold);
        if done % (SAMPLE_TICKS * 60) == 0 {
            let mut cells: BTreeMap<(i32, i32), usize> = BTreeMap::new();
            for id in &stuck {
                let p = sim.world().vehicles[*id].trans.pos;
                *cells
                    .entry(((p.x / 200.0) as i32 * 200, (p.y / 200.0) as i32 * 200))
                    .or_default() += 1;
            }
            let mut cells: Vec<_> = cells.into_iter().collect();
            cells.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
            cells.truncate(4);
            println!(
                "t+{}min elapsed_tick={tick} stuck={} exact_zero={zero} colliders={} cells={cells:?}",
                (now - start) / 60,
                stuck.len(),
                sim.world()
                    .vehicles
                    .values()
                    .filter(|v| v.collider.is_some())
                    .count()
            );
        }
        if stuck.is_empty() {
            continue;
        }
        let map = sim.map();
        let blockers = blockers(&sim, &still);
        for &id in &stuck {
            if episodes.contains_key(&id) {
                continue;
            }
            let (class, detail) = classify(&sim, &map, id, &blockers);
            let queue = queue_behind(id, &blockers);
            let entry = max_queue.entry(class.clone()).or_default();
            *entry = (*entry).max(queue);
            *episode_classes.entry(class.clone()).or_default() += 1;
            let list = examples.entry(class.clone()).or_default();
            if list.len() < 4 {
                list.push(format!(
                    "t+{}min elapsed_tick={tick} {id:?} queue={queue} {detail} braking={:?}",
                    (now - start) / 60,
                    braking_evidence(&sim, id)
                ));
            }
            if class.contains("wait_reroute") && std::env::var("PROBE_DIAG").is_ok() {
                diagnose(&sim, &map, id);
            }
            episodes.insert(id, (class, now));
        }
    }

    let now = sim.read::<GameTime>().seconds;
    let map = sim.map();
    let blockers = blockers(&sim, &still);
    let mut final_classes: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for (&id, s) in &still {
        if now - s.since < threshold {
            continue;
        }
        let (class, _) = classify(&sim, &map, id, &blockers);
        let e = final_classes.entry(class).or_default();
        e.0 += 1;
        e.1 = e.1.max(queue_behind(id, &blockers));
    }
    println!(
        "probe {} {} hours={hours} threshold={threshold_minutes}min vehicles={}",
        root.display(),
        slot.as_str(),
        sim.world().vehicles.len()
    );
    let completions = JourneyCompletions::from(&*sim.read::<TripStats>());
    println!("{}", completions.report_since(start_completions));
    let final_stuck: usize = final_classes.values().map(|(count, _)| count).sum();
    println!(
        "TRACE_END elapsed_tick={} calendar_tick={} peak_exact_zero={peak_zero} sampled_spillback_candidate_ticks={}",
        elapsed_tick(&sim).0, sim.get_tick(), spillback.longest
    );
    let longest_minutes =
        f64::from(stationary_delay.longest_seconds) / f64::from(SECONDS_PER_MINUTE);
    println!(
        "stationary delay ({SAMPLE_SECONDS} game sec samples): longest={longest_minutes:.1} game min"
    );
    println!(
        "vehicles stationary for at least {threshold_minutes} game min: peak={} end={final_stuck}",
        stationary_delay.peak_vehicles
    );
    println!("episodes (vehicle still >= threshold, class when first detected):");
    for (class, n) in &episode_classes {
        println!("  {class}: {n} (max queue {})", max_queue[class]);
        for ex in &examples[class] {
            println!("      {ex}");
        }
    }
    let stranded = sim
        .world()
        .humans
        .values()
        .filter(|h| match h.location {
            Location::Vehicle(car) => {
                matches!(h.router.cur_step(), Some(RoutingStep::DriveTo(..)))
                    && sim
                        .world()
                        .vehicles
                        .get(car)
                        .is_some_and(|v| v.vehicle.state.is_parked())
            }
            _ => false,
        })
        .count();
    println!("humans stranded in a parked car with DriveTo at end: {stranded}");
    println!("still stuck at end:");
    for (class, (n, q)) in &final_classes {
        println!("  {class}: {n} (max queue {q})");
    }
}

/// For each still vehicle, the closest vehicle collider ahead of it within 10 m.
fn blockers(
    sim: &Simulation,
    still: &BTreeMap<VehicleID, Still>,
) -> BTreeMap<VehicleID, VehicleID> {
    let world = sim.world();
    let mut out = BTreeMap::new();
    for &id in still.keys() {
        let me = &world.vehicles[id];
        let mut best: Option<(f32, VehicleID)> = None;
        for (other, v) in world.vehicles.iter() {
            if other == id || v.collider.is_none() || (v.trans.pos.z - me.trans.pos.z).abs() > 5.0 {
                continue;
            }
            let towards = v.trans.pos.xy() - me.trans.pos.xy();
            let dist = towards.mag();
            if dist > 10.0 || dist < 0.01 {
                continue;
            }
            if towards.normalize().dot(me.trans.dir.xy()) < 0.8 {
                continue;
            }
            if best.is_none_or(|(d, _)| dist < d) {
                best = Some((dist, other));
            }
        }
        if let Some((_, other)) = best {
            out.insert(id, other);
        }
    }
    out
}

fn queue_behind(head: VehicleID, blockers: &BTreeMap<VehicleID, VehicleID>) -> usize {
    blockers
        .keys()
        .filter(|&&id| {
            let mut cur = id;
            for _ in 0..200 {
                match blockers.get(&cur) {
                    Some(&next) if next == head => return true,
                    Some(&next) => cur = next,
                    None => return false,
                }
            }
            false
        })
        .count()
}

fn itinerary(it: &Itinerary) -> &'static str {
    if it.is_wait_for_reroute().is_some() {
        "wait_reroute"
    } else if it.is_none_or_wait() {
        "none"
    } else if it.has_ended(0.0) {
        "ended"
    } else if it.get_terminal().is_some() {
        "terminal"
    } else {
        "route"
    }
}

fn classify(
    sim: &Simulation,
    map: &Map,
    id: VehicleID,
    blockers: &BTreeMap<VehicleID, VehicleID>,
) -> (String, String) {
    let world = sim.world();
    let v = &world.vehicles[id];
    let pos = v.trans.pos;
    let state = match v.vehicle.state {
        VehicleState::Driving => "driving",
        VehicleState::Panicking(_) => "panicking",
        VehicleState::RoadToPark(..) => "road_to_park",
        VehicleState::ReturningToDepot { .. } => "returning_to_depot",
        VehicleState::Parked(_) | VehicleState::ParkedAtDepot => "parked",
    };
    let driver = world
        .humans
        .iter()
        .find(|(_, h)| h.location == Location::Vehicle(id));
    let step = driver
        .and_then(|(_, h)| h.router.cur_step())
        .map(|step| match step {
            RoutingStep::DriveTo(..) => "DriveTo",
            RoutingStep::DriveProperty(..) => "DriveProperty",
            RoutingStep::ParkProperty(..) => "ParkProperty",
            RoutingStep::Park(..) => "Park",
            RoutingStep::Unpark(..) => "Unpark",
            RoutingStep::GetInVehicle(..) => "GetInVehicle",
            RoutingStep::GetOutVehicle(..) => "GetOutVehicle",
            RoutingStep::WalkTo(..) | RoutingStep::WalkProperty(..) => "WalkTo",
            RoutingStep::GetInBuilding(..) => "GetInBuilding",
            RoutingStep::GetOutBuilding(..) => "GetOutBuilding",
            RoutingStep::WaitAtStop { .. } => "WaitAtStop",
            RoutingStep::RideTransit { .. } => "RideTransit",
        })
        .unwrap_or(if driver.is_some() {
            "no_step"
        } else {
            "no_driver"
        });
    let it = itinerary(&v.it);
    let driving = lane_dist(map, pos, LaneKind::Driving);
    let parking = lane_dist(map, pos, LaneKind::Parking);
    let at = place(map, pos);
    let mut head_id = id;
    let head = match blockers.get(&id) {
        Some(other) if world.vehicles[*other].collider.is_some() => {
            let mut cur = *other;
            let mut seen = BTreeSet::from([id]);
            while let Some(&next) = blockers.get(&cur) {
                if !seen.insert(cur) {
                    break;
                }
                cur = next;
            }
            head_id = cur;
            if seen.contains(&cur) && blockers.contains_key(&cur) {
                "cycle"
            } else {
                "queued"
            }
        }
        _ => "head",
    };
    if head == "queued" && !blockers.contains_key(&head_id) {
        let h = &world.vehicles[head_id];
        let hstate = match h.vehicle.state {
            VehicleState::Driving => "driving",
            VehicleState::Panicking(_) => "panicking",
            VehicleState::RoadToPark(..) => "road_to_park",
            VehicleState::ReturningToDepot { .. } => "returning_to_depot",
            VehicleState::Parked(_) | VehicleState::ParkedAtDepot => "parked",
        };
        let moving = if h.speed.0.abs() > 0.1 {
            "moving"
        } else {
            "stopped"
        };
        let class = format!(
            "queued behind {:?} {hstate} {moving} it={} at={}",
            h.vehicle.kind,
            itinerary(&h.it),
            place(map, h.trans.pos)
        );
        let detail = format!(
            "head={head_id:?} head_pos=({:.1},{:.1}) head_travers={:?} head_point={:?} head_dir=({:.2},{:.2})",
            h.trans.pos.x,
            h.trans.pos.y,
            h.it.get_travers().map(|t| t.kind),
            h.it.get_point(),
            h.trans.dir.x,
            h.trans.dir.y
        );
        return (class, detail);
    }
    let class = if head == "head" {
        format!(
            "head {:?} {state} it={it} step={step} at={at}",
            v.vehicle.kind
        )
    } else {
        format!("{head} {:?} at={at}", v.vehicle.kind)
    };
    let mut detail = format!(
        "pos=({:.1},{:.1},{:.1}) dir=({:.2},{:.2}) drive_d={driving:?} park_d={parking:?} it_point={:?} travers={:?} err={:?}",
        pos.x,
        pos.y,
        pos.z,
        v.trans.dir.x,
        v.trans.dir.y,
        v.it.get_point(),
        v.it.get_travers().map(|t| t.kind),
        driver.and_then(|(_, h)| h.router.last_error),
    );
    if head == "cycle" {
        detail.push_str(&format!(
            " inferred_cycle={:?}",
            cycle_evidence(sim, head_id, blockers)
        ));
    }
    (class, detail)
}

fn lane_dist(map: &Map, pos: Vec3, kind: LaneKind) -> Option<f32> {
    map.nearest_lane(pos, kind, Some(20.0))
        .and_then(|l| map.get(l))
        .map(|l| l.points.project(pos).distance(pos))
}

fn place(map: &Map, pos: Vec3) -> &'static str {
    let driving = lane_dist(map, pos, LaneKind::Driving);
    let parking = lane_dist(map, pos, LaneKind::Parking);
    if matches!(
        map.project(pos, 0.0, ProjectFilter::INTER).kind,
        ProjectKind::Intersection(_)
    ) {
        "intersection"
    } else if driving.is_some_and(|d| d < 1.0) {
        "on_driving_lane"
    } else if parking.is_some_and(|d| d < 1.0) {
        "on_parking_lane"
    } else if driving.is_some_and(|d| d < 6.0) {
        "beside_driving_lane"
    } else {
        "off_road"
    }
}

fn diagnose(sim: &Simulation, map: &Map, id: VehicleID) {
    let world = sim.world();
    let v = &world.vehicles[id];
    let pos = v.trans.pos;
    let target = v.it.target();
    let driver = world
        .humans
        .iter()
        .find(|(_, h)| h.location == Location::Vehicle(id));
    let end = match target {
        Some(ItineraryTarget::Position(p)) => Some(p),
        Some(ItineraryTarget::DirectedLane(l, o)) => map.get(l).map(|l| l.points.point_along(o)),
        None => None,
    };
    let start_lane = PathKind::Vehicle.nearest_lane(map, pos);
    let end_lane = end.and_then(|e| PathKind::Vehicle.nearest_lane(map, e));
    let tick = sim.read::<GameTime>().tick;
    let path = start_lane.zip(end_lane).map(|(s, e)| {
        PathKind::Vehicle
            .path(
                map,
                tick,
                Traversable::new(TraverseKind::Lane(s), TraverseDirection::Forward),
                e,
            )
            .map(|p| p.len())
    });
    let route = end.map(|e| Itinerary::route(tick, pos, e, map, PathKind::Vehicle).is_some());
    let lane_info = |l: Option<simulation::map::LaneID>| {
        l.and_then(|l| map.get(l)).map(|lane| {
            let road = map.get(lane.parent);
            format!(
                "{:?} road={:?} len={:.1} src={:?}({} roads) dst={:?}({} roads) proj_dist_along={:.1}",
                lane.id,
                lane.parent,
                lane.points.length(),
                lane.src,
                map.get(lane.src).map_or(0, |i| i.roads.len()),
                lane.dst,
                map.get(lane.dst).map_or(0, |i| i.roads.len()),
                road.map_or(0.0, |_| lane.points.length_at_proj(lane.points.project(pos)))
            )
        })
    };
    println!(
        "   DIAG {id:?} pos={pos:?} end={end:?} dist={:?} start_lane={:?} end_lane={:?} same={} path={path:?} route={route:?} decision={:?} target={:?}",
        end.map(|e| e.distance(pos)),
        lane_info(start_lane),
        lane_info(end_lane),
        start_lane == end_lane,
        driver.map(|(_, h)| &h.decision.kind),
        driver.map(|(_, h)| h.router.target_dest),
    );
}

fn dump_area(sim: &Simulation, center: geom::Vec2, radius: f32) {
    let map = sim.map();
    for (id, i) in map.intersections().iter() {
        if i.pos.xy().distance(center) > radius {
            continue;
        }
        println!(
            "INTER {id:?} pos=({:.1},{:.1}) roads={:?} light={:?} turns={}",
            i.pos.x,
            i.pos.y,
            i.roads,
            i.light_policy,
            i.turns().len()
        );
        for t in i.turns() {
            println!("    turn {:?}->{:?} kind={:?}", t.id.src, t.id.dst, t.kind);
        }
    }
    for (id, r) in map.roads().iter() {
        if r.points.project(center.z(0.0)).xy().distance(center) > radius {
            continue;
        }
        println!(
            "ROAD {id:?} {:?}->{:?} len={:.1} width={:.1}",
            r.src,
            r.dst,
            r.length(),
            r.width
        );
        for (l, kind) in r.lanes_iter() {
            let lane = &map.lanes()[l];
            println!(
                "    lane {l:?} {kind:?} {:?}->{:?} len={:.1} speed={:.1} control={:?}",
                lane.src,
                lane.dst,
                lane.points.length(),
                lane.speed_limit,
                lane.control
            );
        }
    }
}

fn watch_vehicle(sim: &Simulation, watch: &str, minute: u32) {
    let world = sim.world();
    for (id, v) in world.vehicles.iter() {
        if format!("{id:?}") != watch {
            continue;
        }
        let driver = world
            .humans
            .iter()
            .find(|(_, h)| h.location == Location::Vehicle(id));
        let owner = world
            .humans
            .iter()
            .find(|(_, h)| h.router.personal_car == Some(id));
        println!(
            "WATCH t+{minute} {id:?} state={:?} speed={:.2} pos=({:.1},{:.1}) collider={} it={} driver={:?} owner={:?}",
            std::mem::discriminant(&v.vehicle.state),
            v.speed.0,
            v.trans.pos.x,
            v.trans.pos.y,
            v.collider.is_some(),
            itinerary(&v.it),
            driver.map(|(hid, h)| (hid, h.router.cur_step().map(|s| format!("{s:?}")), &h.decision.kind)),
            owner.map(|(hid, h)| (hid, &h.location, h.router.cur_step().map(|s| format!("{s:?}")), h.router.target_dest, &h.decision.kind)),
        );
    }
}

fn snapshot(sim: &Simulation, center: geom::Vec2, radius: f32) {
    let world = sim.world();
    let map = sim.map();
    for (id, v) in world.vehicles.iter() {
        if v.collider.is_none() || v.trans.pos.xy().distance(center) > radius {
            continue;
        }
        let route = v.it.get_route();
        println!(
            "SNAP {id:?} pos=({:.1},{:.1}) dir=({:.2},{:.2}) speed={:.2} state={:?} flag={} wait={:.2} travers={:?} next={:?} point={:?} lane_ctrl={:?}",
            v.trans.pos.x, v.trans.pos.y, v.trans.dir.x, v.trans.dir.y, v.speed.0,
            std::mem::discriminant(&v.vehicle.state), v.vehicle.flag, v.vehicle.wait_time,
            v.it.get_travers().map(|t| t.kind),
            route.and_then(|r| r.reversed_route.last()).map(|t| t.kind),
            v.it.get_point().map(|p| (p.x, p.y)),
            v.it.get_travers().and_then(|t| match t.kind { TraverseKind::Lane(l) => map.get(l).map(|l| (l.control, l.points.length())), _ => None }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qw1_probe_samples_the_partial_batch_at_the_cutoff() {
        let mut sim = scenarios::ScenarioCtx::new(simulation::SimulationOptions {
            terrain_size: 1,
            save_replay: false,
            ..simulation::SimulationOptions::LEGACY
        })
        .sim;
        let mut sched = Simulation::schedule();
        let start = sim.get_tick();
        let ticks = SAMPLE_TICKS + 7;
        let first = advance_sample(&mut sim, &mut sched, ticks);
        assert_eq!(first, SAMPLE_TICKS);
        assert_eq!(sim.get_tick(), start + first);
        let last = advance_sample(&mut sim, &mut sched, ticks - first);
        assert_eq!(sim.get_tick() - start, ticks);
        assert_eq!(first + last, ticks);
    }

    #[test]
    fn qw1_completion_report_counts_only_arrivals_during_the_probe() {
        let mut stats = TripStats::default();
        stats.walk.completed = 10;
        stats.car.completed = 20;
        stats.transit.completed = 30;
        let start = JourneyCompletions::from(&stats);
        assert_eq!(
            JourneyCompletions::from(&stats).report_since(start),
            "driving-mode journeys completed (includes human-driven freight): 0\nall tracked journeys completed: 0 (walk=0 driving=0 transit=0)"
        );

        stats.walk.completed += 5;
        stats.car.completed += 2;
        stats.transit.completed += 3;
        assert_eq!(
            JourneyCompletions::from(&stats).report_since(start),
            "driving-mode journeys completed (includes human-driven freight): 2\nall tracked journeys completed: 10 (walk=5 driving=2 transit=3)"
        );
    }

    #[test]
    fn qw1_stationary_delay_retains_peaks_after_traffic_moves() {
        let id = VehicleID::default();
        let mut still = BTreeMap::from([(
            id,
            Still {
                since: 30,
                pos: Vec3::ZERO,
            },
        )]);
        let mut delay = StationaryDelay::default();
        assert!(delay.sample(&still, 60, 60).is_empty());
        assert_eq!(delay.longest_seconds, 30);
        assert_eq!(delay.sample(&still, 90, 60), BTreeSet::from([id]));

        still.clear();
        assert!(delay.sample(&still, 120, 60).is_empty());
        assert_eq!(delay.peak_vehicles, 1);
        assert_eq!(delay.longest_seconds, 60);
    }
}
