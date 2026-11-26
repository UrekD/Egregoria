use crate::economy::{Bought, Sold, Workers};
use crate::map::{
    BuildingID, BuildingKind, GrowableBusinessState, GrowableLotError, GrowableSiteState, LotID,
    Map, PreparedGrowable, PreparedGrowableAppearance, ZoneCell,
};
use crate::map_dynamic::{
    BuildingInfos, DispatchID, Dispatcher, Itinerary, ItineraryFollower, ItineraryLeader,
    ParkingManagement, Router,
};
use crate::souls::desire::{BuyFood, BuyGoods, Home, Work};
use crate::souls::freight_station::FreightStation;
use crate::souls::goods_company::GoodsCompanyState;
use crate::souls::growth::{self, GrowableSite};
use crate::souls::household::HouseholdStock;
use crate::souls::human::{HumanDecision, PersonalInfo};
use crate::souls::lifecycle;
use crate::souls::residence::{Residence, Residences};
use crate::transportation::road_throughput::RoadUserKind;
use crate::transportation::train::{Locomotive, LocomotiveReservation, RailWagon};
use crate::transportation::{
    Location, Pedestrian, Speed, TransportGrid, Transporter, Vehicle, VehicleKind, VehicleState,
};
use crate::utils::par_command_buffer::SimDrop;
use crate::{impl_entity, impl_trans, Simulation};
use common::iter::chain;
use derive_more::{From, TryInto};
use geom::{Transform, Vec2, Vec3};
use prototypes::{GoodsCompanyID, MAX_PROFILED_SLOTS};
use serde::Deserialize;
use slotmapd::__impl::Serialize;
use slotmapd::{new_key_type, HopSlotMap, Key};
use std::fmt::{Display, Formatter};
use std::ops::{Deref, DerefMut};

new_key_type! {
    pub struct VehicleID;
    pub struct TrainID;
    pub struct HumanID;
    pub struct WagonID;
    pub struct FreightStationID;
    pub struct CompanyID;
}

impl_entity!(VehicleID, VehicleEnt, vehicles);
debug_inspect_impl!(HumanID);
impl_entity!(TrainID, TrainEnt, trains);
impl_entity!(WagonID, WagonEnt, wagons);
impl_entity!(FreightStationID, FreightStationEnt, freight_stations);
impl_entity!(CompanyID, CompanyEnt, companies);

impl_trans!(HumanID);
impl_trans!(VehicleID);
impl_trans!(TrainID);
impl_trans!(WagonID);
impl_trans!(FreightStationID);
impl_trans!(CompanyID);

#[derive(PartialEq, Eq, Copy, Clone, Debug, From, TryInto)]
pub enum AnyEntity {
    VehicleID(VehicleID),
    TrainID(TrainID),
    WagonID(WagonID),
    FreightStationID(FreightStationID),
    CompanyID(CompanyID),
    HumanID(HumanID),
}

#[derive(Inspect, Serialize, Deserialize)]
pub struct VehicleEnt {
    pub trans: Transform,
    pub speed: Speed,
    pub vehicle: Vehicle,
    pub it: Itinerary,
    pub collider: Option<Transporter>,
}

impl SimDrop for VehicleEnt {
    fn sim_drop(mut self, id: VehicleID, sim: &mut Simulation) {
        let res = &sim.resources;
        if let Some(collider) = self.collider {
            res.write::<TransportGrid>().remove_maintain(collider.0);
        }

        if let VehicleState::Parked(resa)
        | VehicleState::RoadToPark(_, _, resa)
        | VehicleState::ReturningToDepot { spot: resa, .. } =
            std::mem::replace(&mut self.vehicle.state, VehicleState::Driving)
        {
            res.write::<ParkingManagement>().free(resa);
        }

        if matches!(self.vehicle.kind, VehicleKind::Truck) {
            res.write::<Dispatcher>()
                .unregister(DispatchID::SmallTruck(id));
        }
        res.write::<crate::transportation::DrivewayTraffic>()
            .vehicle_removed(id);
        lifecycle::vehicle_dropped(sim, id, self.trans.pos);
    }
}

#[derive(Inspect, Serialize, Deserialize)]
pub struct HumanEnt {
    pub trans: Transform,
    pub speed: Speed,
    pub location: Location,
    pub pedestrian: Pedestrian,
    pub collider: Option<Transporter>,

    pub router: Router,
    pub it: Itinerary,

    pub decision: HumanDecision,
    pub home: Home,
    pub food: BuyFood,
    pub goods: BuyGoods,
    pub bought: Bought,
    pub work: Option<Work>,

    pub personal_info: Box<PersonalInfo>,
    #[inspect(skip)]
    pub household: Box<HouseholdStock>,
}

impl SimDrop for HumanEnt {
    fn sim_drop(mut self, id: HumanID, sim: &mut Simulation) {
        if let Some(collider) = self.collider {
            sim.write::<TransportGrid>().remove_maintain(collider.0);
        }
        lifecycle::human_dropped(sim, id, &mut self);
    }
}

#[derive(Inspect, Serialize, Deserialize)]
pub struct TrainEnt {
    pub trans: Transform,
    pub speed: Speed,
    pub it: Itinerary,
    pub locomotive: Locomotive,
    pub res: LocomotiveReservation,
    #[inspect(skip)]
    pub leader: ItineraryLeader,
}

impl SimDrop for TrainEnt {
    fn sim_drop(self, id: TrainID, sim: &mut Simulation) {
        lifecycle::train_dropped(sim, id);
    }
}

#[derive(Inspect, Serialize, Deserialize)]
pub struct WagonEnt {
    pub trans: Transform,
    pub speed: Speed,
    pub wagon: RailWagon,
    pub itfollower: ItineraryFollower,
}

impl SimDrop for WagonEnt {
    fn sim_drop(self, _: WagonID, _: &mut Simulation) {}
}

#[derive(Inspect, Serialize, Deserialize)]
pub struct FreightStationEnt {
    pub trans: Transform,
    pub f: FreightStation,
}

impl SimDrop for FreightStationEnt {
    fn sim_drop(self, id: FreightStationID, sim: &mut Simulation) {
        lifecycle::freight_station_dropped(sim, id, self);
    }
}

#[derive(Inspect, Serialize, Deserialize)]
pub struct CompanyEnt {
    pub trans: Transform,
    pub comp: GoodsCompanyState,
    pub workers: Workers,
    pub sold: Sold,
    pub bought: Bought,
}

impl SimDrop for CompanyEnt {
    fn sim_drop(self, id: CompanyID, sim: &mut Simulation) {
        lifecycle::company_dropped(sim, id, self);
    }
}

/// Human work that can be skipped at a phase's entry.
#[derive(Clone, Copy, Debug)]
pub enum HumanWork {
    Pedestrian,
    RouteChanged,
    RouteStep,
    Itinerary,
    /// Transit boarding examines humans outside and riders in transit, in one pass.
    Outside,
}

impl HumanWork {
    const COUNT: usize = 5;

    const ITINERARY_MEMBERSHIP: u8 =
        (1 << Self::Pedestrian as usize) | (1 << Self::Itinerary as usize);

    fn movement_membership(human: &HumanEnt) -> u8 {
        let needs_itinerary_update = !human.it.is_inert()
            || (human.location == Location::Outside && human.collider.is_some());
        (u8::from(!human.pedestrian.is_still(human)) << Self::Pedestrian as usize)
            | (u8::from(needs_itinerary_update) << Self::Itinerary as usize)
    }

    fn membership(human: &HumanEnt) -> u8 {
        let mut mask = Self::movement_membership(human);
        mask |= u8::from(human.router.needs_route_change(&human.location))
            << Self::RouteChanged as usize;
        mask |= u8::from(human.router.has_route_step()) << Self::RouteStep as usize;
        mask |= u8::from(matches!(
            human.location,
            Location::Outside | Location::Transit(_)
        )) << Self::Outside as usize;
        mask
    }
}

#[derive(Default)]
struct ActivitySet {
    words: Vec<u64>,
    // One bit per nonempty word. This skips vacant slot ranges in order.
    nonempty: Vec<u64>,
}

fn set_bits(mut word: u64) -> impl Iterator<Item = usize> {
    std::iter::from_fn(move || {
        if word == 0 {
            return None;
        }
        let bit = word.trailing_zeros() as usize;
        word &= word - 1;
        Some(bit)
    })
}

impl ActivitySet {
    fn toggle(&mut self, slot: usize) {
        let block = slot / 64;
        let group = block / 64;
        if block >= self.words.len() {
            self.words.resize(block + 1, 0);
        }
        let word = &mut self.words[block];
        let was_empty = *word == 0;
        *word ^= 1 << (slot % 64);
        if was_empty != (*word == 0) {
            if group >= self.nonempty.len() {
                self.nonempty.resize(group + 1, 0);
            }
            self.nonempty[group] ^= 1 << (block % 64);
            while self.nonempty.last() == Some(&0) {
                self.nonempty.pop();
            }
        }
    }

    fn clear(&mut self) {
        self.words.clear();
        self.nonempty.clear();
    }

    fn active_slots(&self) -> impl Iterator<Item = usize> + '_ {
        self.nonempty
            .iter()
            .enumerate()
            .flat_map(move |(group, &blocks)| {
                set_bits(blocks).flat_map(move |bit| {
                    let block = group * 64 + bit;
                    set_bits(self.words[block]).map(move |offset| block * 64 + offset)
                })
            })
    }
}

#[derive(Default)]
struct HumanActivity {
    slots: Vec<(HumanID, u8)>,
    sets: [ActivitySet; HumanWork::COUNT],
    scratch: [Vec<HumanID>; HumanWork::COUNT],
    dirty: bool,
    #[cfg(test)]
    full_scan: bool,
}

impl HumanActivity {
    fn slot(id: HumanID) -> usize {
        // HopSlotMap iteration visits increasing slot indices. In the pinned
        // slotmapd format these occupy the low 32 bits, below the generation.
        id.data().as_ffi() as u32 as usize
    }

    fn register(&mut self, id: HumanID, human: &HumanEnt) {
        let slot = Self::slot(id);
        if slot >= self.slots.len() {
            self.slots.resize(slot + 1, (HumanID::null(), 0));
        }
        self.slots[slot].0 = id;
        self.set(slot, HumanWork::membership(human));
    }

    fn set(&mut self, slot: usize, mask: u8) {
        let previous = &mut self.slots[slot].1;
        let changed = *previous ^ mask;
        if changed == 0 {
            return;
        }
        *previous = mask;
        for kind in set_bits(u64::from(changed)) {
            self.sets[kind].toggle(slot);
        }
    }

    fn refresh(&mut self, id: HumanID, human: &HumanEnt) {
        if !self.dirty {
            self.set(Self::slot(id), HumanWork::membership(human));
        }
    }

    fn refresh_itinerary(&mut self, id: HumanID, human: &HumanEnt) {
        if !self.dirty {
            let slot = Self::slot(id);
            let previous = self.slots[slot].1;
            self.set(
                slot,
                (previous & !HumanWork::ITINERARY_MEMBERSHIP)
                    | HumanWork::movement_membership(human),
            );
        }
    }

    fn rebuild(&mut self, humans: &HopSlotMap<HumanID, HumanEnt>) {
        if self.dirty {
            self.slots.clear();
            for set in &mut self.sets {
                set.clear();
            }
            for (id, human) in humans {
                self.register(id, human);
            }
            self.dirty = false;
        }
    }

    fn collect(
        &self,
        kind: HumanWork,
        out: &mut Vec<HumanID>,
        #[cfg(test)] humans: &HopSlotMap<HumanID, HumanEnt>,
    ) {
        out.clear();
        #[cfg(test)]
        if self.full_scan {
            out.extend(humans.keys());
            return;
        }
        out.extend(
            self.sets[kind as usize]
                .active_slots()
                .map(|slot| self.slots[slot].0),
        );
    }
}

fn unindexed_humans() -> HumanActivity {
    HumanActivity {
        dirty: true,
        ..Default::default()
    }
}

/// Ordered human storage with derived work sets. The serialized representation
/// is the underlying slot map, including its slot generations.
#[derive(Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HumanStorage {
    entities: HopSlotMap<HumanID, HumanEnt>,
    #[serde(skip, default = "unindexed_humans")]
    activity: HumanActivity,
}

impl Deref for HumanStorage {
    type Target = HopSlotMap<HumanID, HumanEnt>;

    fn deref(&self) -> &Self::Target {
        &self.entities
    }
}

/// Compatibility for raw fixture editing. Any mutable slot-map access dirties
/// the work sets, including writes through indexing, iterators and generic storage.
impl DerefMut for HumanStorage {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.activity.dirty = true;
        &mut self.entities
    }
}

impl<'a> IntoIterator for &'a HumanStorage {
    type Item = (HumanID, &'a HumanEnt);
    type IntoIter = slotmapd::hop::Iter<'a, HumanID, HumanEnt>;

    fn into_iter(self) -> Self::IntoIter {
        self.entities.iter()
    }
}

#[derive(Clone, Copy)]
enum HumanRefresh {
    None,
    Itinerary,
    All,
}

/// The storage keeps this guard until its edit callback returns. The index
/// still holds the old mask until the guard drops.
pub(crate) struct HumanEdit<'a> {
    id: HumanID,
    human: &'a mut HumanEnt,
    activity: &'a mut HumanActivity,
    refresh: HumanRefresh,
}

impl HumanEdit<'_> {
    pub(crate) fn decision_mut(&mut self) -> &mut HumanDecision {
        &mut self.human.decision
    }

    pub(crate) fn itinerary_mut(&mut self) -> (&mut Itinerary, &mut geom::Vec3) {
        // Movement can wake the pedestrian pass and finish the itinerary. The
        // other work sets depend only on the router and location.
        if matches!(self.refresh, HumanRefresh::None) {
            self.refresh = HumanRefresh::Itinerary;
        }
        (&mut self.human.it, &mut self.human.trans.pos)
    }
}

impl Deref for HumanEdit<'_> {
    type Target = HumanEnt;

    fn deref(&self) -> &Self::Target {
        self.human
    }
}

impl DerefMut for HumanEdit<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.refresh = HumanRefresh::All;
        self.human
    }
}

impl Drop for HumanEdit<'_> {
    fn drop(&mut self) {
        match self.refresh {
            HumanRefresh::None => (),
            HumanRefresh::Itinerary => self.activity.refresh_itinerary(self.id, self.human),
            HumanRefresh::All => self.activity.refresh(self.id, self.human),
        }
    }
}

impl HumanStorage {
    pub(crate) fn edit<R>(
        &mut self,
        id: HumanID,
        edit: impl FnOnce(&mut HumanEnt) -> R,
    ) -> Option<R> {
        self.edit_guarded(id, |human| edit(human))
    }

    fn edit_guarded<R>(
        &mut self,
        id: HumanID,
        edit: impl FnOnce(&mut HumanEdit<'_>) -> R,
    ) -> Option<R> {
        let mut human = HumanEdit {
            id,
            human: self.entities.get_mut(id)?,
            activity: &mut self.activity,
            refresh: HumanRefresh::None,
        };
        Some(edit(&mut human))
    }

    pub(crate) fn edit_each(&mut self, mut edit: impl FnMut(HumanID, &mut HumanEnt)) {
        self.edit_each_split(|id, human| edit(id, human));
    }

    pub(crate) fn edit_each_split(&mut self, mut edit: impl FnMut(HumanID, &mut HumanEdit<'_>)) {
        for (id, human) in &mut self.entities {
            let mut human = HumanEdit {
                id,
                human,
                activity: &mut self.activity,
                refresh: HumanRefresh::None,
            };
            edit(id, &mut human);
        }
    }

    fn collect_active(&mut self, kind: HumanWork, out: &mut Vec<HumanID>) {
        self.activity.rebuild(&self.entities);
        self.activity.collect(
            kind,
            out,
            #[cfg(test)]
            &self.entities,
        );
    }

    pub(crate) fn edit_active(
        &mut self,
        kind: HumanWork,
        mut edit: impl FnMut(HumanID, &mut HumanEdit<'_>),
    ) {
        // These passes only change the current human. Cross-entity callbacks
        // drain after the pass, before the next phase collects its own IDs.
        let mut ids = std::mem::take(&mut self.activity.scratch[kind as usize]);
        self.collect_active(kind, &mut ids);
        for &id in &ids {
            self.edit_guarded(id, |human| edit(id, human));
        }
        self.activity.scratch[kind as usize] = ids;
    }

    fn insert_indexed(&mut self, human: HumanEnt) -> HumanID {
        let id = self.entities.insert(human);
        if !self.activity.dirty {
            self.activity.register(id, &self.entities[id]);
        }
        id
    }

    fn remove_indexed(&mut self, id: HumanID) -> Option<HumanEnt> {
        let human = self.entities.remove(id)?;
        if !self.activity.dirty {
            self.activity.set(HumanActivity::slot(id), 0);
        }
        Some(human)
    }
}

impl Entity for HumanEnt {
    type ID = HumanID;

    fn storage(w: &World) -> &HopSlotMap<HumanID, Self> {
        &w.humans
    }

    fn storage_mut(w: &mut World) -> &mut HopSlotMap<HumanID, Self> {
        &mut w.humans
    }

    fn insert(w: &mut World, human: Self) -> HumanID {
        w.humans.insert_indexed(human)
    }

    fn remove(w: &mut World, id: HumanID) -> Option<Self> {
        w.humans.remove_indexed(id)
    }
}

impl EntityID for HumanID {
    type Entity = HumanEnt;
}

#[derive(Default, Serialize, Deserialize)]
pub struct World {
    pub vehicles: HopSlotMap<VehicleID, VehicleEnt>,
    pub humans: HumanStorage,
    pub trains: HopSlotMap<TrainID, TrainEnt>,
    pub wagons: HopSlotMap<WagonID, WagonEnt>,
    pub freight_stations: HopSlotMap<FreightStationID, FreightStationEnt>,
    pub companies: HopSlotMap<CompanyID, CompanyEnt>,
    #[serde(default)]
    pub(crate) company_departures: lifecycle::CompanyDepartures,
    #[serde(skip)]
    pub(crate) parking_validation: crate::transportation::road::ParkingValidation,
    #[serde(skip)]
    pub(crate) departing: Option<lifecycle::DepartureReferences>,
}

impl World {
    pub fn collect_active(&mut self, kind: HumanWork, out: &mut Vec<HumanID>) {
        self.humans.collect_active(kind, out);
    }

    pub fn edit_human<R>(
        &mut self,
        id: HumanID,
        edit: impl FnOnce(&mut HumanEnt) -> R,
    ) -> Option<R> {
        self.humans.edit(id, edit)
    }

    pub fn invalidate_derived(&mut self, _reason: &'static str) {
        self.humans.activity.dirty = true;
    }

    pub fn reconcile_derived(&mut self, _phase: &'static str) {
        self.humans.activity.rebuild(&self.humans.entities);
    }

    #[cfg(test)]
    pub(crate) fn use_legacy_human_scan(&mut self) {
        self.humans.activity.full_scan = true;
    }

    pub fn get<E: EntityID>(&self, id: E) -> Option<&E::Entity> {
        <<E as EntityID>::Entity as Entity>::storage(self).get(id)
    }

    pub fn storage<E: Entity>(&self) -> &HopSlotMap<E::ID, E> {
        E::storage(self)
    }

    pub fn storage_id<E: EntityID>(&self, _: E) -> &HopSlotMap<E, E::Entity> {
        E::Entity::storage(self)
    }

    pub fn insert<E: Entity>(&mut self, e: E) -> E::ID {
        E::insert(self, e)
    }

    pub fn contains(&self, id: AnyEntity) -> bool {
        match id {
            AnyEntity::VehicleID(id) => self.storage_id(id).contains_key(id),
            AnyEntity::TrainID(id) => self.storage_id(id).contains_key(id),
            AnyEntity::WagonID(id) => self.storage_id(id).contains_key(id),
            AnyEntity::FreightStationID(id) => self.storage_id(id).contains_key(id),
            AnyEntity::CompanyID(id) => self.storage_id(id).contains_key(id),
            AnyEntity::HumanID(id) => self.storage_id(id).contains_key(id),
        }
    }

    pub fn pos_any(&self, id: AnyEntity) -> Option<Vec3> {
        match id {
            AnyEntity::VehicleID(x) => self.pos(x),
            AnyEntity::TrainID(x) => self.pos(x),
            AnyEntity::WagonID(x) => self.pos(x),
            AnyEntity::HumanID(x) => self.pos(x),
            _ => None,
        }
    }

    pub fn it_any(&self, id: AnyEntity) -> Option<&Itinerary> {
        match id {
            AnyEntity::VehicleID(x) => Some(&self.get(x)?.it),
            AnyEntity::TrainID(x) => Some(&self.get(x)?.it),
            AnyEntity::HumanID(x) => Some(&self.get(x)?.it),
            _ => None,
        }
    }

    pub fn pos<E: WorldTransform>(&self, id: E) -> Option<Vec3> {
        self.get(id).map(|x| E::trans(x).pos)
    }

    pub fn trans<E: WorldTransform>(&self, id: E) -> Option<Transform> {
        self.get(id).map(|x| E::trans(x))
    }

    #[rustfmt::skip]
    pub fn query_trans_itin(&self) -> impl Iterator<Item = (AnyEntity, (&Transform, &Itinerary))> + '_ {
        chain((
            self.humans  .iter().map(|(id, x)| (AnyEntity::HumanID(id), (&x.trans, &x.it))),
            self.vehicles.iter().map(|(id, x)| (AnyEntity::VehicleID(id), (&x.trans, &x.it))),
            self.trains  .iter().map(|(id, x)| (AnyEntity::TrainID(id), (&x.trans, &x.it))),
        ))
    }

    #[rustfmt::skip]
    pub fn query_selectable_pos(&self) -> impl Iterator<Item = (AnyEntity, Vec2)> + '_ {
        chain((
            self.humans  .iter().map(|(id, x)| (AnyEntity::HumanID(id), x.trans.pos.xy())),
            self.vehicles.iter().map(|(id, x)| (AnyEntity::VehicleID(id), x.trans.pos.xy())),
            self.trains  .iter().map(|(id, x)| (AnyEntity::TrainID(id), x.trans.pos.xy())),
            self.wagons  .iter().map(|(id, x)| (AnyEntity::WagonID(id), x.trans.pos.xy())),
        ))
    }

    pub fn query_it_trans_speed(
        &mut self,
    ) -> impl Iterator<Item = (&mut Itinerary, &mut Transform, f32, Option<RoadUserKind>)> + '_ {
        // Inert itineraries record no road entries. Skip reading their user kind.
        let humans = self.humans.values_mut().map(|human| {
            let kind = (!human.it.is_inert() && human.location == Location::Outside)
                .then_some(RoadUserKind::Pedestrian);
            (&mut human.it, &mut human.trans, human.speed.0, kind)
        });
        let trains = self.trains.values_mut().map(|train| {
            (&mut train.it, &mut train.trans, train.speed.0, None)
        });
        let vehicles = self.vehicles.values_mut().map(|vehicle| {
            let kind = (!vehicle.it.is_inert()).then(|| match vehicle.vehicle.kind {
                VehicleKind::Car => RoadUserKind::Car,
                VehicleKind::Truck => RoadUserKind::Truck,
                VehicleKind::Bus => RoadUserKind::Bus,
            });
            (&mut vehicle.it, &mut vehicle.trans, vehicle.speed.0, kind)
        });
        chain((humans, trains, vehicles))
    }


    #[rustfmt::skip]
    pub fn query_trans_speed_coll_vehicle(
        &self,
    ) -> impl Iterator<Item = (&Transform, &Speed, Transporter, Option<&Vehicle>)> {
        chain((
              self.vehicles.values().filter_map(|x| { x.collider.map(|coll| (&x.trans, &x.speed, coll, Some(&x.vehicle))) }),
              self.humans  .values().filter_map(|x| { x.collider.map(|coll| (&x.trans, &x.speed, coll, None)) }),
        ))
    }

    pub fn entities(&self) -> impl Iterator<Item = AnyEntity> + '_ {
        chain((
            chain((
                self.humans.keys().map(AnyEntity::HumanID),
                self.vehicles.keys().map(AnyEntity::VehicleID),
                self.trains.keys().map(AnyEntity::TrainID),
                self.wagons.keys().map(AnyEntity::WagonID),
            )),
            chain((
                self.freight_stations
                    .keys()
                    .map(AnyEntity::FreightStationID),
                self.companies.keys().map(AnyEntity::CompanyID),
            )),
        ))
    }
}

pub(crate) struct GrowablePlan {
    placement: PreparedGrowable,
    home: Option<Residence>,
    job_slots: u32,
    pub(crate) parking: Option<prototypes::GrowableParking>,
}

impl GrowablePlan {
    pub(crate) fn prepare(
        map: &Map,
        site: GrowableSite,
        appearance: Option<PreparedGrowableAppearance>,
        home: Option<Residence>,
        job_slots: u32,
    ) -> Result<Self, GrowableLotError> {
        let lot = map.get(site.lot).ok_or(GrowableLotError::MissingLot)?;
        if lot.parent != site.frontage || lot.shape.corners != site.shape.corners {
            return Err(GrowableLotError::MissingRoad);
        }
        if let Some(appearance) = &appearance {
            let development = &appearance.development;
            if development.zone != site.zone
                || development.level != 1
                || development.floors == 0
                || development.site != GrowableSiteState::Active
                || development.business
                    != site.company.map(|_| GrowableBusinessState::AwaitingSpawn)
                || development.planned_jobs != job_slots
                || development.home != home.as_ref().map(|home| home.prototype)
                || home.as_ref().map(|home| home.prototype) != site.home.map(|proto| proto.id)
                || home
                    .as_ref()
                    .is_some_and(|home| !(1..=MAX_PROFILED_SLOTS).contains(&home.capacity))
                || (site.company.is_some() && !(1..=MAX_PROFILED_SLOTS).contains(&job_slots))
            {
                return Err(GrowableLotError::MissingPrototype);
            }
        }

        let placement = map.prepare_checked_growable(lot, site.kind, site.generator, appearance)?;
        Ok(Self {
            placement,
            home,
            job_slots,
            parking: site
                .home
                .and_then(|p| p.parking)
                .or_else(|| site.company.and_then(|p| p.parking)),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlacedGrowable {
    pub building: BuildingID,
    pub housing_slots: u32,
    pub job_slots: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct GrowableSummary {
    pub zone: ZoneCell,
    pub level: u8,
    pub floors: u8,
    pub roof_z: f32,
    pub height_above_base: f32,
    pub housing_slots: u32,
    pub occupants: usize,
    pub planned_jobs: u32,
    pub active_job_slots: u32,
    pub filled_jobs: usize,
    pub site: GrowableSiteState,
    pub business: Option<GrowableBusinessState>,
}

impl Simulation {
    /// Place a checked growable without spending automatic growth credits.
    pub fn place_zoned_growable(
        &mut self,
        lot: LotID,
        zone: ZoneCell,
    ) -> Result<PlacedGrowable, GrowableLotError> {
        let plan = growth::plan_growable(self, lot, zone)?;
        self.commit_prepared_growable(plan)
    }

    pub(crate) fn commit_prepared_growable(
        &mut self,
        plan: GrowablePlan,
    ) -> Result<PlacedGrowable, GrowableLotError> {
        let GrowablePlan {
            placement,
            home,
            job_slots,
            parking,
        } = plan;
        let building = self.map_mut().commit_prepared_growable(placement)?;
        let mixed = self
            .map()
            .get(building)
            .is_some_and(|site| site.is_mixed_use());
        {
            let mut infos = self.write::<BuildingInfos>();
            infos.insert(building);
            if mixed {
                infos.mark_business_address(building);
            }
        }
        let home_parking = home.is_some();
        let housing_slots = home.as_ref().map_or(0, |home| home.capacity);
        if let Some(home) = home {
            self.write::<Residences>().insert(building, home);
        }
        if let Some(spec) = parking {
            crate::map::building_parking::refresh_property(
                self,
                building,
                Some(spec),
                home_parking,
            );
        }
        Ok(PlacedGrowable {
            building,
            housing_slots,
            job_slots,
        })
    }

    /// Committed development and live role counts for a profiled parcel.
    pub fn growable_summary(&self, id: BuildingID) -> Option<GrowableSummary> {
        let map = self.map();
        let building = map.get(id)?;
        let development = building.development()?;
        let infos = self.read::<BuildingInfos>();
        let residences = self.read::<Residences>();
        let business = crate::map_dynamic::active_business(building, &infos, self.world());
        Some(GrowableSummary {
            zone: development.zone,
            level: development.level,
            floors: development.floors,
            roof_z: development.roof_z,
            height_above_base: development.roof_z - building.height,
            housing_slots: residences
                .by_building
                .get(&id)
                .map_or(0, |home| home.capacity),
            occupants: infos.get(id).map_or(0, |info| info.residents.len()),
            planned_jobs: development.planned_jobs,
            active_job_slots: business.map_or(0, |business| business.slots),
            filled_jobs: business.map_or(0, |business| business.filled),
            site: development.site,
            business: development.business,
        })
    }

    pub(crate) fn profiled_business_can_spawn(
        &self,
        id: BuildingID,
        proto: GoodsCompanyID,
    ) -> bool {
        let map = self.map();
        let Some(building) = map.get(id).filter(|building| {
            building.development().is_some()
                && !building.is_orphaned()
                && building.kind == BuildingKind::GoodsCompany(proto)
                && map.lot_origin(id).is_some()
        }) else {
            return false;
        };
        let site = building.development().expect("profiled development");
        if BuildingKind::GoodsCompany(proto).is_mixed_growable() != building.is_mixed_use() {
            return false;
        }
        site.site == GrowableSiteState::Active
            && (!building.is_mixed_use() || site.home.is_some())
            && site.business.is_some_and(|business| {
                business.can_open(crate::map_dynamic::transit::elapsed_ticks(&self.resources).0)
            })
            && self
                .read::<BuildingInfos>()
                .get(id)
                .is_some_and(|info| info.owner.is_none())
            && site.home.is_none_or(|proto| {
                self.read::<Residences>()
                    .by_building
                    .get(&id)
                    .is_some_and(|home| home.prototype == proto && home.capacity > 0)
            })
    }
}

/// A trait that describes an entity, therefore having storage within the world
pub trait Entity: 'static + Sized + Send {
    type ID: EntityID<Entity = Self>;

    fn storage(w: &World) -> &HopSlotMap<Self::ID, Self>;
    fn storage_mut(w: &mut World) -> &mut HopSlotMap<Self::ID, Self>;

    fn insert(w: &mut World, entity: Self) -> Self::ID {
        Self::storage_mut(w).insert(entity)
    }

    fn remove(w: &mut World, id: Self::ID) -> Option<Self> {
        Self::storage_mut(w).remove(id)
    }
}

/// A trait that describes an entity id to be able to find an Entity from an ID
pub trait EntityID: 'static + slotmapd::Key + Send {
    type Entity: Entity<ID = Self>;
}

/// A trait that describes an entity having a position within the world
pub trait WorldTransform: EntityID {
    fn trans(obj: &Self::Entity) -> Transform;
}

mod macros {
    #[macro_export]
    macro_rules! impl_trans {
        ($t:ty) => {
            impl WorldTransform for $t {
                fn trans(obj: &Self::Entity) -> Transform {
                    obj.trans
                }
            }
        };
    }

    #[macro_export]
    macro_rules! impl_entity {
        ($id:ty, $obj:ty, $s:ident) => {
            debug_inspect_impl!($id);

            impl Entity for $obj {
                type ID = $id;

                fn storage(w: &World) -> &HopSlotMap<Self::ID, Self> {
                    &w.$s
                }

                fn storage_mut(w: &mut World) -> &mut HopSlotMap<Self::ID, Self> {
                    &mut w.$s
                }
            }

            impl EntityID for $id {
                type Entity = $obj;
            }
        };
    }
}

impl Display for AnyEntity {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            AnyEntity::HumanID(id) => write!(f, "{:?}", id),
            AnyEntity::VehicleID(id) => write!(f, "{:?}", id),
            AnyEntity::TrainID(id) => write!(f, "{:?}", id),
            AnyEntity::WagonID(id) => write!(f, "{:?}", id),
            AnyEntity::FreightStationID(id) => write!(f, "{:?}", id),
            AnyEntity::CompanyID(id) => write!(f, "{:?}", id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ActivitySet;

    #[test]
    fn l1_activity_sparse_words_preserve_slot_order_after_mass_removal() {
        let mut set = ActivitySet::default();
        let last = 16_384;
        for slot in 1..=last {
            set.toggle(slot);
        }
        for slot in 2..last {
            set.toggle(slot);
        }
        assert_eq!(set.active_slots().collect::<Vec<_>>(), [1, last]);
        assert_eq!(set.nonempty, [1, 0, 0, 0, 1]);

        set.toggle(last);
        assert_eq!(set.nonempty, [1]);
        for slot in [last, 4096, 64, 2] {
            set.toggle(slot);
        }
        assert_eq!(
            set.active_slots().collect::<Vec<_>>(),
            [1, 2, 64, 4096, last]
        );
    }
}
