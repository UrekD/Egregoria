use std::collections::{BTreeMap, BTreeSet, VecDeque};

use geom::Vec3;
use prototypes::{ItemID, Money, Tick};
use serde::{Deserialize, Serialize};

use super::{
    bounded_transfer, CargoBook, Endpoint, ImportDesk, LoadId, ShipmentId, TRAIN_CAPACITY,
};
use crate::map::{BuildingID, LaneID, LaneKind, Map, TraverseKind};
use crate::map_dynamic::Itinerary;
use crate::{CompanyID, TrainID, VehicleID};

const POSITION_TOLERANCE: f32 = 0.04;
const MAX_HISTORY: usize = 65_536;
const MAX_ACTIVE: usize = 65_536;
const RETAIN_TICKS: u64 = 7 * super::TICKS_PER_BUDGET_DAY;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) struct RailAnchor {
    pub lane: LaneID,
    offset: f32,
    pub position: Vec3,
    dir: Vec3,
}

impl RailAnchor {
    pub fn near(map: &Map, hint: Vec3) -> Option<Self> {
        let lane = map.nearest_lane(hint, LaneKind::Rail, Some(50.0))?;
        let track = &map.get(lane)?.points;
        let offset = track.length_at_proj(track.project(hint));
        let (position, dir) = track.point_dir_along(offset);
        (position.xy().distance(hint.xy()) <= 50.0 && (position.z - hint.z).abs() < 4.0).then_some(
            Self {
                lane,
                offset,
                position,
                dir,
            },
        )
    }

    fn well_formed(self) -> bool {
        self.offset.is_finite()
            && self.offset >= 0.0
            && self.position.is_finite()
            && self.dir.is_finite()
            && (0.99..=1.01).contains(&self.dir.mag2())
    }

    pub fn live(self, map: &Map) -> bool {
        map.get(self.lane).is_some_and(|lane| {
            lane.kind == LaneKind::Rail
                && (0.0..=lane.points.length()).contains(&self.offset)
                && lane.points.point_along(self.offset).distance(self.position)
                    <= POSITION_TOLERANCE
                && lane.points.point_dir_along(self.offset).1.dot(self.dir) > 0.999
        })
    }

    pub fn route(self, tick: Tick, map: &Map, start: Vec3, lane: LaneID) -> Option<Itinerary> {
        if !self.live(map) {
            return None;
        }
        let track = map.get(lane)?;
        if track.kind != LaneKind::Rail || track.points.project_dist2(start) > 0.25 {
            return None;
        }
        Itinerary::route_rail(tick, map, start, lane, self.lane, self.offset)
    }

    pub fn departure_point(self, map: &Map) -> Option<Self> {
        let track = &map.get(self.lane)?.points;
        let offset = (self.offset + 40.0).min(track.length() - 0.1);
        if offset <= self.offset {
            return None;
        }
        let (position, dir) = track.point_dir_along(offset);
        Some(Self {
            offset,
            position,
            dir,
            ..self
        })
    }

    pub fn before(self, position: Vec3) -> bool {
        (position - self.position).dot(self.dir) < 0.0
    }

    fn crossed(self, from: Vec3, to: Vec3) -> bool {
        from.distance(self.position) < 10.0 && self.before(from) && !self.before(to)
    }

    fn reached(self, traversal: TraverseKind, from: Vec3, to: Vec3) -> bool {
        let remaining = to.distance(self.position);
        if remaining > POSITION_TOLERANCE {
            return false;
        }
        match traversal {
            TraverseKind::Lane(lane) if lane == self.lane => {
                (to - from).dot(self.dir) > 0.0 && self.before(from)
            }
            // At offset zero the inbound movement ends on the turn whose exit
            // is the start of this lane. There may be no nonzero lane segment.
            TraverseKind::Turn(turn)
                if turn.dst == self.lane && self.offset <= POSITION_TOLERANCE =>
            {
                from.distance(self.position) > remaining
            }
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FreightCallKey {
    pub terminal: Endpoint,
    pub train: TrainID,
    pub epoch: u64,
    pub outside_gate: BuildingID,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FreightStage {
    Reserved,
    InboundRail,
    TerminalStock,
    OutboundTruck,
    Delivered,
    Returned,
    ExportStaged,
    ExportLoaded,
    OutboundRail,
    Exited,
    WrittenOff,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FreightFailure {
    TrainRemoved,
    TerminalRemoved,
    OutsideGateRemoved,
    TrackChanged,
    RailStalled,
    TruckInterrupted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DemandKey {
    pub building: BuildingID,
    pub owner: CompanyID,
    pub item: ItemID,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FreightFragment {
    pub id: ShipmentId,
    pub parent: Option<ShipmentId>,
    pub item: Option<ItemID>,
    pub units: u32,
    pub call: Option<FreightCallKey>,
    pub ordered_for: Option<Endpoint>,
    pub ordered_at: Option<u64>,
    pub rail_received_at: Option<u64>,
    pub source: Option<crate::SoulID>,
    pub booked: Money,
    pub ledger: Option<ShipmentId>,
    pub road_load: Option<LoadId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FreightRecord {
    pub fragment: FreightFragment,
    pub stage: FreightStage,
    pub terminal: Endpoint,
    pub receiver: Option<Endpoint>,
    pub truck: Option<VehicleID>,
    pub load: Option<LoadId>,
    pub elapsed: u64,
    pub failure: Option<FreightFailure>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum RailLeg {
    Positioning,
    Inbound,
    Loading { until: u64 },
    Outbound,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct CallManifest {
    pub key: FreightCallKey,
    pub gate: RailAnchor,
    pub gate_departure: RailAnchor,
    pub berth: RailAnchor,
    pub leg: RailLeg,
    pub departed: bool,
    pub reached: bool,
    pub departure_recorded: bool,
    pub retry_at: u64,
    pub last_motion: u64,
    pub imports: Vec<FreightFragment>,
    pub exports: Vec<FreightFragment>,
    pub exports_loaded: bool,
}

impl CallManifest {
    pub fn export_units(&self) -> u32 {
        self.exports
            .iter()
            .fold(0u32, |total, fragment| total.saturating_add(fragment.units))
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct TruckManifest {
    load: LoadId,
    fragments: Vec<FreightFragment>,
    lane: LaneID,
    position: Vec3,
    arrived: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct FreightTrace {
    next_id: u64,
    revision: u64,
    pub(crate) policies: BTreeMap<Endpoint, super::TerminalPolicyView>,
    pub(crate) depots: BTreeMap<CompanyID, super::DepotRestockView>,
    pub(crate) calls: BTreeMap<TrainID, CallManifest>,
    stock: BTreeMap<(Endpoint, ItemID), VecDeque<FreightFragment>>,
    exports: BTreeMap<Endpoint, VecDeque<FreightFragment>>,
    trucks: BTreeMap<VehicleID, TruckManifest>,
    history: VecDeque<FreightRecord>,
    omitted: u64,
    active_records: usize,
    #[serde(skip)]
    by_query: BTreeMap<FreightQuery, VecDeque<u64>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum FreightQuery {
    Demand(DemandKey),
    Terminal(Endpoint),
    Corridor(BuildingID),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FreightCursor {
    query: FreightQuery,
    revision: u64,
    before: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StaleFreightCursor;

#[derive(Debug)]
pub struct FreightPage {
    pub revision: u64,
    pub rows: Vec<FreightRecord>,
    pub omitted: u64,
    pub next: Option<FreightCursor>,
}

impl FreightRecord {
    fn queries(&self) -> impl Iterator<Item = FreightQuery> {
        let demand = |endpoint: Endpoint| {
            let crate::SoulID::GoodsCompany(owner) = endpoint.soul else {
                return None;
            };
            Some(FreightQuery::Demand(DemandKey {
                building: endpoint.building,
                owner,
                item: self.fragment.item?,
            }))
        };
        let original = self
            .fragment
            .ordered_for
            .filter(|original| Some(*original) != self.receiver);
        [
            Some(FreightQuery::Terminal(self.terminal)),
            self.receiver.and_then(demand),
            original.and_then(demand),
            self.fragment
                .call
                .map(|call| FreightQuery::Corridor(call.outside_gate)),
        ]
        .into_iter()
        .flatten()
    }
}

impl FreightTrace {
    pub(crate) fn is_pristine(&self) -> bool {
        self.next_id == 0 && self.policies.is_empty() && self.depots.is_empty()
    }

    pub(crate) fn remove_terminal(&mut self, terminal: Endpoint, now: u64) {
        self.policies.remove(&terminal);
        if let Some(exports) = self.exports.remove(&terminal) {
            self.active_records -= exports.len();
            for fragment in exports {
                self.record(terminal, &fragment, FreightStage::WrittenOff, None, now);
            }
        }
        let keys: Vec<_> = self
            .stock
            .keys()
            .copied()
            .filter(|(end, _)| *end == terminal)
            .collect();
        for key in keys {
            let stock = self.stock.remove(&key).unwrap_or_default();
            self.active_records -= stock.len();
            for fragment in stock {
                self.record(terminal, &fragment, FreightStage::WrittenOff, None, now);
            }
        }
    }

    pub(crate) fn next_id(&mut self) -> u64 {
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("freight identifiers exhausted");
        self.next_id
    }

    pub fn terminal_policy(&self, endpoint: Endpoint) -> super::TerminalPolicyView {
        self.policies.get(&endpoint).copied().unwrap_or_default()
    }

    pub fn trace_demand(&self, demand: DemandKey) -> FreightPage {
        self.page(FreightQuery::Demand(demand), None).unwrap()
    }

    pub fn terminal_view(&self, endpoint: Endpoint) -> FreightPage {
        self.page(FreightQuery::Terminal(endpoint), None).unwrap()
    }

    pub fn corridor_report(&self, outside_gate: BuildingID) -> FreightPage {
        self.page(FreightQuery::Corridor(outside_gate), None)
            .unwrap()
    }

    pub fn continue_trace(&self, cursor: FreightCursor) -> Result<FreightPage, StaleFreightCursor> {
        self.page(cursor.query, Some(cursor))
    }

    fn page(
        &self,
        query: FreightQuery,
        cursor: Option<FreightCursor>,
    ) -> Result<FreightPage, StaleFreightCursor> {
        const ROWS: usize = 64;
        let before = match cursor {
            Some(cursor) if cursor.revision == self.revision && cursor.query == query => {
                cursor.before
            }
            Some(_) => return Err(StaleFreightCursor),
            None => u64::MAX,
        };
        let mut rows = Vec::new();
        let mut next = None;
        if let Some(ids) = self.by_query.get(&query) {
            let end = ids.partition_point(|id| *id < before);
            let start = end.saturating_sub(ROWS);
            rows.extend(
                (start..end)
                    .rev()
                    .map(|idx| self.history[(ids[idx] - self.omitted) as usize].clone()),
            );
            if start > 0 {
                next = Some(FreightCursor {
                    query,
                    revision: self.revision,
                    before: ids[start],
                });
            }
        }
        Ok(FreightPage {
            revision: self.revision,
            rows,
            omitted: self.omitted,
            next,
        })
    }

    fn trim_one(&mut self) {
        let row = self.history.pop_front().expect("only trim retained rows");
        for query in row.queries() {
            let ids = self.by_query.get_mut(&query).expect("history indexed");
            ids.pop_front();
            if ids.is_empty() {
                self.by_query.remove(&query);
            }
        }
        self.omitted += 1;
        self.revision += 1;
    }

    pub(crate) fn rebuild_indexes(&mut self) -> Result<(), &'static str> {
        let records = self
            .stock
            .values()
            .chain(self.exports.values())
            .map(VecDeque::len)
            .sum::<usize>()
            + self
                .calls
                .values()
                .map(|call| call.imports.len() + call.exports.len())
                .sum::<usize>()
            + self
                .trucks
                .values()
                .map(|truck| truck.fragments.len())
                .sum::<usize>();
        if records != self.active_records
            || records > MAX_ACTIVE
            || self.history.len() > MAX_HISTORY
            || self
                .omitted
                .checked_add(self.history.len() as u64)
                .is_none()
            || self
                .policies
                .values()
                .any(|view| !view.policy.valid() || view.revision == u64::MAX)
            || self
                .depots
                .values()
                .any(|view| view.revision == u64::MAX || view.limits.low >= view.limits.high)
            || self.revision == u64::MAX
            || self.next_id == u64::MAX
            || self.calls.iter().any(|(&train, call)| {
                train != call.key.train
                    || call.export_units() > TRAIN_CAPACITY
                    || call
                        .imports
                        .iter()
                        .map(|fragment| u64::from(fragment.units))
                        .sum::<u64>()
                        > u64::from(TRAIN_CAPACITY)
                    || !call.gate.well_formed()
                    || !call.gate_departure.well_formed()
                    || !call.berth.well_formed()
            })
        {
            return Err("invalid freight trace capacity, policy or call");
        }
        let mut seen = BTreeSet::new();
        let active = self
            .stock
            .values()
            .chain(self.exports.values())
            .flatten()
            .chain(
                self.calls
                    .values()
                    .flat_map(|call| call.imports.iter().chain(&call.exports)),
            )
            .chain(
                self.trucks
                    .values()
                    .flat_map(|truck| truck.fragments.iter()),
            );
        if active.into_iter().any(|line| {
            line.units == 0
                || line.id.0 == 0
                || line.id.0 > self.next_id
                || !seen.insert(line.id)
                || line
                    .item
                    .is_some_and(|item| prototypes::try_prototype(item).is_none())
        }) {
            return Err("invalid or duplicate active freight fragment");
        }
        self.by_query.clear();
        for (index, row) in self.history.iter().enumerate() {
            if row.fragment.units == 0 || row.fragment.id.0 > self.next_id {
                return Err("invalid freight record");
            }
            for query in row.queries() {
                self.by_query
                    .entry(query)
                    .or_default()
                    .push_back(self.omitted + index as u64);
            }
        }
        Ok(())
    }

    pub(crate) fn validate_ownership(
        &self,
        book: &CargoBook,
        world: &crate::World,
    ) -> Result<(), &'static str> {
        let mut reserved = BTreeMap::<(Endpoint, ItemID), u64>::new();
        for call in self.calls.values() {
            let crate::SoulID::FreightStation(station) = call.key.terminal.soul else {
                return Err("freight call has no terminal owner");
            };
            if world.trains.get(call.key.train).is_none()
                || !world.freight_stations.get(station).is_some_and(|owner| {
                    owner.f.building == call.key.terminal.building
                        && owner
                            .f
                            .trains
                            .iter()
                            .any(|&(train, _)| train == call.key.train)
                })
                || call.exports_loaded
                    != matches!(call.leg, RailLeg::Loading { .. } | RailLeg::Outbound)
            {
                return Err("freight call disagrees with its train or terminal assignment");
            }
            for line in &call.imports {
                let Some(item) = line.item else {
                    return Err("an import has no item");
                };
                *reserved.entry((call.key.terminal, item)).or_default() += u64::from(line.units);
            }
        }
        for ((terminal, item), units) in reserved {
            let ordered = book
                .imports
                .get(&terminal)
                .and_then(|desk| desk.on_rail.get(&item));
            if ordered.is_none_or(|&ordered| ordered < units) {
                return Err("reserved rail freight exceeds its orders");
            }
        }
        for (&(terminal, item), fragments) in &self.stock {
            let units = fragments
                .iter()
                .map(|line| u64::from(line.units))
                .sum::<u64>();
            let stock = book
                .imports
                .get(&terminal)
                .and_then(|desk| desk.stock.get(&item));
            if stock.is_none_or(|&stock| stock < units) {
                return Err("traced terminal stock exceeds physical stock");
            }
        }
        for (&truck, manifest) in &self.trucks {
            let Some(trip) = book
                .station_trucks
                .get(&truck)
                .and_then(|truck| truck.trip.as_ref())
            else {
                return Err("traced cargo has no active truck trip");
            };
            let mut traced = BTreeMap::<ItemID, u64>::new();
            for line in &manifest.fragments {
                let Some(item) = line.item else {
                    return Err("a truck fragment has no item");
                };
                *traced.entry(item).or_default() += u64::from(line.units);
            }
            if trip.returning
                || !manifest.position.is_finite()
                || world.vehicles.get(truck).is_none()
                || traced.len() != trip.items.len()
                || trip
                    .items
                    .iter()
                    .any(|line| traced.get(&line.item) != Some(&u64::from(line.units)))
            {
                return Err("traced truck cargo disagrees with physical cargo");
            }
        }
        Ok(())
    }

    pub(crate) fn admit_export(&self, endpoint: Endpoint) -> bool {
        self.terminal_policy(endpoint).policy.admit_exports && self.active_records < MAX_ACTIVE / 2
    }

    pub(crate) fn unreserved_exports(&self, terminal: Endpoint, waiting: u32) -> u32 {
        let reserved = self
            .calls
            .values()
            .filter(|call| call.key.terminal == terminal && !call.exports_loaded)
            .map(CallManifest::export_units)
            .sum::<u32>();
        waiting.saturating_sub(reserved)
    }

    pub(crate) fn may_move(&self, train: TrainID) -> bool {
        let Some(call) = self.calls.get(&train) else {
            return true;
        };
        call.leg != RailLeg::Inbound || call.departed || self.may_start(call)
    }

    pub(crate) fn may_start(&self, call: &CallManifest) -> bool {
        let prior = self
            .calls
            .values()
            .filter(|other| {
                other.key.terminal == call.key.terminal
                    && other.key.train != call.key.train
                    && (other.departed
                        || matches!(other.leg, RailLeg::Loading { .. } | RailLeg::Outbound)
                        || other.key.epoch < call.key.epoch)
            })
            .count();
        prior < usize::from(self.terminal_policy(call.key.terminal).policy.max_calls)
    }

    fn unknown(&mut self, item: Option<ItemID>, units: u32) -> FreightFragment {
        FreightFragment {
            id: ShipmentId(self.next_id()),
            parent: None,
            item,
            units,
            call: None,
            ordered_for: None,
            ordered_at: None,
            rail_received_at: None,
            source: None,
            booked: Money::ZERO,
            ledger: None,
            road_load: None,
        }
    }

    fn record(
        &mut self,
        terminal: Endpoint,
        fragment: &FreightFragment,
        stage: FreightStage,
        truck: Option<(VehicleID, LoadId, Endpoint)>,
        elapsed: u64,
    ) {
        self.age(elapsed);
        if self.history.len() == MAX_HISTORY {
            self.trim_one();
        }
        self.revision += 1;
        let row = FreightRecord {
            fragment: fragment.clone(),
            stage,
            terminal,
            truck: truck.map(|(id, _, _)| id),
            load: truck.map(|(_, id, _)| id),
            receiver: truck.map(|(_, _, receiver)| receiver),
            elapsed,
            failure: None,
        };
        let id = self.omitted + self.history.len() as u64;
        for query in row.queries() {
            self.by_query.entry(query).or_default().push_back(id);
        }
        self.history.push_back(row);
    }

    pub(crate) fn age(&mut self, now: u64) {
        while self
            .history
            .front()
            .is_some_and(|row| now.saturating_sub(row.elapsed) >= RETAIN_TICKS)
        {
            self.trim_one();
        }
    }

    pub(crate) fn stage_export(
        &mut self,
        terminal: Endpoint,
        trade: super::Trade,
        load: Option<LoadId>,
        now: u64,
    ) {
        if trade.qty <= 0 {
            return;
        }
        let mut fragment = self.unknown(Some(trade.kind), trade.qty as u32);
        fragment.source = Some(trade.seller.0);
        fragment.booked = trade.money_delta;
        fragment.ledger = Some(fragment.id);
        fragment.road_load = load;
        self.record(terminal, &fragment, FreightStage::ExportStaged, None, now);
        self.exports
            .entry(terminal)
            .or_default()
            .push_back(fragment);
        self.active_records += 1;
    }

    pub(super) fn available_imports(
        &self,
        terminal: Endpoint,
        desk: &ImportDesk,
    ) -> BTreeMap<ItemID, u64> {
        let mut available = desk.on_rail.clone();
        for call in self
            .calls
            .values()
            .filter(|call| call.key.terminal == terminal)
        {
            for fragment in &call.imports {
                if let Some(units) = fragment.item.and_then(|item| available.get_mut(&item)) {
                    *units = units.saturating_sub(u64::from(fragment.units));
                }
            }
        }
        available
    }

    pub(super) fn reserve_imports(&mut self, call: &mut CallManifest, desk: &ImportDesk, now: u64) {
        let available = self.available_imports(call.key.terminal, desk);
        let credit = (MAX_ACTIVE / 2).saturating_sub(self.active_records);
        let mut room = TRAIN_CAPACITY.min(credit as u32);
        for (item, unassigned) in available {
            let mut units = bounded_transfer(unassigned, room);
            if units == 0 {
                continue;
            }
            room -= units;
            let mut covered = desk.stock.get(&item).copied().unwrap_or(0)
                + desk
                    .on_rail
                    .get(&item)
                    .copied()
                    .unwrap_or(0)
                    .saturating_sub(unassigned);
            for order in desk
                .orders
                .iter()
                .filter(|order| order.quantity.item == item)
            {
                let skipped = bounded_transfer(covered, order.quantity.units);
                covered -= u64::from(skipped);
                let quantity = units.min(order.quantity.units - skipped);
                if quantity > 0 {
                    let mut fragment = self.unknown(Some(item), quantity);
                    fragment.call = Some(call.key);
                    fragment.ordered_for = Some(order.buyer);
                    fragment.ordered_at = Some(order.ordered_at);
                    self.record(
                        call.key.terminal,
                        &fragment,
                        FreightStage::Reserved,
                        None,
                        now,
                    );
                    call.imports.push(fragment);
                    self.active_records += 1;
                    units -= quantity;
                }
                if units == 0 {
                    break;
                }
            }
            if units > 0 {
                let mut fragment = self.unknown(Some(item), units);
                fragment.call = Some(call.key);
                self.record(
                    call.key.terminal,
                    &fragment,
                    FreightStage::Reserved,
                    None,
                    now,
                );
                call.imports.push(fragment);
                self.active_records += 1;
            }
        }
    }

    pub(crate) fn observe_motion(
        &mut self,
        train: TrainID,
        traversal: TraverseKind,
        from: Vec3,
        to: Vec3,
        now: u64,
    ) {
        let Some(call) = self.calls.get_mut(&train) else {
            return;
        };
        call.last_motion = now;
        let anchor = match call.leg {
            RailLeg::Positioning | RailLeg::Outbound => call.gate,
            RailLeg::Inbound => call.berth,
            RailLeg::Loading { .. } => return,
        };
        if anchor.reached(traversal, from, to) {
            call.reached = true;
        }
        if call.leg == RailLeg::Inbound
            && traversal == TraverseKind::Lane(call.gate_departure.lane)
            && call.gate_departure.crossed(from, to)
        {
            call.departed = true;
        }
        if matches!(call.leg, RailLeg::Outbound) && (from != to) {
            call.departed = true;
        }
    }

    pub(crate) fn mark_departed(&mut self, call: &CallManifest, now: u64) {
        let (stage, fragments) = if call.leg == RailLeg::Inbound {
            (FreightStage::InboundRail, &call.imports)
        } else {
            (FreightStage::OutboundRail, &call.exports)
        };
        for fragment in fragments {
            self.record(call.key.terminal, fragment, stage, None, now);
        }
    }

    pub(super) fn receive_imports(
        &mut self,
        call: &mut CallManifest,
        desk: &mut ImportDesk,
        now: u64,
    ) {
        for mut fragment in call.imports.drain(..) {
            let item = fragment.item.expect("rail imports have a SKU");
            if let Some(units) = desk.on_rail.get_mut(&item) {
                *units -= u64::from(fragment.units);
            }
            *desk.stock.entry(item).or_default() += u64::from(fragment.units);
            fragment.rail_received_at = Some(now);
            self.record(
                call.key.terminal,
                &fragment,
                FreightStage::TerminalStock,
                None,
                now,
            );
            self.stock
                .entry((call.key.terminal, item))
                .or_default()
                .push_back(fragment);
        }
        desk.on_rail.retain(|_, &mut units| units > 0);
    }

    fn split(&mut self, fragment: &mut FreightFragment, units: u32) -> FreightFragment {
        let mut part = fragment.clone();
        part.parent = Some(fragment.id);
        part.id = ShipmentId(self.next_id());
        self.active_records += 1;
        part.units = units;
        let total = fragment.booked.inner();
        // Divide toward zero and leave the remainder with the retained stock.
        let booked = (i128::from(total) * i128::from(units) / i128::from(fragment.units)) as i64;
        part.booked = Money::new_inner(booked);
        fragment.booked -= part.booked;
        fragment.units -= units;
        part
    }

    fn take(
        &mut self,
        queue: &mut VecDeque<FreightFragment>,
        mut units: u32,
    ) -> Vec<FreightFragment> {
        let mut fragments = Vec::new();
        while units > 0 {
            let Some(mut head) = queue.pop_front() else {
                break;
            };
            if units < head.units {
                fragments.push(self.split(&mut head, units));
                queue.push_front(head);
                break;
            }
            units -= head.units;
            fragments.push(head);
        }
        fragments
    }

    pub(crate) fn reserve_exports(&mut self, call: &mut CallManifest, waiting: u32) {
        let terminal = call.key.terminal;
        let available = self.unreserved_exports(terminal, waiting);
        let loaded = available.min(TRAIN_CAPACITY);
        let mut queued = self.exports.remove(&terminal).unwrap_or_default();
        let known = queued
            .iter()
            .map(|fragment| u64::from(fragment.units))
            .sum::<u64>();
        let unknown = bounded_transfer(u64::from(available).saturating_sub(known), loaded);
        if unknown > 0 {
            let mut fragment = self.unknown(None, unknown);
            fragment.call = Some(call.key);
            call.exports.push(fragment);
            self.active_records += 1;
        }
        for mut fragment in self.take(&mut queued, loaded - unknown) {
            fragment.call = Some(call.key);
            call.exports.push(fragment);
        }
        if !queued.is_empty() {
            self.exports.insert(terminal, queued);
        }
    }

    pub(crate) fn load_exports(&mut self, call: &mut CallManifest, now: u64) -> u32 {
        call.exports_loaded = true;
        for fragment in &call.exports {
            self.record(
                call.key.terminal,
                fragment,
                FreightStage::ExportLoaded,
                None,
                now,
            );
        }
        call.export_units()
    }

    pub(super) fn close_call(
        &mut self,
        call: CallManifest,
        mut desk: Option<&mut ImportDesk>,
        terminal_live: bool,
        failure: Option<FreightFailure>,
        now: u64,
    ) {
        let terminal = call.key.terminal;
        self.active_records -= call.imports.len();
        for fragment in call.imports {
            let stage = if call.departed || desk.is_none() {
                FreightStage::WrittenOff
            } else {
                FreightStage::Returned
            };
            self.record(terminal, &fragment, stage, None, now);
            self.history.back_mut().unwrap().failure = failure;
            if call.departed {
                if let Some(desk) = desk.as_deref_mut() {
                    // Close the physical commitment and the associated demand.
                    // Matching can place replacement orders on its next pass.
                    let item = fragment.item.unwrap();
                    if let Some(units) = desk.on_rail.get_mut(&item) {
                        *units -= u64::from(fragment.units);
                    }
                    let mut lost = fragment.units;
                    for order in desk.orders.iter_mut().filter(|order| {
                        Some(order.buyer) == fragment.ordered_for
                            && Some(order.ordered_at) == fragment.ordered_at
                            && order.quantity.item == item
                    }) {
                        let quantity = order.quantity.units.min(lost);
                        order.quantity.units -= quantity;
                        lost -= quantity;
                        if lost == 0 {
                            break;
                        }
                    }
                }
            }
        }
        if let Some(desk) = desk {
            desk.orders.retain(|order| order.quantity.units > 0);
            desk.on_rail.retain(|_, &mut units| units > 0);
        }
        if !call.exports_loaded && terminal_live {
            let mut known = Vec::new();
            for mut fragment in call.exports {
                if fragment.item.is_some() {
                    fragment.call = None;
                    known.push(fragment);
                } else {
                    self.active_records -= 1;
                }
            }
            let queued = self.exports.entry(terminal).or_default();
            queued.extend(known);
            queued
                .make_contiguous()
                .sort_by_key(|fragment| fragment.ledger);
        } else {
            self.active_records -= call.exports.len();
            for fragment in call.exports {
                let stage = if failure.is_none() {
                    FreightStage::Exited
                } else {
                    FreightStage::WrittenOff
                };
                self.record(terminal, &fragment, stage, None, now);
                self.history.back_mut().unwrap().failure = failure;
            }
        }
    }

    pub(crate) fn load_truck(
        &mut self,
        truck: VehicleID,
        terminal: Endpoint,
        buyer: Endpoint,
        items: &BTreeMap<ItemID, u32>,
        remaining: &BTreeMap<ItemID, u64>,
        lane: LaneID,
        position: Vec3,
        now: u64,
    ) {
        let load = LoadId(self.next_id());
        let mut fragments = Vec::new();
        for (&item, &units) in items {
            let mut stock = self.stock.remove(&(terminal, item)).unwrap_or_default();
            let known = stock
                .iter()
                .map(|fragment| u64::from(fragment.units))
                .sum::<u64>();
            let physical = remaining.get(&item).copied().unwrap_or(0) + u64::from(units);
            let unknown = bounded_transfer(physical.saturating_sub(known), units);
            if unknown > 0 {
                fragments.push(self.unknown(Some(item), unknown));
                self.active_records += 1;
            }
            fragments.extend(self.take(&mut stock, units - unknown));
            if !stock.is_empty() {
                self.stock.insert((terminal, item), stock);
            }
        }
        for fragment in &fragments {
            self.record(
                terminal,
                fragment,
                FreightStage::OutboundTruck,
                Some((truck, load, buyer)),
                now,
            );
        }
        self.trucks.insert(
            truck,
            TruckManifest {
                load,
                fragments,
                lane,
                position,
                arrived: false,
            },
        );
    }

    pub(crate) fn observe_truck_motion(
        &mut self,
        truck: VehicleID,
        kind: TraverseKind,
        from: Vec3,
        to: Vec3,
    ) {
        if let Some(manifest) = self.trucks.get_mut(&truck) {
            if kind == TraverseKind::Lane(manifest.lane)
                && from != to
                && to.distance(manifest.position) < POSITION_TOLERANCE
            {
                manifest.arrived = true;
            }
        }
    }

    pub(crate) fn truck_arrived(&self, truck: VehicleID) -> bool {
        // A legacy load has no provenance binding. Its live route and curb
        // position still have to pass the ordinary delivery check.
        self.trucks
            .get(&truck)
            .is_none_or(|manifest| manifest.arrived)
    }

    pub(crate) fn settle_truck(
        &mut self,
        truck: VehicleID,
        terminal: Endpoint,
        buyer: Endpoint,
        stage: FreightStage,
        totals: &BTreeMap<ItemID, Money>,
        now: u64,
    ) {
        let Some(TruckManifest {
            load,
            mut fragments,
            ..
        }) = self.trucks.remove(&truck)
        else {
            return;
        };
        if stage != FreightStage::Returned {
            self.active_records -= fragments.len();
        }
        let mut remaining = BTreeMap::<ItemID, (u32, i64)>::new();
        for fragment in &fragments {
            if let Some(item) = fragment.item {
                let entry = remaining
                    .entry(item)
                    .or_insert_with(|| (0, totals.get(&item).copied().unwrap_or_default().inner()));
                entry.0 += fragment.units;
            }
        }
        for mut fragment in fragments.drain(..) {
            if let Some(item) = fragment.item {
                if stage == FreightStage::Delivered {
                    let (units, money) = remaining.get_mut(&item).unwrap();
                    let booked = (i128::from(*money) * i128::from(fragment.units)
                        / i128::from(*units)) as i64;
                    *units -= fragment.units;
                    *money -= booked;
                    fragment.booked = Money::new_inner(booked);
                    fragment.ledger = Some(fragment.id);
                }
                if stage == FreightStage::Returned {
                    self.stock
                        .entry((terminal, item))
                        .or_default()
                        .push_back(fragment.clone());
                }
            }
            self.record(terminal, &fragment, stage, Some((truck, load, buyer)), now);
            if stage != FreightStage::Delivered {
                self.history.back_mut().unwrap().failure = Some(FreightFailure::TruckInterrupted);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::economy::cargo::{ImportOrder, ItemQuantity};
    use crate::SoulID;

    #[test]
    fn p8_freight_berth_at_lane_start_uses_the_connecting_turn() {
        use crate::map::{IntersectionID, TurnID};
        use slotmapd::SlotMap;

        let mut lanes = SlotMap::with_key();
        let inbound = lanes.insert(());
        let outgoing = lanes.insert(());
        let other = lanes.insert(());
        let anchor = RailAnchor {
            lane: outgoing,
            offset: 0.0,
            position: Vec3::X,
            dir: Vec3::X,
        };
        let connected = TraverseKind::Turn(TurnID::new(
            IntersectionID::default(),
            inbound,
            outgoing,
            false,
        ));
        let wrong_exit = TraverseKind::Turn(TurnID::new(
            IntersectionID::default(),
            inbound,
            other,
            false,
        ));
        assert!(anchor.reached(connected, Vec3::ZERO, Vec3::X));
        assert!(!anchor.reached(connected, Vec3::X, Vec3::X));
        assert!(!anchor.reached(wrong_exit, Vec3::ZERO, Vec3::X));
        assert!(!anchor.reached(connected, Vec3::Z, Vec3::X + Vec3::Z));
        let interior = RailAnchor {
            offset: 5.0,
            ..anchor
        };
        assert!(!interior.reached(connected, Vec3::ZERO, Vec3::X));
    }

    #[test]
    fn p8_freight_two_calls_keep_late_orders_out_and_trace_fragments_to_the_buyer() {
        use slotmapd::SlotMap;

        drop(crate::tests::TestCtx::new_fast());
        let terminal = Endpoint {
            soul: SoulID::FreightStation(SlotMap::with_key().insert(())),
            building: SlotMap::with_key().insert(()),
        };
        let mut buyers = SlotMap::with_key();
        let mut buildings = SlotMap::with_key();
        let buyer = Endpoint {
            soul: SoulID::GoodsCompany(buyers.insert(())),
            building: buildings.insert(()),
        };
        let other = Endpoint {
            soul: SoulID::GoodsCompany(buyers.insert(())),
            building: buildings.insert(()),
        };
        let item = ItemID::new("goods");
        let anchor = RailAnchor {
            lane: LaneID::default(),
            offset: 10.0,
            position: Vec3::X * 10.0,
            dir: Vec3::X,
        };
        let mut trains = SlotMap::with_key();
        let mut trace = FreightTrace::default();
        let mut desk = ImportDesk::default();
        let mut call = |epoch| CallManifest {
            key: FreightCallKey {
                terminal,
                train: trains.insert(()),
                epoch,
                outside_gate: BuildingID::default(),
            },
            gate: anchor,
            gate_departure: anchor,
            berth: anchor,
            leg: RailLeg::Inbound,
            departed: false,
            reached: false,
            departure_recorded: false,
            retry_at: 0,
            last_motion: 0,
            imports: Vec::new(),
            exports: Vec::new(),
            exports_loaded: false,
        };
        desk.place(ImportOrder {
            buyer,
            quantity: ItemQuantity { item, units: 60 },
            ordered_at: 1,
        });
        let mut first = call(trace.next_id());
        trace.reserve_imports(&mut first, &desk, 2);
        let origin = first.imports[0].id;
        trace.calls.insert(first.key.train, first);

        desk.place(ImportOrder {
            buyer: other,
            quantity: ItemQuantity { item, units: 75 },
            ordered_at: 3,
        });
        let mut second = call(trace.next_id());
        trace.reserve_imports(&mut second, &desk, 4);
        assert_eq!(
            second.imports.iter().map(|line| line.units).sum::<u32>(),
            75
        );
        assert_eq!(second.imports[0].ordered_for, Some(other));

        let first_train = trace.calls.keys().next().copied().unwrap();
        let mut first = trace.calls.remove(&first_train).unwrap();
        trace.receive_imports(&mut first, &mut desk, 5);
        trace.receive_imports(&mut first, &mut desk, 6);
        assert_eq!(desk.stock[&item], 60);
        assert_eq!(desk.on_rail[&item], 75);

        // Stockout priority can select another live buyer. The cargo still
        // carries the actual rail call and the original demand attribution.
        let truck = SlotMap::with_key().insert(());
        let remaining = BTreeMap::from([(item, 12)]);
        trace.load_truck(
            truck,
            terminal,
            other,
            &BTreeMap::from([(item, 48)]),
            &remaining,
            anchor.lane,
            anchor.position,
            7,
        );
        assert!(!trace.truck_arrived(truck));
        trace.observe_truck_motion(
            truck,
            TraverseKind::Lane(anchor.lane),
            anchor.position + Vec3::Z,
            anchor.position + Vec3::Z,
        );
        assert!(!trace.truck_arrived(truck));
        trace.observe_truck_motion(
            truck,
            TraverseKind::Lane(anchor.lane),
            anchor.position - Vec3::X,
            anchor.position,
        );
        assert!(trace.truck_arrived(truck));
        trace.settle_truck(
            truck,
            terminal,
            other,
            FreightStage::Delivered,
            &BTreeMap::from([(item, Money::new_inner(-123))]),
            8,
        );
        trace.settle_truck(
            truck,
            terminal,
            other,
            FreightStage::Delivered,
            &BTreeMap::new(),
            9,
        );
        trace.calls.insert(second.key.train, second);
        trace.rebuild_indexes().unwrap();
        let SoulID::GoodsCompany(owner) = other.soul else {
            unreachable!()
        };
        let delivered = trace.trace_demand(DemandKey {
            owner,
            building: other.building,
            item,
        });
        let row = delivered
            .rows
            .iter()
            .find(|row| row.stage == FreightStage::Delivered)
            .unwrap();
        assert_eq!(row.fragment.units, 48);
        assert_eq!(row.fragment.parent, Some(origin));
        assert_eq!(row.fragment.call, Some(first.key));
        assert_eq!(row.fragment.ordered_for, Some(buyer));
        assert_eq!(row.fragment.rail_received_at, Some(5));
        assert_eq!(row.fragment.booked, Money::new_inner(-123));
        assert_eq!(row.receiver, Some(other));
        assert_eq!(row.truck, Some(truck));
        assert!(row.load.is_some());
    }
}
