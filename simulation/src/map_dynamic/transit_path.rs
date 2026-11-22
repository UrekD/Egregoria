use std::collections::BTreeMap;

use flat_spatial::Grid;
use geom::{Vec2, Vec3};
use ordered_float::OrderedFloat;
use prototypes::{Money, Tick, TICKS_PER_HOUR, TICKS_PER_REALTIME_SECOND};

use super::{LineID, RoutingStep, StopID, Transit};
use crate::map::{Map, PathKind, Pathfinder, TraverseDirection};
use crate::map_dynamic::transit::{TransitLine, TransitStop, WAIT_NOT_STARTED};
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
        for run in transit.runs.values() {
            runs.entry(run.line).or_default().push(run);
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
                Some((times, dwell)) => (
                    times.iter().map(|&ticks| ticks as f64).collect(),
                    dwell,
                    Departures::new(
                        now,
                        times,
                        dwell,
                        runs.get(&id).map_or(&[][..], Vec::as_slice),
                        line.target_vehicles,
                    ),
                ),
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
        legacy_ticks: f64,
        walk_speed: f64,
        allows_line: impl Fn(LineID) -> bool,
    ) -> Option<(Vec<RoutingStep>, [ValidatedWalk; 2])> {
        let ride = self.best_ride(origin, destination, legacy_ticks, walk_speed, allows_line)?;
        let access = ride.board.walk_pos;
        let egress = ride.alight.walk_pos;
        let (access_ticks, access_path) =
            ValidatedWalk::route(now, origin, access, map, walk_speed)?;
        let (egress_ticks, egress_path) =
            ValidatedWalk::route(now, egress, destination, map, walk_speed)?;
        let wait = self.lines[&ride.line].wait_ticks(ride.board_index, access_ticks);
        if access_ticks + wait + ride.ride_and_fare + egress_ticks + COST_EPSILON >= legacy_ticks {
            return None;
        }

        let mut steps = Vec::with_capacity(5);
        if let Location::Building(building) = *location {
            steps.push(RoutingStep::GetOutBuilding(building));
        }
        steps.extend([
            RoutingStep::WalkTo(access),
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

    fn best_ride(
        &self,
        origin: Vec3,
        destination: Vec3,
        legacy_ticks: f64,
        walk_speed: f64,
        allows_line: impl Fn(LineID) -> bool,
    ) -> Option<Ride<'a>> {
        let access = self.nearest(origin, &allows_line);
        let egress = self.nearest(destination, &allows_line);
        let mut best: Option<Ride<'a>> = None;
        for (board_distance, board) in access {
            let walk_to_stop = travel_ticks(board_distance.0.sqrt(), walk_speed);
            for &(id, from) in &self.stop_lines[&board.id] {
                if !allows_line(id) {
                    continue;
                }
                let line = &self.lines[&id];
                for &(alight_distance, alight) in &egress {
                    if board.id == alight.id {
                        continue;
                    }
                    let Some(bus_ticks) = line.ride_ticks(from, alight.id) else {
                        continue;
                    };
                    let ride_and_fare = bus_ticks + line.fare_ticks;
                    let ride = Ride {
                        line: id,
                        board,
                        board_index: from,
                        alight,
                        ride_and_fare,
                        ticks: walk_to_stop
                            + line.wait_ticks(from, walk_to_stop)
                            + ride_and_fare
                            + travel_ticks(alight_distance.0.sqrt(), walk_speed),
                    };
                    if ride.ticks + COST_EPSILON < legacy_ticks
                        && best.as_ref().is_none_or(|other| ride.key() < other.key())
                    {
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

    fn wait_ticks(&self, stop_index: usize, access_ticks: f64) -> f64 {
        self.departures
            .as_ref()
            .map_or(self.half_headway, |departures| {
                departures.wait_ticks(stop_index, access_ticks)
            })
    }
}

struct Departures {
    loop_ticks: f64,
    stop_offsets: Vec<u64>,
    first_stop_phases: Vec<u64>,
}

impl Departures {
    fn new(now: Tick, leg_ticks: &[u64], dwell: u64, runs: &[&BusRun], target: u8) -> Option<Self> {
        let mut stop_offsets = Vec::with_capacity(leg_ticks.len());
        let mut loop_ticks = 0;
        for leg in leg_ticks {
            stop_offsets.push(loop_ticks);
            loop_ticks += leg + dwell;
        }
        let first_stop_phases: Vec<_> = runs
            .iter()
            .take(usize::from(target))
            .filter_map(|run| {
                let index = run.next_stop_idx;
                let offset = *stop_offsets.get(index)?;
                let previous = (index + leg_ticks.len() - 1) % leg_ticks.len();
                let until_departure = run.dwell_until.map_or_else(
                    || {
                        leg_ticks[previous].saturating_sub(now.0.saturating_sub(run.leg_started.0))
                            + dwell
                    },
                    |until| until.0.saturating_sub(now.0),
                );
                Some((until_departure + loop_ticks - offset) % loop_ticks)
            })
            .collect();
        (!first_stop_phases.is_empty()).then_some(Self {
            loop_ticks: loop_ticks as f64,
            stop_offsets,
            first_stop_phases,
        })
    }

    fn wait_ticks(&self, stop_index: usize, access_ticks: f64) -> f64 {
        let offset = self.stop_offsets[stop_index] as f64;
        self.first_stop_phases
            .iter()
            .map(|&phase| (phase as f64 + offset - access_ticks).rem_euclid(self.loop_ticks))
            .reduce(f64::min)
            .unwrap_or_default()
    }
}

struct Ride<'a> {
    line: LineID,
    board: &'a TransitStop,
    board_index: usize,
    alight: &'a TransitStop,
    ride_and_fare: f64,
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

pub(super) fn legacy_cost(
    origin: Vec3,
    destination: Vec3,
    car_pos: Option<Vec3>,
    walk_speed: f64,
) -> f64 {
    if let Some(car) = car_pos {
        travel_ticks(origin.distance(car), walk_speed)
            + travel_ticks(car.distance(destination), CAR_SPEED)
            + PARKING_SECONDS * TICKS_PER_REALTIME_SECOND as f64
    } else {
        travel_ticks(origin.distance(destination), walk_speed)
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

fn walking_distance(it: &Itinerary, start: Vec3, end: Vec3, map: &Map) -> Option<f32> {
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
    use geom::Color;

    #[test]
    fn p3b_wait_prediction_accounts_for_boarding_stop_and_access_time() {
        let run = BusRun {
            line: LineID::default(),
            next_stop_idx: 0,
            dwell_until: Some(Tick(30)),
            leg_started: Tick(0),
            failures: 0,
        };
        let departures = Departures::new(Tick(20), &[100, 200], 10, &[&run], 1).unwrap();
        assert_eq!(departures.wait_ticks(0, 8.0), 2.0);
        assert_eq!(departures.wait_ticks(0, 12.0), 318.0);
        assert_eq!(departures.wait_ticks(1, 110.0), 10.0);
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
            .best_ride(origin, destination, 100_000.0, 1.2, |_| true)
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
