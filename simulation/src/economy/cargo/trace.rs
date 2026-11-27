use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};

use geom::Vec3;
use prototypes::{ItemID, Money, Tick};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use super::{
    bounded_transfer, CargoBook, Endpoint, ImportDesk, ImportOrderKey, LoadId, ShipmentId,
    TRAIN_CAPACITY,
};
use crate::map::{BuildingID, LaneID, LaneKind, Map, TraverseKind};
use crate::map_dynamic::Itinerary;
use crate::{CompanyID, TrainID, VehicleID};

const POSITION_TOLERANCE: f32 = 0.04;
const MAX_HISTORY: usize = 65_536;
const MAX_ACTIVE: usize = 65_536;
// Leave room for imports after export admission stops, and for splits and
// legacy stock fragments after both kinds of admission stop.
const EXPORT_ADMISSION_LIMIT: usize = MAX_ACTIVE / 2;
const IMPORT_ADMISSION_LIMIT: usize = 3 * MAX_ACTIVE / 4;
const RETAIN_TICKS: u64 = 7 * super::TICKS_PER_BUDGET_DAY;

thread_local! {
    static DECODE_FRAGMENTS: Cell<Option<usize>> = const { Cell::new(None) };
}

fn bound_collection<E: de::Error>(count: usize, max: usize) -> Result<(), E> {
    if count > max {
        return Err(E::custom("freight collection exceeds its capacity"));
    }
    Ok(())
}

fn charge_fragments<E: de::Error>(count: usize) -> Result<(), E> {
    DECODE_FRAGMENTS.with(|budget| {
        if let Some(remaining) = budget.get() {
            bound_collection(count, remaining)?;
            budget.set(Some(remaining - count));
        }
        Ok(())
    })
}

fn bounded_sequence<'de, D, T, C, const MAX: usize, const ACTIVE: bool>(d: D) -> Result<C, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
    C: FromIterator<T>,
{
    struct Sequence<T, C, const MAX: usize, const ACTIVE: bool>(PhantomData<(T, C)>);

    impl<'de, T, C, const MAX: usize, const ACTIVE: bool> Visitor<'de> for Sequence<T, C, MAX, ACTIVE>
    where
        T: Deserialize<'de>,
        C: FromIterator<T>,
    {
        type Value = C;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "a freight collection of at most {MAX} entries")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<C, A::Error> {
            let declared = seq.size_hint().unwrap_or(0);
            bound_collection(declared, MAX)?;
            if ACTIVE {
                charge_fragments(declared)?;
            }
            let mut entries = Vec::with_capacity(declared);
            while let Some(entry) = seq.next_element()? {
                bound_collection(entries.len() + 1, MAX)?;
                if ACTIVE && entries.len() >= declared {
                    charge_fragments(1)?;
                }
                entries.push(entry);
            }
            Ok(entries.into_iter().collect())
        }
    }

    d.deserialize_seq(Sequence::<T, C, MAX, ACTIVE>(PhantomData))
}

fn bounded_map<'de, D, K, V>(d: D) -> Result<BTreeMap<K, V>, D::Error>
where
    D: Deserializer<'de>,
    K: Deserialize<'de> + Ord,
    V: Deserialize<'de>,
{
    struct Entries<K, V>(PhantomData<(K, V)>);

    impl<'de, K, V> Visitor<'de> for Entries<K, V>
    where
        K: Deserialize<'de> + Ord,
        V: Deserialize<'de>,
    {
        type Value = BTreeMap<K, V>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "a freight map of at most {MAX_ACTIVE} entries")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            bound_collection(map.size_hint().unwrap_or(0), MAX_ACTIVE)?;
            let mut entries = BTreeMap::new();
            while let Some(key) = map.next_key()? {
                bound_collection(entries.len() + 1, MAX_ACTIVE)?;
                if entries.insert(key, map.next_value()?).is_some() {
                    return Err(de::Error::custom("duplicate freight map key"));
                }
            }
            Ok(entries)
        }
    }

    d.deserialize_map(Entries(PhantomData))
}

fn train_fragments<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<FreightFragment>, D::Error> {
    bounded_sequence::<D, _, _, { TRAIN_CAPACITY as usize }, true>(d)
}

fn truck_fragments<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<FreightFragment>, D::Error> {
    bounded_sequence::<D, _, _, { super::STATION_TRUCK_CAPACITY as usize }, true>(d)
}

fn freight_history<'de, D: Deserializer<'de>>(d: D) -> Result<VecDeque<FreightRecord>, D::Error> {
    bounded_sequence::<D, _, _, MAX_HISTORY, false>(d)
}

fn active_queue<'de, D: Deserializer<'de>>(d: D) -> Result<VecDeque<FreightFragment>, D::Error> {
    bounded_sequence::<D, _, _, MAX_ACTIVE, true>(d)
}

fn fragment_queues<'de, D, K>(d: D) -> Result<BTreeMap<K, VecDeque<FreightFragment>>, D::Error>
where
    D: Deserializer<'de>,
    K: Deserialize<'de> + Ord,
{
    #[derive(Deserialize)]
    struct FragmentQueue(#[serde(deserialize_with = "active_queue")] VecDeque<FreightFragment>);

    let entries = bounded_map::<D, K, FragmentQueue>(d)?;
    Ok(entries
        .into_iter()
        .map(|(key, queue)| (key, queue.0))
        .collect())
}

#[derive(Serialize, Deserialize)]
pub(crate) struct FreightSnapshotTail {
    pub version: u8,
    #[serde(deserialize_with = "bounded_map")]
    pub legacy_exports: BTreeMap<Endpoint, u32>,
    #[serde(deserialize_with = "bounded_map")]
    pub dispatch_retries: BTreeMap<Endpoint, u64>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) struct RailAnchor {
    pub lane: LaneID,
    offset: f32,
    pub position: Vec3,
    dir: Vec3,
}

impl RailAnchor {
    pub fn near(map: &Map, hint: Vec3) -> Option<Self> {
        const SNAP_RADIUS: f32 = 50.0;
        let lane = map.nearest_lane(hint, LaneKind::Rail, Some(SNAP_RADIUS))?;
        let track = &map.get(lane)?.points;
        let offset = track.length_at_proj(track.project(hint));
        let (position, dir) = track.point_dir_along(offset);
        // Terminals and outside markets are sited on the ground beside or
        // below the rails. Measure reach to the actual track in three dimensions.
        (position.distance2(hint) <= SNAP_RADIUS * SNAP_RADIUS).then_some(Self {
            lane,
            offset,
            position,
            dir,
        })
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

    fn at_offset(self, map: &Map, offset: f32) -> Option<Self> {
        let track = &map.get(self.lane)?.points;
        if !(0.0..=track.length()).contains(&offset) {
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

    pub fn clearance(self, map: &Map, length: f32) -> Option<Self> {
        self.at_offset(map, self.offset.max(length + 0.1))
    }

    pub fn departure_point(self, map: &Map, clearance: Self) -> Option<Self> {
        let track = &map.get(self.lane)?.points;
        let offset = (self.offset + 40.0)
            .max(clearance.offset + 1.0)
            .min(track.length() - 0.1);
        (offset > clearance.offset)
            .then(|| self.at_offset(map, offset))
            .flatten()
    }

    pub fn at(self, position: Vec3) -> bool {
        position.distance(self.position) <= POSITION_TOLERANCE
    }

    pub fn before(self, position: Vec3) -> bool {
        (position - self.position).dot(self.dir) < 0.0
    }

    fn crossed(self, from: Vec3, to: Vec3) -> bool {
        from.distance(self.position) < 10.0 && self.before(from) && !self.before(to)
    }

    fn passed(self, traversal: TraverseKind, from: Vec3, to: Vec3) -> bool {
        match traversal {
            TraverseKind::Lane(lane) if self.lane == lane => {
                (self.crossed(from, to) || (self.at(from) && !self.before(to))) && from != to
            }
            _ => self.reached(traversal, from, to),
        }
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

/// The current leg and shipment of an assigned freight train.
pub struct RailShipment<'a> {
    pub outside_gate: BuildingID,
    pub destination: BuildingID,
    pub fragments: &'a [FreightFragment],
    pub reserved: bool,
}

impl FreightFragment {
    fn order_key(&self) -> Option<ImportOrderKey> {
        Some(ImportOrderKey {
            buyer: self.ordered_for?,
            item: self.item?,
            opened_at: self.ordered_at?,
        })
    }
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
    /// Movement was admitted before the train crossed the departure marker.
    InboundStarted,
}

impl RailLeg {
    pub fn inbound(self) -> bool {
        matches!(self, Self::Inbound | Self::InboundStarted)
    }
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
    #[serde(deserialize_with = "train_fragments")]
    pub imports: Vec<FreightFragment>,
    #[serde(deserialize_with = "train_fragments")]
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
    #[serde(deserialize_with = "truck_fragments")]
    fragments: Vec<FreightFragment>,
    lane: LaneID,
    position: Vec3,
    arrived: bool,
}

#[derive(Debug)]
struct FreightSession(u64);

impl Default for FreightSession {
    fn default() -> Self {
        static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);
        Self(NEXT_SESSION.fetch_add(1, Ordering::Relaxed))
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct FreightTrace {
    next_id: u64,
    revision: u64,
    #[serde(deserialize_with = "bounded_map")]
    pub(crate) policies: BTreeMap<Endpoint, super::TerminalPolicyView>,
    #[serde(deserialize_with = "bounded_map")]
    pub(crate) depots: BTreeMap<CompanyID, super::DepotRestockView>,
    #[serde(deserialize_with = "bounded_map")]
    pub(crate) calls: BTreeMap<TrainID, CallManifest>,
    #[serde(deserialize_with = "fragment_queues")]
    stock: BTreeMap<(Endpoint, ItemID), VecDeque<FreightFragment>>,
    #[serde(deserialize_with = "fragment_queues")]
    exports: BTreeMap<Endpoint, VecDeque<FreightFragment>>,
    #[serde(deserialize_with = "bounded_map")]
    trucks: BTreeMap<VehicleID, TruckManifest>,
    #[serde(deserialize_with = "freight_history")]
    history: VecDeque<FreightRecord>,
    omitted: u64,
    active_records: usize,
    /// Export loads and accepted market settlements own their space up to handoff.
    #[serde(skip)]
    export_credits: usize,
    #[serde(skip)]
    pending_exports: BTreeMap<Endpoint, u64>,
    #[serde(skip)]
    pub(crate) measured_rail: bool,
    #[serde(skip)]
    pub(crate) saved_legacy_exports: Option<BTreeMap<Endpoint, u32>>,
    #[serde(skip)]
    pub(crate) dispatch_retries: BTreeMap<Endpoint, u64>,
    #[serde(skip)]
    session: FreightSession,
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
    session: u64,
    query: FreightQuery,
    first: u64,
    last: u64,
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
    pub(crate) fn decode(reader: &mut &[u8]) -> std::io::Result<Self> {
        use common::saveload::{Bincode, Encoder};

        struct Budget(Option<usize>);
        impl Drop for Budget {
            fn drop(&mut self) {
                DECODE_FRAGMENTS.with(|budget| budget.set(self.0));
            }
        }
        let _budget = Budget(DECODE_FRAGMENTS.with(|budget| budget.replace(Some(MAX_ACTIVE))));
        Bincode::decode_reader(reader)
    }

    pub(crate) fn is_pristine(&self) -> bool {
        self.next_id == 0
            && self.policies.is_empty()
            && self.depots.is_empty()
            && self.dispatch_retries.is_empty()
    }

    pub(crate) fn remove_terminal(&mut self, terminal: Endpoint, now: u64) {
        self.policies.remove(&terminal);
        self.dispatch_retries.remove(&terminal);
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

    pub(crate) fn dispatch_retry_due(&self, terminal: Endpoint, now: u64) -> bool {
        self.dispatch_retries
            .get(&terminal)
            .is_none_or(|&when| now >= when)
    }

    pub(crate) fn defer_dispatch(&mut self, terminal: Endpoint, when: u64) {
        self.dispatch_retries.insert(terminal, when);
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
        match cursor {
            Some(cursor)
                if cursor.session != self.session.0
                    || cursor.query != query
                    || cursor.first < self.omitted =>
            {
                return Err(StaleFreightCursor);
            }
            _ => {}
        }
        let mut rows = Vec::new();
        let mut next = None;
        if let Some(ids) = self.by_query.get(&query) {
            let (start, end) = match cursor {
                Some(cursor) => (
                    ids.partition_point(|&id| id < cursor.first),
                    ids.partition_point(|&id| id <= cursor.last),
                ),
                None => (ids.len().saturating_sub(ROWS), ids.len()),
            };
            rows.extend(
                (start..end)
                    .rev()
                    .map(|idx| self.history[(ids[idx] - self.omitted) as usize].clone()),
            );
            if start > 0 {
                next = Some(FreightCursor {
                    session: self.session.0,
                    query,
                    first: ids[start.saturating_sub(ROWS)],
                    last: ids[start - 1],
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
                    .parent
                    .is_some_and(|parent| parent.0 == 0 || parent >= line.id)
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

    fn export_balances(&self) -> BTreeMap<Endpoint, u64> {
        let mut balances = BTreeMap::new();
        for (&terminal, queued) in &self.exports {
            *balances.entry(terminal).or_default() +=
                queued.iter().map(|line| u64::from(line.units)).sum::<u64>();
        }
        for call in self.calls.values().filter(|call| !call.exports_loaded) {
            *balances.entry(call.key.terminal).or_default() += u64::from(call.export_units());
        }
        balances
    }

    pub(crate) fn legacy_export_balances(&self, world: &crate::World) -> BTreeMap<Endpoint, u32> {
        let traced = self.export_balances();
        world
            .freight_stations
            .iter()
            .filter_map(|(id, station)| {
                let terminal = Endpoint {
                    soul: crate::SoulID::FreightStation(id),
                    building: station.f.building,
                };
                let unknown = u64::from(station.f.waiting_cargo)
                    .saturating_sub(traced.get(&terminal).copied().unwrap_or(0))
                    as u32;
                (unknown != 0).then_some((terminal, unknown))
            })
            .collect()
    }

    pub(crate) fn validate_ownership(
        &self,
        book: &CargoBook,
        world: &crate::World,
    ) -> Result<(), &'static str> {
        use crate::souls::freight_station::FreightTrainState;

        let owner = |terminal: Endpoint| {
            let crate::SoulID::FreightStation(id) = terminal.soul else {
                return None;
            };
            let station = &world.freight_stations.get(id)?.f;
            (station.building == terminal.building).then_some(station)
        };
        let mut assigned = BTreeMap::new();
        for (id, station) in world.freight_stations.iter() {
            if station.f.trains.len() > 2 {
                return Err("too many train assignments for freight terminal");
            }
            for &(train, state) in &station.f.trains {
                let terminal = Endpoint {
                    soul: crate::SoulID::FreightStation(id),
                    building: station.f.building,
                };
                if world.trains.get(train).is_none()
                    || assigned.insert(train, (terminal, state)).is_some()
                {
                    return Err("invalid or duplicate freight train assignment");
                }
            }
        }
        let mut export_ledger = BTreeMap::new();
        for row in self
            .history
            .iter()
            .filter(|row| row.stage == FreightStage::ExportStaged)
        {
            if row.fragment.ledger != Some(row.fragment.id)
                || export_ledger.insert(row.fragment.id, (row, 0u64, 0u128)).is_some()
            {
                return Err("invalid retained export ledger root");
            }
        }
        let mut valid_export = |terminal: Endpoint, line: &FreightFragment| match line.item {
            Some(_) => {
                line.source.is_some()
                    && line
                        .road_load
                        .is_none_or(|load| load.0 > 0 && load.0 <= book.next_id)
                    && line.ledger.is_some_and(|id| {
                        if id.0 == 0 || id > line.id {
                            return false;
                        }
                        let Some((root, units, booked)) = export_ledger.get_mut(&id) else {
                            return self.omitted > 0;
                        };
                        let root_booked = root.fragment.booked.inner();
                        let line_booked = line.booked.inner();
                        if root.terminal != terminal
                            || root.fragment.item != line.item
                            || root.fragment.source != line.source
                            || root.fragment.road_load != line.road_load
                            || (line_booked != 0 && line_booked.signum() != root_booked.signum())
                        {
                            return false;
                        }
                        *units += u64::from(line.units);
                        *booked += u128::from(line_booked.unsigned_abs());
                        let root_units = u64::from(root.fragment.units);
                        let root_amount = u128::from(root_booked.unsigned_abs());
                        *units <= root_units
                            && *booked <= root_amount
                            && (*units < root_units || *booked == root_amount)
                    })
            }
            None => {
                line.source.is_none()
                    && line.ledger.is_none()
                    && line.road_load.is_none()
                    && line.booked == Money::ZERO
            }
        };
        for (&terminal, queue) in &self.exports {
            if owner(terminal).is_none()
                || queue
                    .iter()
                    .any(|line| line.call.is_some() || !valid_export(terminal, line))
            {
                return Err("invalid staged export owner or ledger");
            }
        }
        let mut exported = self.export_balances();
        if let Some(legacy) = &self.saved_legacy_exports {
            for (&terminal, &units) in legacy {
                *exported.entry(terminal).or_default() += u64::from(units);
            }
        }
        for (&terminal, &units) in &exported {
            if owner(terminal).is_none_or(|station| units > u64::from(station.waiting_cargo)) {
                return Err("traced exports exceed the station balance");
            }
        }
        if self.saved_legacy_exports.is_some()
            && world.freight_stations.iter().any(|(id, station)| {
                let endpoint = Endpoint {
                    soul: crate::SoulID::FreightStation(id),
                    building: station.f.building,
                };
                exported.get(&endpoint).copied().unwrap_or(0) != u64::from(station.f.waiting_cargo)
            })
        {
            return Err("export balances disagree with the station counters");
        }

        let mut reserved = BTreeMap::<(Endpoint, ItemID), u64>::new();
        for call in self.calls.values() {
            let matches_state = |state| match call.leg {
                RailLeg::Positioning | RailLeg::Inbound | RailLeg::InboundStarted => {
                    matches!(state, FreightTrainState::Arriving)
                }
                RailLeg::Loading { .. } => matches!(state, FreightTrainState::Loading),
                RailLeg::Outbound => matches!(state, FreightTrainState::Moving),
            };
            if !assigned
                .get(&call.key.train)
                .is_some_and(|&(terminal, state)| {
                    terminal == call.key.terminal && matches_state(state)
                })
                || call.key.epoch == 0
                || call.key.epoch > self.next_id
                || call.exports_loaded
                    != matches!(call.leg, RailLeg::Loading { .. } | RailLeg::Outbound)
                || (call.exports_loaded && !call.imports.is_empty())
                || call.exports.iter().any(|line| {
                    line.call != Some(call.key) || !valid_export(call.key.terminal, line)
                })
            {
                return Err("freight call disagrees with its assignment or cargo ledger");
            }
            for line in &call.imports {
                let Some(item) = line.item else {
                    return Err("an import has no item");
                };
                if line.call != Some(call.key)
                    || line.ledger.is_some()
                    || line.booked != Money::ZERO
                {
                    return Err("invalid import reservation owner or ledger");
                }
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
            if owner(terminal).is_none()
                || fragments.iter().any(|line| {
                    line.item != Some(item)
                        || line.call.is_some_and(|call| call.terminal != terminal)
                })
            {
                return Err("invalid terminal stock ownership");
            }
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
            let Some((terminal, trip)) = book
                .station_trucks
                .get(&truck)
                .and_then(|fleet| Some((fleet.station, fleet.trip.as_ref()?)))
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
                || owner(terminal).is_none()
                || manifest.load.0 == 0
                || manifest.load.0 > self.next_id
                || manifest
                    .fragments
                    .iter()
                    .any(|line| line.call.is_some_and(|call| call.terminal != terminal))
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
        self.terminal_policy(endpoint).policy.admit_exports
            && self.record_usage() < EXPORT_ADMISSION_LIMIT
    }

    fn record_usage(&self) -> usize {
        self.active_records + self.export_credits
    }

    fn free_records(&self) -> usize {
        MAX_ACTIVE.saturating_sub(self.record_usage())
    }

    pub(super) fn import_record_credit(&self) -> usize {
        IMPORT_ADMISSION_LIMIT.saturating_sub(self.record_usage())
    }

    pub(crate) fn export_units_fit(&self, terminal: Endpoint, waiting: u32, units: u64) -> bool {
        u64::from(waiting) + self.pending_exports.get(&terminal).copied().unwrap_or(0) + units
            <= u64::from(u32::MAX)
    }

    pub(crate) fn reserve_export_space(
        &mut self,
        terminal: Endpoint,
        waiting: u32,
        units: u64,
        records: usize,
    ) -> bool {
        if records == 0
            || units == 0
            || records > EXPORT_ADMISSION_LIMIT.saturating_sub(self.record_usage())
            || !self.export_units_fit(terminal, waiting, units)
        {
            return false;
        }
        self.add_export_credit(terminal, units, records);
        true
    }

    fn add_export_credit(&mut self, terminal: Endpoint, units: u64, records: usize) {
        self.export_credits += records;
        *self.pending_exports.entry(terminal).or_default() += units;
    }

    pub(super) fn release_export_space(&mut self, terminal: Endpoint, units: u64, records: usize) {
        self.export_credits -= records;
        let pending = self
            .pending_exports
            .get_mut(&terminal)
            .expect("export space was reserved");
        *pending -= units;
        if *pending == 0 {
            self.pending_exports.remove(&terminal);
        }
    }

    pub(crate) fn restore_export_space(
        &mut self,
        book: &CargoBook,
        world: &crate::World,
    ) -> Result<(), &'static str> {
        for load in book.loads.values() {
            let terminal = load.destination;
            let crate::SoulID::FreightStation(id) = terminal.soul else {
                continue;
            };
            let Some(station) = world
                .freight_stations
                .get(id)
                .filter(|entity| entity.f.building == terminal.building)
            else {
                return Err("road export has no terminal owner");
            };
            let units = load.items.iter().map(|line| u64::from(line.units)).sum();
            let records = load.items.len();
            if units == 0
                || load.items.iter().any(|line| line.units == 0)
                || records > self.free_records()
                || !self.export_units_fit(terminal, station.f.waiting_cargo, units)
            {
                return Err("road export settlement exceeds freight capacity");
            }
            self.add_export_credit(terminal, units, records);
        }
        Ok(())
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

    pub fn train_dispatch_held(&self, train: TrainID) -> bool {
        self.calls.get(&train).is_some_and(|call| self.held(call))
    }

    pub fn rail_shipment(&self, train: TrainID) -> Option<RailShipment<'_>> {
        let call = self.calls.get(&train)?;
        let outside_gate = call.key.outside_gate;
        Some(RailShipment {
            outside_gate,
            destination: if call.leg.inbound() {
                call.key.terminal.building
            } else {
                outside_gate
            },
            fragments: if call.exports_loaded {
                &call.exports
            } else {
                &call.imports
            },
            reserved: !call.exports_loaded && !call.departed && !call.imports.is_empty(),
        })
    }

    /// Use the current receiver because pooled imports can go to a different
    /// buyer than the one that originally ordered the rail shipment.
    pub fn truck_shipment(
        &self,
        cargo: &CargoBook,
        truck: VehicleID,
    ) -> Option<(Endpoint, &[FreightFragment])> {
        let manifest = self.trucks.get(&truck)?;
        let trip = cargo.station_trucks.get(&truck)?.trip.as_ref()?;
        (!trip.returning).then_some((trip.buyer, &manifest.fragments))
    }

    pub(crate) fn may_move(&self, train: TrainID) -> bool {
        !self.train_dispatch_held(train)
    }

    pub(crate) fn held(&self, call: &CallManifest) -> bool {
        call.leg == RailLeg::Inbound && !call.departed && !self.may_start(call)
    }

    fn may_start(&self, call: &CallManifest) -> bool {
        let prior = self
            .calls
            .values()
            .filter(|other| {
                other.key.terminal == call.key.terminal
                    && other.key.train != call.key.train
                    && (other.departed
                        || matches!(
                            other.leg,
                            RailLeg::InboundStarted | RailLeg::Loading { .. } | RailLeg::Outbound
                        )
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
        self.dispatch_retries.retain(|_, when| now < *when);
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
        self.release_export_space(terminal, trade.qty as u64, 1);
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

    pub(super) fn detach_closed_orders(&mut self, terminal: Endpoint, desk: &ImportDesk) {
        let open: BTreeSet<_> = desk.orders.iter().map(|order| order.key()).collect();
        let lines = self
            .calls
            .values_mut()
            .filter(|call| call.key.terminal == terminal)
            .flat_map(|call| &mut call.imports)
            .chain(
                self.stock
                    .iter_mut()
                    .filter(|((owner, _), _)| *owner == terminal)
                    .flat_map(|(_, queue)| queue),
            );
        for line in lines {
            if line.order_key().is_some_and(|key| !open.contains(&key)) {
                line.ordered_for = None;
                line.ordered_at = None;
            }
        }
    }

    pub(super) fn reserve_imports(&mut self, call: &mut CallManifest, desk: &ImportDesk, now: u64) {
        self.detach_closed_orders(call.key.terminal, desk);
        let terminal = call.key.terminal;
        let mut coverage = BTreeMap::<ImportOrderKey, u64>::new();
        let reserved = self
            .calls
            .values()
            .filter(|call| call.key.terminal == terminal)
            .flat_map(|call| &call.imports)
            .chain(
                self.stock
                    .iter()
                    .filter(|((owner, _), _)| *owner == terminal)
                    .flat_map(|(_, queue)| queue),
            );
        for line in reserved {
            if let Some(key) = line.order_key() {
                *coverage.entry(key).or_default() += u64::from(line.units);
            }
        }
        let available = self.available_imports(call.key.terminal, desk);
        let credit = self.import_record_credit();
        let mut room = TRAIN_CAPACITY.min(credit as u32);
        for (item, unassigned) in available {
            let mut units = bounded_transfer(unassigned, room);
            if units == 0 {
                continue;
            }
            room -= units;
            for order in desk
                .orders
                .iter()
                .filter(|order| order.quantity.item == item)
            {
                let key = order.key();
                let covered = coverage.entry(key).or_default();
                let skipped = bounded_transfer(*covered, order.quantity.units);
                *covered -= u64::from(skipped);
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
        if call.leg == RailLeg::Inbound {
            call.leg = RailLeg::InboundStarted;
        }
        call.reached |= match call.leg {
            RailLeg::Positioning => call.gate.passed(traversal, from, to),
            RailLeg::Inbound | RailLeg::InboundStarted => call.berth.reached(traversal, from, to),
            RailLeg::Outbound => call.gate.passed(traversal, from, to),
            RailLeg::Loading { .. } => return,
        };
        if call.leg.inbound()
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
        let (stage, fragments) = if call.leg.inbound() {
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
                if self.free_records() != 0 {
                    fragments.push(self.split(&mut head, units));
                }
                queue.push_front(head);
                break;
            }
            units -= head.units;
            fragments.push(head);
        }
        fragments
    }

    fn requeue(queue: &mut VecDeque<FreightFragment>, fragment: FreightFragment) -> bool {
        // Cancelled splits can rejoin the stock they came from. The return
        // event keeps the split ID in the retained history.
        if let Some(parent) = fragment.parent.and_then(|id| {
            queue.iter_mut().find(|line| {
                line.id == id
                    && line.item == fragment.item
                    && line.call == fragment.call
                    && line.order_key() == fragment.order_key()
                    && line.ledger == fragment.ledger
            })
        }) {
            parent.units += fragment.units;
            parent.booked += fragment.booked;
            return true;
        }
        queue.push_back(fragment);
        false
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
        let materialized = if self.free_records() > 0 { unknown } else { 0 };
        if materialized > 0 {
            let mut fragment = self.unknown(None, materialized);
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
                    for order in desk
                        .orders
                        .iter_mut()
                        .filter(|order| Some(order.key()) == fragment.order_key())
                    {
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
            let mut required = BTreeMap::<ItemID, u64>::new();
            for order in &desk.orders {
                *required.entry(order.quantity.item).or_default() +=
                    u64::from(order.quantity.units);
            }
            for (item, units) in required {
                let needed = units.saturating_sub(desk.stock.get(&item).copied().unwrap_or(0));
                if needed > 0 {
                    let pending = desk.on_rail.entry(item).or_default();
                    *pending = (*pending).max(needed);
                }
            }
            self.detach_closed_orders(terminal, desk);
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
            for fragment in known {
                self.active_records -= usize::from(Self::requeue(queued, fragment));
            }
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

    pub(super) fn can_load_truck(
        &self,
        terminal: Endpoint,
        items: &BTreeMap<ItemID, u32>,
        physical: &BTreeMap<ItemID, u64>,
    ) -> bool {
        if self.free_records() >= items.len() * 2 {
            // Each SKU needs at most an anonymous fragment and a split.
            return true;
        }
        let records = items
            .iter()
            .map(|(&item, &units)| {
                let queue = self.stock.get(&(terminal, item));
                let known = queue
                    .into_iter()
                    .flatten()
                    .map(|line| u64::from(line.units))
                    .sum::<u64>();
                let unknown = bounded_transfer(physical[&item].saturating_sub(known), units);
                let mut remaining = units - unknown;
                let mut split = false;
                for line in queue.into_iter().flatten() {
                    if remaining < line.units {
                        split = remaining > 0;
                        break;
                    }
                    remaining -= line.units;
                }
                usize::from(unknown != 0) + usize::from(split)
            })
            .sum::<usize>();
        records <= self.free_records()
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
                    let queue = self.stock.entry((terminal, item)).or_default();
                    self.active_records -= usize::from(Self::requeue(queue, fragment.clone()));
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
    fn p8_fragment_splits_stop_at_the_active_limit() {
        let mut trace = FreightTrace {
            active_records: MAX_ACTIVE,
            ..FreightTrace::default()
        };
        let mut queue = VecDeque::from([trace.unknown(None, 10)]);
        assert!(trace.take(&mut queue, 5).is_empty());
        assert_eq!(trace.active_records, MAX_ACTIVE);
        assert_eq!(queue[0].units, 10);
    }

    #[test]
    fn p8_fragment_decoder_rejects_oversized_collections_before_reading_entries() {
        use common::saveload::{Bincode, Encoder};

        let bytes = Bincode::encode(&(0u64, 0u64, MAX_ACTIVE + 1)).unwrap();
        let error = Bincode::decode::<FreightTrace>(&bytes).unwrap_err();
        assert!(
            error.to_string().contains("freight collection exceeds"),
            "{error}"
        );
    }

    #[test]
    fn p8_export_settlement_owns_its_slot_through_a_terminal_hold() {
        use common::saveload::{Bincode, Encoder};

        drop(crate::tests::TestCtx::new_fast());
        let terminal = Endpoint {
            soul: SoulID::FreightStation(crate::FreightStationID::default()),
            building: BuildingID::default(),
        };
        let item = ItemID::new("coal");
        let trade = super::super::Trade {
            buyer: super::super::TradeTarget(terminal.soul),
            seller: super::super::TradeTarget(SoulID::GoodsCompany(CompanyID::default())),
            kind: item,
            qty: 1,
            money_delta: Money::new_inner(9),
        };
        let mut trace = FreightTrace::default();
        for waiting in 0..(MAX_ACTIVE / 2 - 1) {
            assert!(trace.reserve_export_space(terminal, waiting as u32, 1, 1));
            trace.stage_export(terminal, trade, None, 1);
        }
        assert!(trace.reserve_export_space(terminal, (MAX_ACTIVE / 2 - 1) as u32, 1, 1));
        assert!(!trace.admit_export(terminal));
        assert!(!trace.reserve_export_space(terminal, (MAX_ACTIVE / 2 - 1) as u32, 1, 1));
        trace.policies.insert(
            terminal,
            super::super::TerminalPolicyView {
                revision: 1,
                policy: super::super::FreightTerminalPolicy {
                    admit_exports: false,
                    max_calls: 0,
                    ..Default::default()
                },
            },
        );
        trace.stage_export(terminal, trade, None, 2);
        let data = Bincode::encode(&trace).unwrap();
        let mut restored = FreightTrace::decode(&mut data.as_slice()).unwrap();
        restored.rebuild_indexes().unwrap();
        assert_eq!(restored.active_records, MAX_ACTIVE / 2);
        assert_eq!(restored.exports[&terminal].len(), MAX_ACTIVE / 2);
        let page = trace.terminal_view(terminal);
        assert_eq!(page.rows.len(), 64);
        let cursor = page.next.unwrap();
        assert!(trace.continue_trace(cursor).is_ok());
        assert!(restored.continue_trace(cursor).is_err());
    }

    fn test_call(terminal: Endpoint, train: TrainID, epoch: u64) -> CallManifest {
        let anchor = RailAnchor {
            lane: LaneID::default(),
            offset: 10.0,
            position: Vec3::X * 10.0,
            dir: Vec3::X,
        };
        CallManifest {
            key: FreightCallKey {
                terminal,
                train,
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
        }
    }

    #[test]
    fn p8_held_export_backlog_leaves_room_for_an_open_terminal_import() {
        drop(crate::tests::TestCtx::new_fast());
        let mut stations = slotmapd::SlotMap::with_key();
        let held = Endpoint {
            soul: SoulID::FreightStation(stations.insert(())),
            building: BuildingID::default(),
        };
        let open = Endpoint {
            soul: SoulID::FreightStation(stations.insert(())),
            ..held
        };
        let trade = super::super::Trade {
            buyer: super::super::TradeTarget(held.soul),
            seller: super::super::TradeTarget(SoulID::GoodsCompany(CompanyID::default())),
            kind: ItemID::new("coal"),
            qty: 1,
            money_delta: Money::new_inner(9),
        };
        let mut trace = FreightTrace::default();
        trace.policies.insert(
            held,
            super::super::TerminalPolicyView {
                revision: 1,
                policy: super::super::FreightTerminalPolicy {
                    max_calls: 0,
                    ..Default::default()
                },
            },
        );
        for waiting in 0..MAX_ACTIVE / 2 {
            assert!(trace.reserve_export_space(held, waiting as u32, 1, 1));
            trace.stage_export(held, trade, None, 1);
        }
        let desk = ImportDesk {
            on_rail: BTreeMap::from([(ItemID::new("goods"), u64::from(TRAIN_CAPACITY))]),
            ..Default::default()
        };
        let mut call = test_call(open, TrainID::default(), trace.next_id());
        trace.reserve_imports(&mut call, &desk, 2);
        assert_eq!(
            call.imports.iter().map(|line| line.units).sum::<u32>(),
            TRAIN_CAPACITY
        );
        assert!(trace.free_records() >= TRAIN_CAPACITY as usize);
        assert_eq!(trace.exports[&held].len(), MAX_ACTIVE / 2);
    }

    #[test]
    fn p8_freight_cursor_uses_retained_rows_and_expires_after_eviction() {
        let mut stations = slotmapd::SlotMap::with_key();
        let terminal = Endpoint {
            soul: SoulID::FreightStation(stations.insert(())),
            building: BuildingID::default(),
        };
        let other = Endpoint {
            soul: SoulID::FreightStation(stations.insert(())),
            ..terminal
        };
        let mut trace = FreightTrace::default();
        let row = trace.unknown(None, 1);
        trace.record(other, &row, FreightStage::ExportStaged, None, 1);
        for index in 0..129 {
            let row = trace.unknown(None, 1);
            trace.record(terminal, &row, FreightStage::ExportStaged, None, index + 2);
        }
        let cursor = trace.terminal_view(terminal).next.unwrap();
        let expected = trace.continue_trace(cursor).unwrap();
        let ids = |page: &FreightPage| -> Vec<ShipmentId> {
            page.rows.iter().map(|row| row.fragment.id).collect()
        };
        trace.record(other, &row, FreightStage::ExportStaged, None, 132);
        assert_eq!(ids(&trace.continue_trace(cursor).unwrap()), ids(&expected));
        trace.record(terminal, &row, FreightStage::ExportStaged, None, 133);
        trace.age(RETAIN_TICKS + 2);
        let retained = trace.continue_trace(cursor).unwrap();
        assert_eq!(ids(&retained), ids(&expected));
        assert!(retained.next.is_none());
        trace.age(RETAIN_TICKS + 3);
        assert!(trace.continue_trace(cursor).is_err());
    }

    #[test]
    fn p8_snapshot_reconciles_staged_and_assigned_export_fragments() {
        use crate::souls::freight_station::{freight_station_soul, FreightTrainState};
        use crate::world_command::{CommandOutcomes, WorldCommand};
        use crate::BuildingKind;
        use common::saveload::{Bincode, Encoder};

        let mut sim = crate::tests::TestCtx::new_fast().g;
        let (building, proto, rail) = {
            let map = sim.map();
            let (building, proto) = map
                .buildings()
                .values()
                .find_map(|b| match b.kind {
                    BuildingKind::RailFreightStation(proto) => Some((b.id, proto)),
                    _ => None,
                })
                .unwrap();
            let rail = map
                .lanes()
                .values()
                .find(|lane| lane.kind == LaneKind::Rail && lane.points.length() > 200.0)
                .unwrap()
                .id;
            (building, proto, rail)
        };
        let station = freight_station_soul(&mut sim, building, proto).unwrap();
        let terminal = Endpoint {
            soul: SoulID::FreightStation(station),
            building,
        };
        sim.apply_commands([&WorldCommand::SpawnTrain {
            wagons: vec![prototypes::RollingStockID::new("locomotive")],
            lane: rail,
            dist: 100.0,
        }]);
        assert_eq!(sim.read::<CommandOutcomes>().accepted, [0]);
        let train = sim.world().trains.keys().next().unwrap();
        let freight = &mut sim.world_mut_unchecked().freight_stations[station].f;
        freight.trains.push((train, FreightTrainState::Arriving));
        freight.waiting_cargo = 110;
        {
            let mut trace = sim.write::<FreightTrace>();
            assert!(trace.reserve_export_space(terminal, 0, 110, 1));
            trace.stage_export(
                terminal,
                super::super::Trade {
                    buyer: super::super::TradeTarget(terminal.soul),
                    seller: super::super::TradeTarget(SoulID::GoodsCompany(CompanyID::default())),
                    kind: ItemID::new("coal"),
                    qty: 110,
                    money_delta: Money::new_inner(9),
                },
                None,
                1,
            );
            let mut call = test_call(terminal, train, trace.next_id());
            trace.reserve_exports(&mut call, 110);
            assert_eq!(call.exports[0].units, 100);
            assert_eq!(trace.exports[&terminal][0].units, 10);
            trace.calls.insert(train, call);
        }
        let reload = |sim: &crate::Simulation| {
            Bincode::decode::<crate::Simulation>(&Bincode::encode(sim).unwrap())
        };
        assert!(reload(&sim).is_ok(), "a real split must survive reload");

        sim.write::<FreightTrace>().exports.get_mut(&terminal).unwrap()[0].booked =
            Money::new_inner(2);
        let overbooked = reload(&sim).err();
        {
            let mut trace = sim.write::<FreightTrace>();
            let staged = &mut trace.exports.get_mut(&terminal).unwrap()[0];
            staged.booked = Money::new_inner(1);
            staged.units = 11;
        }
        sim.world_mut_unchecked().freight_stations[station].f.waiting_cargo = 111;
        let excess_units = reload(&sim).err();
        assert!(overbooked.is_some(), "the split booked more than its root");
        assert!(excess_units.is_some(), "the split exceeds its root quantity");
    }

    #[test]
    fn p8_freight_closed_order_does_not_cover_later_buyer() {
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
        trace.calls.insert(first.key.train, first);
        desk.orders.clear();
        desk.place(ImportOrder {
            buyer: other,
            quantity: ItemQuantity { item, units: 75 },
            ordered_at: 3,
        });
        // A separate unassigned physical commitment can fill the entire order.
        desk.on_rail.insert(item, 135);
        let mut second = call(trace.next_id());
        trace.reserve_imports(&mut second, &desk, 4);
        assert_eq!(
            second.imports.iter().map(|line| line.units).sum::<u32>(),
            75
        );
        assert!(second
            .imports
            .iter()
            .all(|line| line.ordered_for == Some(other)));
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
