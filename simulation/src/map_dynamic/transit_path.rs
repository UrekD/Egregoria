use std::collections::BTreeSet;

use geom::Vec3;
use ordered_float::OrderedFloat;
use prototypes::{Money, Tick, TICKS_PER_HOUR, TICKS_PER_REALTIME_SECOND};

use super::{LineID, RoutingStep, StopID, Transit};
use crate::map::{Map, PathKind};
use crate::map_dynamic::transit::{TransitLine, TransitStop, WAIT_NOT_STARTED};
use crate::map_dynamic::Itinerary;
use crate::transportation::Location;

const NEAREST_STOPS: usize = 3;
const STOP_ACCESS_RADIUS: f32 = 600.0;
const WALK_SPEED: f64 = 1.2;
const CAR_SPEED: f64 = 10.0;
const BUS_SPEED: f64 = 8.0;
const PARKING_SECONDS: f64 = 120.0;
const VALUE_OF_TIME_PER_GAME_HOUR: Money = Money::new_bucks(20);
// Nominal headway assumes even bus spacing. Allow for bunching and traffic delays.
pub(super) const HEADWAY_MULTIPLIER: u64 = 5;

pub(super) struct TransitPaths<'a> {
    lines: Vec<(&'a TransitLine, u64)>,
    stops: Vec<&'a TransitStop>,
}

impl<'a> TransitPaths<'a> {
    pub(super) fn new(
        transit: &'a Transit,
        headway: impl Fn(&Transit, LineID) -> Option<u64>,
    ) -> Self {
        let running_lines: BTreeSet<_> = transit.runs.values().map(|run| run.line).collect();
        let lines: Vec<_> = running_lines
            .into_iter()
            .filter_map(|id| {
                let line = transit.lines.get(id).filter(|line| !line.suspended)?;
                Some((line, headway(transit, id)?))
            })
            .collect();
        let stop_ids: BTreeSet<_> = lines.iter().flat_map(|(line, _)| &line.stops).collect();
        let stops = stop_ids
            .into_iter()
            .filter_map(|&id| transit.stops.get(id))
            .collect();
        Self { lines, stops }
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
    ) -> Option<Vec<RoutingStep>> {
        let ride = self.best_ride(origin, destination, legacy_ticks)?;
        let access = ride.board.walk_pos;
        let egress = ride.alight.walk_pos;
        Itinerary::route(now, origin, access, map, PathKind::Pedestrian)?;
        Itinerary::route(now, egress, destination, map, PathKind::Pedestrian)?;

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
        Some(steps)
    }

    fn best_ride(&self, origin: Vec3, destination: Vec3, legacy_ticks: f64) -> Option<Ride<'a>> {
        let access = self.nearest(origin);
        let egress = self.nearest(destination);
        let mut best: Option<Ride<'a>> = None;
        for &(line, headway) in &self.lines {
            let fare_ticks = line.fare.inner() as f64 / VALUE_OF_TIME_PER_GAME_HOUR.inner() as f64
                * TICKS_PER_HOUR as f64;
            let wait_and_fare = headway as f64 * 0.5 + fare_ticks;
            for &(board, walk_to_stop) in &access {
                if !line.stops.contains(&board.id) {
                    continue;
                }
                for &(alight, walk_from_stop) in &egress {
                    if board.id == alight.id || !line.stops.contains(&alight.id) {
                        continue;
                    }
                    let bus_ticks = travel_ticks(board.berth.distance(alight.berth), BUS_SPEED);
                    let ride = Ride {
                        line: line.id,
                        board,
                        alight,
                        ticks: walk_to_stop + bus_ticks + wait_and_fare + walk_from_stop,
                    };
                    if ride.ticks < legacy_ticks
                        && best.as_ref().is_none_or(|other| ride.key() < other.key())
                    {
                        best = Some(ride);
                    }
                }
            }
        }
        best
    }

    fn nearest(&self, pos: Vec3) -> Vec<(&'a TransitStop, f64)> {
        let mut stops: Vec<_> = self
            .stops
            .iter()
            .filter_map(|&stop| {
                let distance = pos.distance2(stop.walk_pos);
                (distance <= STOP_ACCESS_RADIUS.powi(2)).then_some((OrderedFloat(distance), stop))
            })
            .collect();
        stops.sort_unstable_by_key(|&(distance, stop)| (distance, stop.id));
        stops
            .into_iter()
            .take(NEAREST_STOPS)
            .map(|(distance, stop)| (stop, travel_ticks(distance.into_inner().sqrt(), WALK_SPEED)))
            .collect()
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

pub(super) fn legacy_cost(origin: Vec3, destination: Vec3, car_pos: Option<Vec3>) -> f64 {
    if let Some(car) = car_pos {
        travel_ticks(origin.distance(car), WALK_SPEED)
            + travel_ticks(car.distance(destination), CAR_SPEED)
            + PARKING_SECONDS * TICKS_PER_REALTIME_SECOND as f64
    } else {
        travel_ticks(origin.distance(destination), WALK_SPEED)
    }
}

fn travel_ticks(distance: f32, meters_per_second: f64) -> f64 {
    f64::from(distance) / meters_per_second * TICKS_PER_REALTIME_SECOND as f64
}
