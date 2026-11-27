use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use flat_spatial::Grid;
use geom::{Vec2, Vec3};
use ordered_float::OrderedFloat;
use prototypes::{Money, Tick, TICKS_PER_HOUR, TICKS_PER_REALTIME_SECOND};
use serde::{Deserialize, Serialize};

use super::{LineID, RoutingStep, StopID, Transit};
use crate::map::{Map, PathKind, Pathfinder, TraverseDirection};
use crate::map_dynamic::transit::{
    RailSchedule, RailTransit, TransitLine, TransitStop, GIVE_UP_TICKS, WAIT_NOT_STARTED,
};
use crate::map_dynamic::{BusRun, Itinerary};
use crate::transportation::Location;

const NEAREST_STOPS: usize = 3;
const TRANSFER_STOPS: usize = 8;
const TRANSFER_RADIUS: f32 = 250.0;
const MAX_TRANSFER_LABELS: usize = 256;
const ROUTE_WALKS_PER_SLICE: usize = 32;
const ROUTE_CANDIDATES_PER_SLICE: usize = 64;
const MAX_CACHED_TRANSFERS: usize = 1_024;
const STOP_ACCESS_RADIUS: f32 = 600.0;
const CAR_SPEED: f64 = 10.0;
const BUS_SPEED: f64 = 8.0;
const PARKING_SECONDS: f64 = 120.0;
const VALUE_OF_TIME_PER_GAME_HOUR: Money = Money::new_bucks(20);
// Allow the pedestrian to accelerate and the router to enter the queue.
const BOARDING_SETUP_TICKS: f64 = TICKS_PER_REALTIME_SECOND as f64;
// Require a meaningful saving over the routed walk even if the traveler has a car.
const MIN_WALK_SAVINGS: f64 = 0.07;
const COST_EPSILON: f64 = 0.01;

type TransferWalks = BTreeMap<(StopID, StopID), Option<(f32, ValidatedWalk)>>;

type RideOrdinals = [Option<(u8, u8)>; 2];

type StopWalks<'a> = Vec<(f64, &'a TransitStop, ValidatedWalk)>;

#[derive(Serialize, Deserialize)]
pub(in crate::map_dynamic) struct PendingSearch {
    pub born: Tick,
    pub revision: (u64, u64),
    pub origin: Vec3,
    pub destination: Vec3,
    endpoints: [EndpointProgress; 2],
    transfer_graph: u64,
    transfer: TransferProgress,
    best_transfer: Option<StoredTransfer>,
}

#[derive(Default, Serialize, Deserialize)]
struct EndpointProgress {
    checked: usize,
    #[serde(
        deserialize_with = "crate::world_command::bounded_command_vec::<_, _, TRANSFER_STOPS>"
    )]
    walks: Vec<(StopID, f64, ValidatedWalk)>,
}

#[derive(Clone, Copy, Default, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
struct TransferPosition {
    access: usize,
    line: usize,
    call: usize,
    neighbor: usize,
}

#[derive(Default, Serialize, Deserialize)]
struct TransferProgress {
    position: TransferPosition,
    reachable: usize,
    scored_labels: usize,
}

impl PendingSearch {
    pub fn new(born: Tick, revision: (u64, u64), origin: Vec3, destination: Vec3) -> Self {
        Self {
            born,
            revision,
            origin,
            destination,
            endpoints: Default::default(),
            transfer_graph: 0,
            transfer: TransferProgress::default(),
            best_transfer: None,
        }
    }

    pub fn valid(&self) -> bool {
        self.origin.is_finite()
            && self.destination.is_finite()
            && self.endpoints.iter().enumerate().all(|(end, progress)| {
                progress.walks.len() <= progress.checked
                    && progress.walks.iter().all(|(_, ticks, walk)| {
                        ticks.is_finite()
                            && *ticks >= 0.0
                            && walk.start.is_finite()
                            && walk.end.is_finite()
                            && if end == 0 {
                                walk.start == self.origin
                            } else {
                                walk.end == self.destination
                            }
                    })
            })
            && self.transfer.position.access < TRANSFER_STOPS
            && self.transfer.position.call < 30
            && self.transfer.reachable < TRANSFER_STOPS
            && self.best_transfer.as_ref().is_none_or(|best| {
                [best.ticks, best.first.ticks, best.second.ticks]
                    .into_iter().all(|ticks| ticks.is_finite() && ticks >= 0.0)
                    && [best.first.board_ordinal, best.first.alight_ordinal,
                        best.second.board_ordinal, best.second.alight_ordinal]
                        .into_iter().all(|ordinal| ordinal < 30)
            })
    }
}

pub(super) struct SearchSlice<'a> {
    pending: &'a mut PendingSearch,
    shared_walks: &'a mut usize,
    walks: usize,
    candidates: usize,
    pub deferred: bool,
}

impl<'a> SearchSlice<'a> {
    pub fn new(pending: &'a mut PendingSearch, shared_walks: &'a mut usize) -> Self {
        Self {
            pending,
            shared_walks,
            walks: ROUTE_WALKS_PER_SLICE,
            candidates: ROUTE_CANDIDATES_PER_SLICE,
            deferred: false,
        }
    }

    fn visit_candidate(&mut self) -> bool {
        if self.candidates == 0 {
            self.deferred = true;
            return false;
        }
        self.candidates -= 1;
        true
    }

    fn allow_walk(&mut self) -> bool {
        if self.walks == 0 || *self.shared_walks == 0 {
            self.deferred = true;
            return false;
        }
        self.walks -= 1;
        *self.shared_walks -= 1;
        true
    }
}

#[derive(Clone, Copy, Default)]
struct FareQuote {
    until: f64,
    paid: f64,
}

impl FareQuote {
    fn from_credit(now: Tick, credit: Option<(Tick, Money)>) -> Self {
        credit.map_or_else(Self::default, |(started, paid)| Self {
            until: started
                .0
                .saturating_add(Transit::FARE_WINDOW_TICKS)
                .saturating_sub(now.0) as f64,
            paid: fare_ticks(paid),
        })
    }

    fn charge(self, boarding: f64, fare: f64) -> (f64, Self) {
        if boarding < self.until {
            (
                (fare - self.paid).max(0.0),
                Self {
                    paid: fare.max(self.paid),
                    ..self
                },
            )
        } else {
            (
                fare,
                Self {
                    until: boarding + Transit::FARE_WINDOW_TICKS as f64,
                    paid: fare,
                },
            )
        }
    }
}

fn fare_ticks(fare: Money) -> f64 {
    fare.inner() as f64 / VALUE_OF_TIME_PER_GAME_HOUR.inner() as f64 * TICKS_PER_HOUR as f64
}

pub(super) struct TransitPaths<'a> {
    lines: BTreeMap<LineID, ServiceLine<'a>>,
    stop_lines: BTreeMap<StopID, Vec<(LineID, usize)>>,
    stops: Grid<&'a TransitStop, Vec2>,
    transfer_walks: RefCell<TransferWalks>,
    stops_by_id: BTreeMap<StopID, &'a TransitStop>,
    platform_walks: BTreeMap<StopID, f32>,
}

impl<'a> TransitPaths<'a> {
    #[cfg(test)]
    pub(super) fn new(
        transit: &'a Transit,
        now: Tick,
        headway: impl Fn(&Transit, LineID) -> Option<u64>,
    ) -> Self {
        Self::with_rail(transit, None, now, headway)
    }

    pub(super) fn with_rail(
        transit: &'a Transit,
        rail: Option<&RailTransit>,
        now: Tick,
        headway: impl Fn(&Transit, LineID) -> Option<u64>,
    ) -> Self {
        let mut paths = Self {
            lines: BTreeMap::new(),
            stop_lines: BTreeMap::new(),
            stops: Grid::new(STOP_ACCESS_RADIUS as i32),
            transfer_walks: RefCell::new(BTreeMap::new()),
            stops_by_id: BTreeMap::new(),
            platform_walks: BTreeMap::new(),
        };
        let mut runs = BTreeMap::<_, Vec<_>>::new();
        for (&bus, run) in &transit.runs {
            runs.entry(run.line).or_default().push((bus, run));
        }
        for (id, line) in &transit.lines {
            if line.suspended {
                continue;
            }
            let rail_line = rail.and_then(|rail| rail.lines.get(&id).map(|line| (rail, line)));
            let headway = if let Some((_, config)) = rail_line {
                if line.target_vehicles == 0 || config.is_retiring() {
                    continue;
                }
                let cycle = config.leg_ticks.iter().sum::<u64>()
                    + config.leg_ticks.len() as u64 * config.schedule.dwell_ticks;
                config
                    .schedule
                    .headway_ticks
                    .max(cycle.div_ceil(u64::from(line.target_vehicles)))
            } else {
                let Some(headway) = headway(transit, id) else {
                    continue;
                };
                headway
            };
            let Some(stops) = line
                .stops
                .iter()
                .map(|&id| transit.stops.get(id))
                .collect::<Option<Vec<_>>>()
            else {
                continue;
            };
            let (leg_ticks, dwell_ticks, departures) = if let Some((rail, config)) = rail_line {
                let predictions: Vec<_> = rail
                    .runs
                    .values()
                    .filter(|run| run.line == id)
                    .take(usize::from(line.target_vehicles))
                    .map(|run| run.forecast(config, now))
                    .collect();
                let Some(mut departures) = Departures::new(
                    now,
                    &config.leg_ticks,
                    config.schedule.dwell_ticks,
                    predictions.iter().map(|run| (run, None)),
                ) else {
                    continue;
                };
                departures.rail_calendar = Some((now, config.schedule, config.next_slot));
                (
                    config.leg_ticks.iter().map(|&ticks| ticks as f64).collect(),
                    config.schedule.dwell_ticks,
                    Some(departures),
                )
            } else {
                match transit.directed_leg_times(id) {
                    Some((times, dwell)) => {
                        let active_runs = runs
                            .get(&id)
                            .into_iter()
                            .flatten()
                            .take(usize::from(line.target_vehicles))
                            .map(|&(bus, run)| (run, transit.recovery_leg_ticks(bus, run)));
                        let Some(departures) = Departures::new(now, times, dwell, active_runs)
                        else {
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
                            .map(|(from, to)| {
                                travel_ticks(from.berth.distance(to.berth), BUS_SPEED)
                            })
                            .collect(),
                        0,
                        None,
                    ),
                }
            };
            let fare_ticks = fare_ticks(line.fare);
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
                    paths.stops_by_id.insert(stop.id, stop);
                    if let Some((rail, _)) = rail_line {
                        paths.platform_walks.insert(stop.id, rail.platform_walk_metres(stop.id));
                    }
                }
                lines.push((id, index));
            }
        }
        paths
    }

    pub(super) fn has_service(&self) -> bool {
        !self.lines.is_empty()
    }

    pub(super) fn has_interchanges(&self) -> bool {
        self.lines.len() > 1
    }

    pub(super) fn needs_search_budget(&self) -> bool {
        self.has_interchanges()
            || self
                .lines
                .first_key_value()
                .is_some_and(|(_, service)| service.line.stops.len() > NEAREST_STOPS)
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
        credit: Option<(Tick, Money)>,
        mut search: Option<&mut SearchSlice<'_>>,
        allows_line: impl Fn(LineID) -> bool,
    ) -> Option<(Vec<RoutingStep>, Vec<ValidatedWalk>, RideOrdinals)> {
        if let Some(search) = search.as_deref_mut() {
            let graph = common::hash_u64(
                self.lines
                    .keys()
                    .copied()
                    .filter(|&line| allows_line(line))
                    .collect::<Vec<_>>(),
            );
            if search.pending.transfer_graph != graph {
                search.pending.transfer_graph = graph;
                search.pending.endpoints = Default::default();
                search.pending.transfer = TransferProgress::default();
                search.pending.best_transfer = None;
            }
        }
        let credit = FareQuote::from_credit(now, credit);
        let (mut access, mut egress) = if !self.needs_search_budget() {
            self.route_stop_walks(
                self.nearest(origin, &allows_line),
                self.nearest(destination, &allows_line),
                origin,
                destination,
                |from, to| ValidatedWalk::route(now, from, to, map, walk_speed),
                &allows_line,
            )?
        } else {
            let mut endpoint = |end| {
                self.route_endpoint(
                    origin,
                    destination,
                    map,
                    now,
                    walk_speed,
                    end,
                    search.as_deref_mut(),
                    &allows_line,
                )
            };
            let access = endpoint(0)?;
            let egress = endpoint(1)?;
            (access, egress)
        };
        if search.as_ref().is_some_and(|search| search.deferred) {
            return None;
        }
        for (ticks, stop, _) in access.iter_mut().chain(&mut egress) {
            *ticks += self.platform_walk_ticks(stop.id, walk_speed);
        }
        let direct = self.best_ride(
            access.iter().map(|&(ticks, stop, _)| (ticks, stop)),
            egress.iter().map(|&(ticks, stop, _)| (ticks, stop)),
            credit,
            &allows_line,
        );
        let transfer = (self.lines.len() > 1)
            .then(|| {
                self.best_transfer(&access, &egress, map, now, walk_speed, credit, search.as_deref_mut(), &allows_line)
            })
            .flatten()
            .filter(|trip| {
                direct
                    .as_ref()
                    .is_none_or(|ride| trip.ticks + COST_EPSILON < ride.ticks)
            });
        if search.as_ref().is_some_and(|search| search.deferred) {
            return None;
        }
        let ticks = transfer
            .as_ref()
            .map(|trip| trip.ticks)
            .or_else(|| direct.as_ref().map(|ride| ride.ticks))?;
        let walk_ticks = ValidatedWalk::route(now, origin, destination, map, walk_speed)
            .map_or_else(
                || if self.platform_walks.is_empty() && !self.has_interchanges() {
                    travel_ticks(origin.distance(destination), walk_speed)
                } else {
                    f64::INFINITY
                },
                |(ticks, _)| ticks,
            );
        if ticks >= walk_ticks * (1.0 - MIN_WALK_SAVINGS)
            || car_pos.is_some_and(|car| {
                let car_access = walk_cost(now, origin, car, map, walk_speed);
                let drive_distance =
                    Itinerary::route(now, car, destination, map, PathKind::Vehicle)
                        .and_then(|it| {
                            itinerary_distance(&it, car, destination, map, PathKind::Vehicle)
                        })
                        .unwrap_or_else(|| car.distance(destination));
                let car_ticks = car_access
                    + travel_ticks(drive_distance, CAR_SPEED)
                    + PARKING_SECONDS * TICKS_PER_REALTIME_SECOND as f64;
                ticks + COST_EPSILON >= car_ticks
            })
        {
            return None;
        }
        let (first, second) = match transfer {
            Some(trip) => (trip.first, Some((trip.second, trip.walk))),
            None => (direct?, None),
        };
        let last_stop = second
            .as_ref()
            .map_or(first.alight.id, |(ride, _)| ride.alight.id);
        let access_path = access
            .into_iter()
            .find(|(_, stop, _)| stop.id == first.board.id)?
            .2;
        let egress_path = egress
            .into_iter()
            .find(|(_, stop, _)| stop.id == last_stop)?
            .2;

        let mut steps = Vec::with_capacity(8);
        let mut walks = vec![access_path];
        if let Location::Building(building) = *location {
            steps.push(RoutingStep::GetOutBuilding(building));
        }
        first.append_steps(&mut steps);
        let mut ordinals = [Some(first.ordinals()), None];
        if let Some((second, walk)) = second {
            second.append_steps(&mut steps);
            ordinals[1] = Some(second.ordinals());
            walks.push(walk);
        }
        steps.push(RoutingStep::WalkTo(destination));
        walks.push(egress_path);
        Some((steps, walks, ordinals))
    }

    fn best_transfer(
        &self,
        access: &StopWalks<'a>,
        egress: &StopWalks<'a>,
        map: &Map,
        now: Tick,
        walk_speed: f64,
        credit: FareQuote,
        mut search: Option<&mut SearchSlice<'_>>,
        allows_line: &impl Fn(LineID) -> bool,
    ) -> Option<Transfer<'a>> {
        let mut best = None;
        let mut resume = TransferPosition::default();
        if let Some(search) = search.as_deref_mut() {
            resume = search.pending.transfer.position;
            best = search
                .pending
                .best_transfer
                .as_ref()
                .and_then(|saved| saved.resolve(&self.stops_by_id, map));
        }
        let mut paths = self.transfer_walks.borrow_mut();
        let mut labels = 0;
        let destination_lines: BTreeSet<_> = egress
            .iter()
            .flat_map(|(_, stop, _)| self.stop_lines[&stop.id].iter().map(|&(id, _)| id))
            .filter(|&id| allows_line(id))
            .collect();
        if destination_lines.is_empty() {
            return None;
        }
        'search: for (access_index, &(access_ticks, board, _)) in
            access.iter().enumerate().skip(resume.access)
        {
            for (line_index, &(first_id, from)) in self.stop_lines[&board.id].iter().enumerate() {
                if access_index == resume.access && line_index < resume.line {
                    continue;
                }
                if !allows_line(first_id) || destination_lines.iter().all(|&id| id == first_id) {
                    continue;
                }
                let first = &self.lines[&first_id];
                let first_board_ordinal = from;
                let wait = first.wait_ticks(from, access_ticks);
                let first_board = (access_ticks + wait.unwrap_or(0.0) - first.dwell_ticks).max(access_ticks);
                let (first_fare, credit) = credit.charge(first_board, first.fare_ticks);
                for (alight_offset, &alight) in first
                    .line
                    .stops
                    .iter()
                    .cycle()
                    .skip(from + 1)
                    .take(first.line.stops.len() - 1)
                    .enumerate()
                {
                    let prefix = TransferPosition {
                        access: access_index,
                        line: line_index,
                        call: alight_offset,
                        neighbor: 0,
                    };
                    if prefix
                        < (TransferPosition {
                            neighbor: 0,
                            ..resume
                        })
                    {
                        continue;
                    }
                    let first_alight = (from + 1 + alight_offset) % first.line.stops.len();
                    if alight == board.id {
                        continue;
                    }
                    let alight = self.stops_by_id[&alight];
                    let Some(first_ride) = first.ride_ticks(from, alight.id) else {
                        continue;
                    };
                    let first_arrival = access_ticks + wait.unwrap_or(0.0) + first_ride;
                    let neighbors =
                        self.nearby(alight.walk_pos, TRANSFER_RADIUS, usize::MAX, |stop| {
                            self.stop_lines[&stop.id]
                                .iter()
                                .any(|&(id, _)| id != first_id && destination_lines.contains(&id))
                        });
                    let mut reachable = 0;
                    for (neighbor, (_, interchange)) in neighbors.into_iter().enumerate() {
                        let position = TransferPosition { neighbor, ..prefix };
                        if position < resume {
                            continue;
                        }
                        let mut already_scored = 0;
                        if let Some(search) = search.as_deref_mut() {
                            let progress = &mut search.pending.transfer;
                            if position == resume {
                                reachable = progress.reachable;
                                already_scored = progress.scored_labels;
                            }
                            *progress = TransferProgress {
                                position,
                                reachable,
                                scored_labels: already_scored,
                            };
                            if !search.visit_candidate() {
                                return None;
                            }
                        }
                        let walk_key = (alight.id, interchange.id);
                        let uncached;
                        let walk = if let Some(walk) = paths.get(&walk_key) {
                            walk.as_ref()
                        } else {
                            if search.as_mut().is_some_and(|search| !search.allow_walk()) {
                                return None;
                            }
                            let walk = ValidatedWalk::route(
                                now,
                                alight.walk_pos,
                                interchange.walk_pos,
                                map,
                                1.0,
                            )
                            .map(|(ticks, walk)| {
                                ((ticks / TICKS_PER_REALTIME_SECOND as f64) as f32, walk)
                            })
                            .filter(|(distance, _)| *distance <= TRANSFER_RADIUS);
                            if paths.len() < MAX_CACHED_TRANSFERS {
                                paths.entry(walk_key).or_insert(walk).as_ref()
                            } else {
                                uncached = walk;
                                uncached.as_ref()
                            }
                        };
                        let Some((distance, walk)) = walk else {
                            continue;
                        };
                        reachable += 1;
                        let mut neighbor_labels = 0;
                        let ready = first_arrival
                            + travel_ticks(*distance, walk_speed)
                            + self.platform_walk_ticks(alight.id, walk_speed)
                            + self.platform_walk_ticks(interchange.id, walk_speed);
                        for &(second_id, from) in &self.stop_lines[&interchange.id] {
                            if first_id == second_id || !destination_lines.contains(&second_id) {
                                continue;
                            }
                            let second = &self.lines[&second_id];
                            let second_wait = second.wait_at(from, ready, *distance > 0.0);
                            for &(egress_ticks, destination, _) in egress {
                                if destination.id == interchange.id {
                                    continue;
                                }
                                let Some(second_ride) = second.ride_ticks(from, destination.id)
                                else {
                                    continue;
                                };
                                neighbor_labels += 1;
                                if neighbor_labels <= already_scored {
                                    continue;
                                }
                                labels += 1;
                                if labels > MAX_TRANSFER_LABELS {
                                    if let Some(search) = search.as_deref_mut() {
                                        search.deferred = true;
                                        return None;
                                    }
                                    break 'search;
                                }
                                if let Some(search) = search.as_deref_mut() {
                                    search.pending.transfer.scored_labels = neighbor_labels;
                                }
                                let (Some(_), Some(second_wait)) = (wait, second_wait) else {
                                    continue;
                                };
                                let second_board =
                                    (ready + second_wait - second.dwell_ticks).max(ready);
                                let (second_fare, _) =
                                    credit.charge(second_board, second.fare_ticks);
                                let ticks = ready
                                    + second_wait
                                    + second_ride
                                    + egress_ticks
                                    + first_fare
                                    + second_fare;
                                let key = (
                                    OrderedFloat(ticks),
                                    first_id,
                                    board.id,
                                    alight.id,
                                    second_id,
                                    interchange.id,
                                    destination.id,
                                );
                                if best.as_ref().is_some_and(|trip| key >= trip.key()) {
                                    continue;
                                }
                                best = Some(Transfer {
                                    first: Ride {
                                        line: first_id,
                                        board,
                                        board_ordinal: first_board_ordinal,
                                        alight,
                                        alight_ordinal: first_alight,
                                        ticks: first_arrival,
                                    },
                                    second: Ride {
                                        line: second_id,
                                        board: interchange,
                                        board_ordinal: from,
                                        alight: destination,
                                        alight_ordinal: second.downstream(from, destination.id).unwrap(),
                                        ticks,
                                    },
                                    ticks,
                                    walk: walk.clone(),
                                });
                                if let Some(search) = search.as_deref_mut() {
                                    search.pending.best_transfer = best.as_ref().map(StoredTransfer::from);
                                }
                            }
                        }
                        if reachable == TRANSFER_STOPS {
                            break;
                        }
                    }
                }
            }
        }
        best
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
        credit: FareQuote,
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
                let boarding = (walk_to_stop + wait - line.dwell_ticks).max(walk_to_stop);
                let (fare, _) = credit.charge(boarding, line.fare_ticks);
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
                        board_ordinal: from,
                        alight,
                        alight_ordinal: line.downstream(from, alight.id).unwrap(),
                        ticks: walk_to_stop + wait + bus_ticks + fare + walk_from_stop,
                    };
                    if best.as_ref().is_none_or(|other| ride.key() < other.key()) {
                        best = Some(ride);
                    }
                }
            }
        }
        best
    }

    fn route_endpoint(
        &self,
        origin: Vec3,
        destination: Vec3,
        map: &Map,
        now: Tick,
        speed: f64,
        end: usize,
        mut search: Option<&mut SearchSlice<'_>>,
        allows_line: &impl Fn(LineID) -> bool,
    ) -> Option<StopWalks<'a>> {
        let pos = [origin, destination][end];
        let limit = if self.has_interchanges() {
            TRANSFER_STOPS
        } else {
            NEAREST_STOPS
        };
        let candidates = self.nearby(pos, STOP_ACCESS_RADIUS, usize::MAX, |stop| {
            self.stop_lines[&stop.id]
                .iter()
                .any(|&(line, _)| allows_line(line))
        });
        let (checked, cached) = search.as_ref().map_or((0, &[][..]), |search| {
            let endpoint = &search.pending.endpoints[end];
            (endpoint.checked, endpoint.walks.as_slice())
        });
        let mut reachable = cached
            .iter()
            .filter_map(|(stop, ticks, walk)| {
                let &stop = self.stops_by_id.get(stop)?;
                let mut walk = walk.clone();
                walk.topology = map.walking_topology_generation;
                Some((*ticks, stop, walk))
            })
            .collect::<Vec<_>>();
        for (index, (_, stop)) in candidates.into_iter().enumerate().skip(checked) {
            if reachable.len() == limit {
                break;
            }
            if search
                .as_mut()
                .is_some_and(|search| !search.visit_candidate() || !search.allow_walk())
            {
                return None;
            }
            let (from, to) = if end == 0 {
                (origin, stop.walk_pos)
            } else {
                (stop.walk_pos, destination)
            };
            let walk = ValidatedWalk::route(now, from, to, map, speed);
            if let Some(search) = search.as_mut() {
                let endpoint = &mut search.pending.endpoints[end];
                endpoint.checked = index + 1;
                if let Some((ticks, walk)) = &walk {
                    endpoint.walks.push((stop.id, *ticks, walk.clone()));
                }
            }
            if let Some((ticks, walk)) = walk {
                reachable.push((ticks, stop, walk));
            }
        }
        Some(reachable)
    }

    #[cfg(test)]
    fn reachable_nearby(
        &self,
        pos: Vec3,
        filter: impl Fn(&TransitStop) -> bool,
        mut route: impl FnMut(&TransitStop) -> Option<(f64, ValidatedWalk)>,
    ) -> StopWalks<'a> {
        self.nearby(pos, STOP_ACCESS_RADIUS, usize::MAX, filter)
            .into_iter()
            .filter_map(|(_, stop)| {
                let (ticks, path) = route(stop)?;
                Some((ticks, stop, path))
            })
            .take(TRANSFER_STOPS)
            .collect()
    }

    fn nearest(
        &self,
        pos: Vec3,
        allows_line: &impl Fn(LineID) -> bool,
    ) -> Vec<(OrderedFloat<f32>, &'a TransitStop)> {
        self.nearby(pos, STOP_ACCESS_RADIUS, NEAREST_STOPS, |stop| {
            self.stop_lines[&stop.id]
                .iter()
                .any(|&(line, _)| allows_line(line))
        })
    }

    fn platform_walk_ticks(&self, stop: StopID, walk_speed: f64) -> f64 {
        self.platform_walks.get(&stop).map_or(0.0, |&metres| travel_ticks(metres, walk_speed))
    }

    fn nearby(
        &self,
        pos: Vec3,
        radius: f32,
        limit: usize,
        filter: impl Fn(&TransitStop) -> bool,
    ) -> Vec<(OrderedFloat<f32>, &'a TransitStop)> {
        let mut stops = Vec::with_capacity(limit.min(TRANSFER_STOPS));
        let bound = Vec2::splat(radius);
        for (handle, _) in self.stops.query_aabb(pos.xy() - bound, pos.xy() + bound) {
            let &stop = self.stops.get(handle).unwrap().1;
            let distance = OrderedFloat(pos.distance2(stop.walk_pos));
            if distance.0 > radius * radius || !filter(stop) {
                continue;
            }
            if limit == usize::MAX {
                stops.push((distance, stop));
                continue;
            }
            let index = stops.partition_point(|&(d, s): &(OrderedFloat<f32>, &TransitStop)| {
                (d, s.id) < (distance, stop.id)
            });
            if index < limit {
                if stops.len() == limit {
                    stops.pop();
                }
                stops.insert(index, (distance, stop));
            }
        }
        if limit == usize::MAX {
            stops.sort_unstable_by_key(|&(distance, stop)| (distance, stop.id));
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
    fn downstream(&self, from: usize, stop: StopID) -> Option<usize> {
        let len = self.line.stops.len();
        (1..len).map(|offset| (from + offset) % len)
            .find(|&ordinal| self.line.stops[ordinal] == stop)
    }

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
        self.wait_at(stop_index, access_ticks, access_ticks > 0.0)
    }

    fn wait_at(&self, stop_index: usize, ready_after: f64, on_foot: bool) -> Option<f64> {
        let Some(departures) = &self.departures else {
            return self.can_board().then_some(self.half_headway);
        };
        let setup = if on_foot { BOARDING_SETUP_TICKS } else { 0.0 };
        let ready = ready_after + setup;
        let wait = departures.wait_ticks(stop_index, ready);
        // The wait deadline is for boarding, which opens at the start of dwell.
        if wait - self.dwell_ticks > GIVE_UP_TICKS as f64 {
            return None;
        }
        // Keep the departure time of a bus already dwelling at this stop.
        // For other calls, shift the forecast toward the expected residual wait
        // as the access walk grows. At two headways we rely on the gap estimate.
        let wait = if departures.rail_calendar.is_some()
            || departures.can_catch_dwelling_bus(stop_index, ready)
        {
            wait
        } else {
            let phase_uncertainty = (ready / (4.0 * self.half_headway)).clamp(0.0, 1.0);
            wait + (departures.mean_wait_ticks - wait) * phase_uncertainty
        };
        Some(setup + wait)
    }
}

struct Departures {
    loop_ticks: u64,
    stop_offsets: Vec<u64>,
    first_departures: Vec<Departure>,
    mean_wait_ticks: f64,
    rail_calendar: Option<(Tick, RailSchedule, u64)>,
}

struct Departure {
    stop_offset: u64,
    until_departure: u64,
    dwelling: bool,
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
                Some(Departure {
                    stop_offset: offset,
                    until_departure,
                    dwelling: run.dwell_until.is_some(),
                })
            })
            .collect();
        // Estimate the residual wait for an uncertain arrival phase. A long
        // gap is more likely to contain the arrival and costs half its length
        // on average. The gaps reflect bunching and exclude overdue buses.
        let mut phases: Vec<_> = first_departures
            .iter()
            .map(|departure| {
                let time = departure.until_departure % loop_ticks;
                if time >= departure.stop_offset {
                    time - departure.stop_offset
                } else {
                    loop_ticks - departure.stop_offset + time
                }
            })
            .collect();
        phases.sort_unstable();
        let (&first, &last) = phases.first().zip(phases.last())?;
        let wrap_gap = loop_ticks - last + first;
        let squared_gaps = phases
            .windows(2)
            .fold((wrap_gap as f64).powi(2), |sum, pair| {
                sum + ((pair[1] - pair[0]) as f64).powi(2)
            });
        let mean_wait_ticks = squared_gaps / (2.0 * loop_ticks as f64);
        Some(Self {
            loop_ticks,
            stop_offsets,
            first_departures,
            mean_wait_ticks,
            rail_calendar: None,
        })
    }

    fn can_catch_dwelling_bus(&self, stop_index: usize, ready_ticks: f64) -> bool {
        self.first_departures.iter().any(|departure| {
            departure.dwelling
                && departure.stop_offset == self.stop_offsets[stop_index]
                && ready_ticks < departure.until_departure as f64
        })
    }

    fn wait_ticks(&self, stop_index: usize, access_ticks: f64) -> f64 {
        let offset = self.stop_offsets[stop_index];
        self.first_departures
            .iter()
            .map(
                |&Departure {
                     stop_offset: target_offset,
                     until_departure,
                     ..
                 }| {
                    if let Some((now, schedule, next_slot)) = self.rail_calendar {
                        if let Some(on_this_lap) = offset.checked_sub(target_offset) {
                            let wait =
                                until_departure.saturating_add(on_this_lap) as f64 - access_ticks;
                            if wait >= 0.0 {
                                return wait;
                            }
                        }
                        let return_ready = now
                            .0
                            .saturating_add(until_departure)
                            .saturating_add(self.loop_ticks - target_offset);
                        let slot = next_slot.max(schedule.slot_at_or_after(
                            return_ready.saturating_sub(schedule.max_late_ticks),
                        ));
                        let next = slot
                            .max(return_ready)
                            .saturating_sub(now.0)
                            .saturating_add(offset) as f64
                            - access_ticks;
                        let cycle = self.loop_ticks.div_ceil(schedule.headway_ticks)
                            * schedule.headway_ticks;
                        return if next < 0.0 {
                            next.rem_euclid(cycle as f64)
                        } else {
                            next
                        };
                    }
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
                },
            )
            .reduce(f64::min)
            .unwrap_or_default()
    }
}

struct Ride<'a> {
    line: LineID,
    board: &'a TransitStop,
    board_ordinal: usize,
    alight: &'a TransitStop,
    alight_ordinal: usize,
    ticks: f64,
}

struct Transfer<'a> {
    first: Ride<'a>,
    second: Ride<'a>,
    ticks: f64,
    walk: ValidatedWalk,
}

#[derive(Serialize, Deserialize)]
struct StoredRide {
    line: LineID,
    board: StopID,
    board_ordinal: usize,
    alight: StopID,
    alight_ordinal: usize,
    ticks: f64,
}

impl From<&Ride<'_>> for StoredRide {
    fn from(ride: &Ride<'_>) -> Self {
        Self {
            line: ride.line,
            board: ride.board.id,
            board_ordinal: ride.board_ordinal,
            alight: ride.alight.id,
            alight_ordinal: ride.alight_ordinal,
            ticks: ride.ticks,
        }
    }
}

impl StoredRide {
    fn resolve<'a>(&self, stops: &BTreeMap<StopID, &'a TransitStop>) -> Option<Ride<'a>> {
        Some(Ride {
            line: self.line,
            board: stops.get(&self.board)?,
            board_ordinal: self.board_ordinal,
            alight: stops.get(&self.alight)?,
            alight_ordinal: self.alight_ordinal,
            ticks: self.ticks,
        })
    }
}

#[derive(Serialize, Deserialize)]
struct StoredTransfer {
    first: StoredRide,
    second: StoredRide,
    ticks: f64,
    walk: ValidatedWalk,
}

impl From<&Transfer<'_>> for StoredTransfer {
    fn from(transfer: &Transfer<'_>) -> Self {
        Self {
            first: StoredRide::from(&transfer.first),
            second: StoredRide::from(&transfer.second),
            ticks: transfer.ticks,
            walk: transfer.walk.clone(),
        }
    }
}

impl StoredTransfer {
    fn resolve<'a>(&self, stops: &BTreeMap<StopID, &'a TransitStop>, map: &Map) -> Option<Transfer<'a>> {
        let mut walk = self.walk.clone();
        walk.topology = map.walking_topology_generation;
        Some(Transfer {
            first: self.first.resolve(stops)?,
            second: self.second.resolve(stops)?,
            ticks: self.ticks,
            walk,
        })
    }
}

impl Transfer<'_> {
    fn key(
        &self,
    ) -> (
        OrderedFloat<f64>,
        LineID,
        StopID,
        StopID,
        LineID,
        StopID,
        StopID,
    ) {
        (
            OrderedFloat(self.ticks),
            self.first.line,
            self.first.board.id,
            self.first.alight.id,
            self.second.line,
            self.second.board.id,
            self.second.alight.id,
        )
    }
}

impl Ride<'_> {
    fn ordinals(&self) -> (u8, u8) {
        (self.board_ordinal as u8, self.alight_ordinal as u8)
    }

    fn append_steps(&self, steps: &mut Vec<RoutingStep>) {
        steps.extend([
            RoutingStep::WalkTo(self.board.walk_pos),
            RoutingStep::WaitAtStop {
                stop: self.board.id,
                line: self.line,
                since: WAIT_NOT_STARTED,
            },
            RoutingStep::RideTransit {
                line: self.line,
                alight: self.alight.id,
            },
        ]);
    }

    fn key(&self) -> (OrderedFloat<f64>, LineID, StopID, StopID) {
        (
            OrderedFloat(self.ticks),
            self.line,
            self.board.id,
            self.alight.id,
        )
    }
}

fn walk_cost(now: Tick, start: Vec3, end: Vec3, map: &Map, speed: f64) -> f64 {
    ValidatedWalk::route(now, start, end, map, speed).map_or_else(
        || travel_ticks(start.distance(end), speed),
        |(ticks, _)| ticks,
    )
}

#[derive(Clone, Serialize, Deserialize)]
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
    itinerary_distance(it, start, end, map, PathKind::Pedestrian)
}

fn itinerary_distance(
    it: &Itinerary,
    start: Vec3,
    end: Vec3,
    map: &Map,
    pathkind: PathKind,
) -> Option<f32> {
    let mut position = start;
    let mut distance = 0.0;
    for &point in it.local_path().iter().rev() {
        distance += position.distance(point);
        position = point;
    }
    if let Some(route) = it.get_route() {
        for (index, traversal) in route.reversed_route.iter().enumerate().rev() {
            if index == 0 {
                let points =
                    pathkind.local_route(map, traversal.destination_lane(), position, end)?;
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
    fn p8_w2_reachable_ninth_stop_survives_disconnected_closer_stops() {
        let mut transit = Transit::default();
        let mut stops = Vec::new();
        for i in 1..=9 {
            let pos = Vec3::X * i as f32;
            stops.push(transit.stops.insert_with_key(|id| TransitStop {
                id,
                pos,
                walk_pos: pos,
                berth: pos,
                dir: Vec2::X,
            }));
        }
        transit.lines.insert_with_key(|id| TransitLine {
            id,
            stops: stops.clone(),
            target_vehicles: 1,
            fare: Money::ZERO,
            color: Color::WHITE,
            suspended: false,
        });
        let index = TransitPaths::new(&transit, Tick(0), |_, _| Some(400));
        let reachable = index.reachable_nearby(
            Vec3::ZERO,
            |_| true,
            |stop| {
                (stop.id == stops[8]).then(|| {
                    (
                        9.0,
                        ValidatedWalk {
                            start: Vec3::ZERO,
                            end: stop.walk_pos,
                            topology: 0,
                            itinerary: Itinerary::simple(vec![stop.walk_pos]),
                        },
                    )
                })
            },
        );
        assert_eq!(
            reachable
                .iter()
                .map(|(_, stop, _)| stop.id)
                .collect::<Vec<_>>(),
            stops[8..]
        );
    }

    #[test]
    fn p8_w2_saved_endpoint_search_resumes_with_a_fresh_work_slice() {
        use common::saveload::{Bincode, Encoder};

        let ctx = TestCtx::new_fast();
        ctx.build_roads(&[vec3(100.0, 500.0, 0.0), vec3(1_600.0, 500.0, 0.0)]);
        let map = ctx.g.map();
        let sidewalk = |x: f32| {
            let pos = vec3(x, 520.0, 0.0);
            let lane = map
                .nearest_lane(pos, crate::map::LaneKind::Walking, None)
                .unwrap();
            map.get(lane).unwrap().points.project(pos)
        };
        let origin = sidewalk(230.0);
        let destination = sidewalk(1_500.0);
        let mut transit = wait_limit_line([sidewalk(280.0), destination]);
        let line = transit.lines.keys().next().unwrap();
        for x in [330.0, 380.0, 430.0] {
            let pos = sidewalk(x);
            let id = transit.stops.insert_with_key(|id| TransitStop {
                id,
                pos,
                walk_pos: pos,
                berth: pos,
                dir: Vec2::X,
            });
            transit.lines[line].stops.push(id);
        }
        let index = TransitPaths::new(&transit, Tick(0), |_, _| Some(400));
        let mut pending = PendingSearch::new(Tick(0), (0, 0), origin, destination);
        let mut allowance = 1;
        let mut slice = SearchSlice::new(&mut pending, &mut allowance);
        assert!(index
            .route_endpoint(
                origin,
                destination,
                &map,
                Tick(0),
                1.0,
                0,
                Some(&mut slice),
                &|_| true
            )
            .is_none());
        assert!(slice.deferred);
        let mut pending: PendingSearch =
            Bincode::decode(&Bincode::encode(&pending).unwrap()).unwrap();
        assert!(pending.valid());

        let mut allowance = 2;
        let mut slice = SearchSlice::new(&mut pending, &mut allowance);
        let stops = index
            .route_endpoint(
                origin,
                destination,
                &map,
                Tick(1),
                1.0,
                0,
                Some(&mut slice),
                &|_| true,
            )
            .unwrap();
        assert_eq!(
            stops.iter().map(|(_, stop, _)| stop.id).collect::<Vec<_>>(),
            [
                transit.lines[line].stops[0],
                transit.lines[line].stops[2],
                transit.lines[line].stops[3],
            ]
        );
        assert!(!slice.deferred);
        assert_eq!(allowance, 0);
    }

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
                        FareQuote::default(),
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
                None,
                None,
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

        let (steps, _, _) = paths
            .winning_steps(
                origin,
                destination,
                &Location::Outside,
                &map,
                now,
                Some(origin),
                1.0,
                None,
                None,
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
                FareQuote::default(),
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
