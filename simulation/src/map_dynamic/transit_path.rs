use std::collections::BTreeMap;

use flat_spatial::Grid;
use geom::{Vec2, Vec3};
use ordered_float::OrderedFloat;
use prototypes::{Money, Tick, TICKS_PER_HOUR, TICKS_PER_REALTIME_SECOND};

use super::{LineID, RoutingStep, StopID, Transit};
use crate::map::{Map, PathKind, Pathfinder, TraverseDirection};
use crate::map_dynamic::transit::{TransitLine, TransitStop, GIVE_UP_TICKS, WAIT_NOT_STARTED};
use crate::map_dynamic::{BusRun, Itinerary};
use crate::transportation::Location;

const NEAREST_STOPS: usize = 3;
const STOP_ACCESS_RADIUS: f32 = 600.0;
const CAR_SPEED: f64 = 10.0;
const BUS_SPEED: f64 = 8.0;
const PARKING_SECONDS: f64 = 120.0;
const VALUE_OF_TIME_PER_GAME_HOUR: Money = Money::new_bucks(20);
// Ties go to the legacy mode. Leave room for rounding in fractional tick costs.
const COST_EPSILON: f64 = 0.01;

type StopWalks<'a> = Vec<(f64, &'a TransitStop, ValidatedWalk)>;

pub(super) struct TransitPaths<'a> {
    lines: BTreeMap<LineID, ServiceLine<'a>>,
    stop_lines: BTreeMap<StopID, Vec<(LineID, usize)>>,
    stops: Grid<&'a TransitStop, Vec2>,
}

impl<'a> TransitPaths<'a> {
    pub(super) fn new(
        transit: &'a Transit,
        now: Tick,
        headway: impl Fn(&Transit, LineID) -> Option<u64>,
    ) -> Self {
        let mut paths = Self {
            lines: BTreeMap::new(),
            stop_lines: BTreeMap::new(),
            stops: Grid::new(STOP_ACCESS_RADIUS as i32),
        };
        let mut runs = BTreeMap::<_, Vec<_>>::new();
        for (&bus, run) in &transit.runs {
            runs.entry(run.line).or_default().push((bus, run));
        }
        for (id, line) in &transit.lines {
            if line.suspended {
                continue;
            }
            let Some(headway) = headway(transit, id) else {
                continue;
            };
            let Some(stops) = line
                .stops
                .iter()
                .map(|&id| transit.stops.get(id))
                .collect::<Option<Vec<_>>>()
            else {
                continue;
            };
            let (leg_ticks, dwell_ticks, departures) = match transit.directed_leg_times(id) {
                Some((times, dwell)) => {
                    let active_runs = runs
                        .get(&id)
                        .into_iter()
                        .flatten()
                        .take(usize::from(line.target_vehicles))
                        .map(|&(bus, run)| (run, transit.recovery_leg_ticks(bus, run)));
                    let Some(departures) = Departures::new(now, times, dwell, active_runs) else {
                        continue;
                    };
                    (
                        times.iter().map(|&ticks| ticks as f64).collect(),
                        dwell,
                        Some(departures),
                    )
                }
                None => (
                    stops
                        .iter()
                        .zip(stops.iter().cycle().skip(1))
                        .map(|(from, to)| travel_ticks(from.berth.distance(to.berth), BUS_SPEED))
                        .collect(),
                    0,
                    None,
                ),
            };
            let fare_ticks = line.fare.inner() as f64 / VALUE_OF_TIME_PER_GAME_HOUR.inner() as f64
                * TICKS_PER_HOUR as f64;
            paths.lines.insert(
                id,
                ServiceLine {
                    line,
                    leg_ticks,
                    dwell_ticks: dwell_ticks as f64,
                    departures,
                    half_headway: headway as f64 * 0.5,
                    fare_ticks,
                },
            );
            for (index, stop) in stops.into_iter().enumerate() {
                let lines = paths.stop_lines.entry(stop.id).or_default();
                if lines.is_empty() {
                    paths.stops.insert(stop.walk_pos.xy(), stop);
                }
                lines.push((id, index));
            }
        }
        paths
    }

    pub(super) fn has_service(&self) -> bool {
        !self.lines.is_empty()
    }

    pub(super) fn winning_steps(
        &self,
        origin: Vec3,
        destination: Vec3,
        location: &Location,
        map: &Map,
        now: Tick,
        car_pos: Option<Vec3>,
        walk_speed: f64,
        allows_line: impl Fn(LineID) -> bool,
    ) -> Option<(Vec<RoutingStep>, [ValidatedWalk; 2])> {
        let access = self.nearest(origin, &allows_line);
        let egress = self.nearest(destination, &allows_line);
        let (access, egress) = self.route_stop_walks(
            access,
            egress,
            origin,
            destination,
            |from, to| ValidatedWalk::route(now, from, to, map, walk_speed),
            &allows_line,
        )?;
        let ride = self.best_ride(
            access.iter().map(|&(ticks, stop, _)| (ticks, stop)),
            egress.iter().map(|&(ticks, stop, _)| (ticks, stop)),
            allows_line,
        )?;
        let legacy_ticks = legacy_cost(origin, destination, car_pos, walk_speed, now, map);
        if ride.ticks + COST_EPSILON >= legacy_ticks {
            return None;
        }
        let access_path = access
            .into_iter()
            .find(|(_, stop, _)| stop.id == ride.board.id)?
            .2;
        let egress_path = egress
            .into_iter()
            .find(|(_, stop, _)| stop.id == ride.alight.id)?
            .2;

        let mut steps = Vec::with_capacity(5);
        if let Location::Building(building) = *location {
            steps.push(RoutingStep::GetOutBuilding(building));
        }
        steps.extend([
            RoutingStep::WalkTo(ride.board.walk_pos),
            RoutingStep::WaitAtStop {
                stop: ride.board.id,
                line: ride.line,
                since: WAIT_NOT_STARTED,
            },
            RoutingStep::RideTransit {
                line: ride.line,
                alight: ride.alight.id,
            },
            RoutingStep::WalkTo(destination),
        ]);
        Some((steps, [access_path, egress_path]))
    }

    fn route_stop_walks(
        &self,
        mut access: Vec<(OrderedFloat<f32>, &'a TransitStop)>,
        mut egress: Vec<(OrderedFloat<f32>, &'a TransitStop)>,
        origin: Vec3,
        destination: Vec3,
        mut route: impl FnMut(Vec3, Vec3) -> Option<(f64, ValidatedWalk)>,
        allows_line: &impl Fn(LineID) -> bool,
    ) -> Option<(StopWalks<'a>, StopWalks<'a>)> {
        // Check for a shared line first. Dynamic boarding times still need the
        // real access walk because a detour can change the next departure.
        let usable = |board: &TransitStop, alight: &TransitStop| {
            board.id != alight.id
                && self.stop_lines[&board.id].iter().any(|&(id, _)| {
                    allows_line(id)
                        && self.stop_lines[&alight.id]
                            .iter()
                            .any(|&(line, _)| line == id)
                        && self.lines[&id].can_board()
                })
        };
        access.retain(|&(_, board)| egress.iter().any(|&(_, alight)| usable(board, alight)));
        if access.is_empty() {
            return None;
        }
        egress.retain(|&(_, alight)| access.iter().any(|&(_, board)| usable(board, alight)));
        // Route each remaining walk once before ranking the pairs.
        let access: Vec<_> = access
            .into_iter()
            .filter_map(|(_, stop)| {
                let (ticks, path) = route(origin, stop.walk_pos)?;
                Some((ticks, stop, path))
            })
            .collect();
        let egress: Vec<_> = egress
            .into_iter()
            .filter_map(|(_, stop)| {
                let (ticks, path) = route(stop.walk_pos, destination)?;
                Some((ticks, stop, path))
            })
            .collect();
        Some((access, egress))
    }

    fn best_ride(
        &self,
        access: impl Iterator<Item = (f64, &'a TransitStop)>,
        egress: impl Iterator<Item = (f64, &'a TransitStop)> + Clone,
        allows_line: impl Fn(LineID) -> bool,
    ) -> Option<Ride<'a>> {
        let mut best: Option<Ride<'a>> = None;
        for (walk_to_stop, board) in access {
            for &(id, from) in &self.stop_lines[&board.id] {
                if !allows_line(id) {
                    continue;
                }
                let line = &self.lines[&id];
                let Some(wait) = line.wait_ticks(from, walk_to_stop) else {
                    continue;
                };
                for (walk_from_stop, alight) in egress.clone() {
                    if board.id == alight.id {
                        continue;
                    }
                    let Some(bus_ticks) = line.ride_ticks(from, alight.id) else {
                        continue;
                    };
                    let ride = Ride {
                        line: id,
                        board,
                        alight,
                        ticks: walk_to_stop + wait + bus_ticks + line.fare_ticks + walk_from_stop,
                    };
                    if best.as_ref().is_none_or(|other| ride.key() < other.key()) {
                        best = Some(ride);
                    }
                }
            }
        }
        best
    }

    fn nearest(
        &self,
        pos: Vec3,
        allows_line: &impl Fn(LineID) -> bool,
    ) -> Vec<(OrderedFloat<f32>, &'a TransitStop)> {
        let mut stops = Vec::with_capacity(NEAREST_STOPS);
        let radius = Vec2::splat(STOP_ACCESS_RADIUS);
        for (handle, _) in self.stops.query_aabb(pos.xy() - radius, pos.xy() + radius) {
            let &stop = self.stops.get(handle).unwrap().1;
            let distance = OrderedFloat(pos.distance2(stop.walk_pos));
            if distance.0 > STOP_ACCESS_RADIUS.powi(2)
                || !self.stop_lines[&stop.id]
                    .iter()
                    .any(|&(line, _)| allows_line(line))
            {
                continue;
            }
            let index = stops.partition_point(|&(d, s): &(OrderedFloat<f32>, &TransitStop)| {
                (d, s.id) < (distance, stop.id)
            });
            if index < NEAREST_STOPS {
                if stops.len() == NEAREST_STOPS {
                    stops.pop();
                }
                stops.insert(index, (distance, stop));
            }
        }
        stops
    }
}

struct ServiceLine<'a> {
    line: &'a TransitLine,
    leg_ticks: Vec<f64>,
    dwell_ticks: f64,
    departures: Option<Departures>,
    half_headway: f64,
    fare_ticks: f64,
}

impl ServiceLine<'_> {
    fn ride_ticks(&self, from: usize, alight: StopID) -> Option<f64> {
        let len = self.line.stops.len();
        // The wait includes any dwell before departure.
        let mut ticks = 0.0;
        for offset in 0..len - 1 {
            let index = (from + offset) % len;
            ticks += self.leg_ticks[index];
            if self.line.stops[(index + 1) % len] == alight {
                return Some(ticks);
            }
            ticks += self.dwell_ticks;
        }
        None
    }

    fn can_board(&self) -> bool {
        self.departures.is_some()
            || (self.half_headway - self.dwell_ticks).max(0.0) <= GIVE_UP_TICKS as f64
    }

    fn wait_ticks(&self, stop_index: usize, access_ticks: f64) -> Option<f64> {
        let wait = self
            .departures
            .as_ref()
            .map_or(self.half_headway, |departures| {
                departures.wait_ticks(stop_index, access_ticks)
            });
        // The passenger can board at the start of dwell. Trip cost still
        // includes the remaining time until the bus departs.
        let until_boarding = (wait - self.dwell_ticks).max(0.0);
        (until_boarding <= GIVE_UP_TICKS as f64).then_some(wait)
    }
}

struct Departures {
    loop_ticks: u64,
    stop_offsets: Vec<u64>,
    first_departures: Vec<(u64, u64)>,
}

impl Departures {
    fn new<'a>(
        now: Tick,
        leg_ticks: &[u64],
        dwell: u64,
        runs: impl Iterator<Item = (&'a BusRun, Option<u64>)>,
    ) -> Option<Self> {
        let mut stop_offsets = Vec::with_capacity(leg_ticks.len());
        let mut loop_ticks = 0;
        for leg in leg_ticks {
            stop_offsets.push(loop_ticks);
            loop_ticks += leg + dwell;
        }
        let first_departures: Vec<_> = runs
            .filter_map(|(run, recovery_ticks)| {
                let index = run.next_stop_idx;
                let offset = *stop_offsets.get(index)?;
                let until_departure = if let Some(until) = run.dwell_until {
                    until.0.saturating_sub(now.0)
                } else {
                    let previous = (index + leg_ticks.len() - 1) % leg_ticks.len();
                    let expected = recovery_ticks.unwrap_or(leg_ticks[previous]);
                    let elapsed = now.0.saturating_sub(run.leg_started.0);
                    // An overdue bus has no reliable arrival forecast until it
                    // reaches a berth. Counting it as imminent hides the wait.
                    expected.checked_sub(elapsed)? + dwell
                };
                Some((offset, until_departure))
            })
            .collect();
        (!first_departures.is_empty()).then_some(Self {
            loop_ticks,
            stop_offsets,
            first_departures,
        })
    }

    fn wait_ticks(&self, stop_index: usize, access_ticks: f64) -> f64 {
        let offset = self.stop_offsets[stop_index];
        self.first_departures
            .iter()
            .map(|&(target_offset, until_departure)| {
                let after_target = if offset >= target_offset {
                    offset - target_offset
                } else {
                    self.loop_ticks - target_offset + offset
                };
                let wait = until_departure as f64 + after_target as f64 - access_ticks;
                // Only repeat the schedule after the first reachable call.
                if wait < 0.0 {
                    wait.rem_euclid(self.loop_ticks as f64)
                } else {
                    wait
                }
            })
            .reduce(f64::min)
            .unwrap_or_default()
    }
}

struct Ride<'a> {
    line: LineID,
    board: &'a TransitStop,
    alight: &'a TransitStop,
    ticks: f64,
}

impl Ride<'_> {
    fn key(&self) -> (OrderedFloat<f64>, LineID, StopID, StopID) {
        (
            OrderedFloat(self.ticks),
            self.line,
            self.board.id,
            self.alight.id,
        )
    }
}

fn legacy_cost(
    origin: Vec3,
    destination: Vec3,
    car_pos: Option<Vec3>,
    walk_speed: f64,
    now: Tick,
    map: &Map,
) -> f64 {
    if let Some(car) = car_pos {
        travel_ticks(origin.distance(car), walk_speed)
            + travel_ticks(car.distance(destination), CAR_SPEED)
            + PARKING_SECONDS * TICKS_PER_REALTIME_SECOND as f64
    } else {
        // Measure the walk on the sidewalks, as the access and egress walks are.
        // A straight line can cut across blocks and undercount it several times over.
        ValidatedWalk::route(now, origin, destination, map, walk_speed).map_or_else(
            || travel_ticks(origin.distance(destination), walk_speed),
            |(ticks, _)| ticks,
        )
    }
}

pub(super) struct ValidatedWalk {
    start: Vec3,
    end: Vec3,
    topology: u64,
    itinerary: Itinerary,
}

impl ValidatedWalk {
    fn route(now: Tick, start: Vec3, end: Vec3, map: &Map, speed: f64) -> Option<(f64, Self)> {
        let itinerary = Itinerary::route(now, start, end, map, PathKind::Pedestrian)?;
        let ticks = travel_ticks(walking_distance(&itinerary, start, end, map)?, speed);
        Some((
            ticks,
            Self {
                start,
                end,
                topology: map.walking_topology_generation,
                itinerary,
            },
        ))
    }

    pub(super) fn matches(&self, start: Vec3, end: Vec3, map: &Map) -> bool {
        self.start == start && self.end == end && self.topology == map.walking_topology_generation
    }

    pub(super) fn into_itinerary(self) -> Itinerary {
        self.itinerary
    }
}

pub(super) fn walking_distance(it: &Itinerary, start: Vec3, end: Vec3, map: &Map) -> Option<f32> {
    let mut position = start;
    let mut distance = 0.0;
    for &point in it.local_path().iter().rev() {
        distance += position.distance(point);
        position = point;
    }
    if let Some(route) = it.get_route() {
        for (index, traversal) in route.reversed_route.iter().enumerate().rev() {
            if index == 0 {
                let points = PathKind::Pedestrian.local_route(
                    map,
                    traversal.destination_lane(),
                    position,
                    end,
                )?;
                distance += position.distance(points.first()) + points.length();
            } else {
                let points = traversal.raw_points(map)?;
                let (first, last) = match traversal.dir {
                    TraverseDirection::Forward => (points.first(), points.last()),
                    TraverseDirection::Backward => (points.last(), points.first()),
                };
                distance += position.distance(first) + points.length();
                position = last;
            }
        }
    }
    Some(distance)
}

fn travel_ticks(distance: f32, meters_per_second: f64) -> f64 {
    f64::from(distance) / meters_per_second * TICKS_PER_REALTIME_SECOND as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map_dynamic::transit::recovery_leg_fixture;
    use crate::tests::TestCtx;
    use geom::{vec3, Color};

    #[test]
    fn p3b_wait_prediction_accounts_for_boarding_stop_and_access_time() {
        let run = BusRun {
            line: LineID::default(),
            next_stop_idx: 0,
            dwell_until: Some(Tick(30)),
            leg_started: Tick(0),
            failures: 0,
        };
        let departures =
            Departures::new(Tick(20), &[100, 200], 10, [(&run, None)].into_iter()).unwrap();
        assert_eq!(departures.wait_ticks(0, 8.0), 2.0);
        assert_eq!(departures.wait_ticks(0, 12.0), 318.0);
        assert_eq!(departures.wait_ticks(1, 110.0), 10.0);
    }

    #[test]
    fn p3b_wait_prediction_excludes_overdue_departures() {
        let late = BusRun {
            line: LineID::default(),
            next_stop_idx: 1,
            dwell_until: None,
            leg_started: Tick(0),
            failures: 0,
        };
        let dwelling = BusRun {
            dwell_until: Some(Tick(170)),
            ..late.clone()
        };
        let now = Tick(120);
        let legs = [100, 200];
        let departures = Departures::new(
            now,
            &legs,
            10,
            [(&late, None), (&dwelling, None)].into_iter(),
        )
        .unwrap();
        assert_eq!(departures.wait_ticks(1, 0.0), 50.0);

        assert!(Departures::new(now, &legs, 10, [(&late, None)].into_iter()).is_none());
    }

    #[test]
    fn p3b_wait_prediction_keeps_a_single_bus_on_a_recovery_leg() {
        let (ctx, expected) = recovery_leg_fixture();
        let transit = ctx.g.read::<Transit>();
        let run = transit.runs.values().next().unwrap();
        let line = run.line;
        let (legs, dwell) = transit.directed_leg_times(line).unwrap();
        let now = Tick(120);
        let paths = TransitPaths::new(&transit, now, Transit::headway);
        assert!(paths.has_service());
        let departures = paths.lines[&line].departures.as_ref().unwrap();
        let until_departure = expected - now.0 + dwell;
        assert_eq!(
            departures.wait_ticks(run.next_stop_idx, 0.0),
            until_departure as f64
        );
        assert_eq!(
            departures.wait_ticks(0, 0.0),
            (until_departure + legs[1] + dwell) as f64
        );
        assert!(!TransitPaths::new(&transit, Tick(expected + 1), Transit::headway).has_service());
    }

    fn wait_limit_line(positions: [Vec3; 2]) -> Transit {
        let mut transit = Transit::default();
        let stops = positions.map(|pos| {
            transit.stops.insert_with_key(|id| TransitStop {
                id,
                pos,
                dir: Vec2::X,
                walk_pos: pos,
                berth: pos,
            })
        });
        transit.lines.insert_with_key(|id| TransitLine {
            id,
            stops: stops.to_vec(),
            target_vehicles: 2,
            fare: Money::ZERO,
            color: Color::WHITE,
            suspended: false,
        });
        transit
    }

    #[test]
    fn p3b_wait_limit_filters_candidates_by_when_boarding_opens() {
        let origin = Vec3::ZERO;
        let destination = Vec3::X * 2_000.0;
        let transit = wait_limit_line([origin, destination]);
        let line = transit.lines.keys().next().unwrap();
        let [board, alight] = [
            &transit.stops[transit.lines[line].stops[0]],
            &transit.stops[transit.lines[line].stops[1]],
        ];
        let now = Tick(120);
        let mut paths = TransitPaths::new(&transit, now, |_, _| Some(20_000));
        let late = BusRun {
            line,
            next_stop_idx: 1,
            dwell_until: None,
            leg_started: Tick(0),
            failures: 0,
        };
        let next = BusRun {
            next_stop_idx: 0,
            ..late.clone()
        };
        let dwell = 150;
        for extra in [0, 1] {
            let service = paths.lines.get_mut(&line).unwrap();
            service.dwell_ticks = dwell as f64;
            service.departures = Departures::new(
                now,
                &[100, GIVE_UP_TICKS + now.0 + extra],
                dwell,
                [(&late, None), (&next, None)].into_iter(),
            );
            assert!(service.departures.is_some());
            assert_eq!(
                paths
                    .best_ride(
                        [(0.0, board)].into_iter(),
                        [(0.0, alight)].into_iter(),
                        |_| true
                    )
                    .is_some(),
                extra == 0,
                "the boarding window opens {} ticks after reaching the stop",
                GIVE_UP_TICKS + extra,
            );
        }
    }

    #[test]
    fn p3b_wait_limit_rechecks_actual_access_walk() {
        let ctx = TestCtx::new_fast();
        ctx.build_roads(&[vec3(100.0, 500.0, 0.0), vec3(1_600.0, 500.0, 0.0)]);
        let map = ctx.g.map();
        let sidewalk = |pos| {
            map.get(
                map.nearest_lane(pos, crate::map::LaneKind::Walking, None)
                    .unwrap(),
            )
            .unwrap()
            .points
            .project_segment(pos)
            .0
        };
        let origin = sidewalk(vec3(250.0, 505.0, 0.0)) + Vec3::Y * 15.0;
        let board = sidewalk(vec3(300.0, 505.0, 0.0)) + Vec3::Y * 15.0;
        let destination = sidewalk(vec3(1_500.0, 505.0, 0.0));
        let transit = wait_limit_line([board, destination]);
        let line = transit.lines.keys().next().unwrap();
        let now = Tick(76_150);
        let mut paths = TransitPaths::new(&transit, now, |_, _| Some(80_000));
        let service = paths.lines.get_mut(&line).unwrap();
        let departure = 3_000;
        service.dwell_ticks = 150.0;
        service.departures = Departures::new(
            now,
            &[1_000, 79_000],
            150,
            [(
                &BusRun {
                    line,
                    next_stop_idx: 0,
                    dwell_until: None,
                    leg_started: Tick(0),
                    failures: 0,
                },
                None,
            )]
            .into_iter(),
        );
        let walk_speed = 1.0;
        let direct_access = travel_ticks(origin.distance(board), walk_speed);
        assert!(direct_access < departure as f64);
        assert!(service.wait_ticks(0, direct_access).is_some());
        let (access, _) = ValidatedWalk::route(now, origin, board, &map, walk_speed).unwrap();
        assert!(
            access > departure as f64,
            "routed access took {access} ticks"
        );
        let wait = service.departures.as_ref().unwrap().wait_ticks(0, access);
        assert!(wait - service.dwell_ticks > GIVE_UP_TICKS as f64);
        assert!(paths
            .winning_steps(
                origin,
                destination,
                &Location::Outside,
                &map,
                now,
                Some(origin + Vec3::Y * 3_000.0),
                walk_speed,
                |_| true
            )
            .is_none());
    }

    #[test]
    fn p3b_ranks_boarding_stops_by_the_routed_access_walk() {
        let ctx = TestCtx::new_fast();
        ctx.build_roads(&[vec3(100.0, 500.0, 0.0), vec3(1_600.0, 500.0, 0.0)]);
        let map = ctx.g.map();
        let sidewalk = |x, y| {
            let pos = vec3(x, y, 0.0);
            let lane = map
                .nearest_lane(pos, crate::map::LaneKind::Walking, None)
                .unwrap();
            map.get(lane).unwrap().points.project_segment(pos).0
        };
        let origin = sidewalk(300.0, 520.0);
        let across_road = sidewalk(300.0, 480.0);
        let same_side = sidewalk(380.0, 520.0);
        let destination = sidewalk(1_500.0, 520.0);
        let mut transit = wait_limit_line([across_road, destination]);
        let line = transit.lines.keys().next().unwrap();
        let other_stop = transit.stops.insert_with_key(|id| TransitStop {
            id,
            pos: same_side,
            dir: Vec2::X,
            walk_pos: same_side,
            berth: same_side,
        });
        transit.lines[line].stops.insert(1, other_stop);

        let now = Tick(0);
        let paths = TransitPaths::new(&transit, now, |_, _| Some(400));
        assert!(origin.distance(across_road) < origin.distance(same_side));

        let (steps, _) = paths
            .winning_steps(
                origin,
                destination,
                &Location::Outside,
                &map,
                now,
                Some(origin),
                1.0,
                |_| true,
            )
            .expect("the same-side stop offers a faster trip than driving");
        assert!(steps.iter().any(|step| matches!(
            step,
            RoutingStep::WaitAtStop { stop, .. } if *stop == other_stop
        )));
    }

    #[test]
    fn p3b_stop_index_bounds_the_candidates_and_matches_their_lines() {
        let mut transit = Transit::default();
        let mut line_through = |positions: &[f32]| {
            let stops = positions
                .iter()
                .map(|&x| {
                    let pos = Vec3::X * x;
                    transit.stops.insert_with_key(|id| TransitStop {
                        id,
                        pos,
                        dir: Vec2::X,
                        walk_pos: pos,
                        berth: pos,
                    })
                })
                .collect();
            transit.lines.insert_with_key(|id| TransitLine {
                id,
                stops,
                target_vehicles: 1,
                fare: Money::ZERO,
                color: Color::WHITE,
                suspended: false,
            })
        };
        let first = line_through(&[600.0, -600.0, 601.0]);
        let second = line_through(&[2_000.0, 2_001.0, 2_002.0, 2_003.0]);
        let origin = Vec3::ZERO;
        let destination = Vec3::X * 2_000.0;
        let index = TransitPaths::new(&transit, Tick(0), |_, _| Some(400));
        let near_origin: Vec<_> = index
            .nearest(origin, &|_| true)
            .into_iter()
            .map(|(_, stop)| stop.id)
            .collect();
        assert_eq!(near_origin, transit.lines[first].stops[..2]);
        let near_destination: Vec<_> = index
            .nearest(destination, &|_| true)
            .into_iter()
            .map(|(_, stop)| stop.id)
            .collect();
        assert_eq!(near_destination, transit.lines[second].stops[..3]);
        assert!(index
            .best_ride(
                near_origin.iter().map(|&stop| (0.0, &transit.stops[stop])),
                near_destination
                    .iter()
                    .map(|&stop| (0.0, &transit.stops[stop])),
                |_| true,
            )
            .is_none());
        assert!(index
            .route_stop_walks(
                index.nearest(origin, &|_| true),
                index.nearest(destination, &|_| true),
                origin,
                destination,
                |_, _| panic!("unconnected line shortlists must not route sidewalks"),
                &|_| true,
            )
            .is_none());
        assert!(index.nearest(origin, &|id| id == second).is_empty());

        drop(index);
        transit.lines[second].suspended = true;
        let index = TransitPaths::new(&transit, Tick(0), |_, _| Some(400));
        assert!(index.nearest(destination, &|_| true).is_empty());
        let index = TransitPaths::new(&transit, Tick(0), |_, _| None);
        assert!(!index.has_service());
    }
}
