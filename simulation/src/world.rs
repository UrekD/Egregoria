use crate::economy::{Bought, Sold, Workers};
use crate::map_dynamic::{
    DispatchID, Dispatcher, Itinerary, ItineraryFollower, ItineraryLeader, ParkingManagement,
    Router,
};
use crate::souls::desire::{BuyFood, BuyGoods, Home, Work};
use crate::souls::freight_station::FreightStation;
use crate::souls::goods_company::GoodsCompanyState;
use crate::souls::household::HouseholdStock;
use crate::souls::human::{HumanDecision, PersonalInfo};
use crate::souls::lifecycle;
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

    fn membership(human: &HumanEnt) -> u8 {
        let mut mask = 0;
        mask |= u8::from(!human.pedestrian.is_still(human)) << Self::Pedestrian as usize;
        mask |= u8::from(human.router.needs_route_change(&human.location))
            << Self::RouteChanged as usize;
        mask |= u8::from(human.router.has_route_step()) << Self::RouteStep as usize;
        mask |= u8::from(!human.it.is_inert()) << Self::Itinerary as usize;
        mask |= u8::from(matches!(
            human.location,
            Location::Outside | Location::Transit(_)
        )) << Self::Outside as usize;
        mask
    }
}

#[derive(Default)]
struct HumanActivity {
    slots: Vec<(HumanID, u8)>,
    sets: [Vec<u64>; HumanWork::COUNT],
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
            for set in &mut self.sets {
                set.resize(slot / 64 + 1, 0);
            }
        }
        self.slots[slot].0 = id;
        self.set(slot, HumanWork::membership(human));
    }

    fn set(&mut self, slot: usize, mask: u8) {
        let previous = &mut self.slots[slot].1;
        let mut changed = *previous ^ mask;
        if changed == 0 {
            return;
        }
        *previous = mask;
        while changed != 0 {
            let kind = changed.trailing_zeros() as usize;
            changed &= changed - 1;
            self.sets[kind][slot / 64] ^= 1 << (slot % 64);
        }
    }

    fn refresh(&mut self, id: HumanID, human: &HumanEnt) {
        if !self.dirty {
            self.set(Self::slot(id), HumanWork::membership(human));
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
        for (block, &word) in self.sets[kind as usize].iter().enumerate() {
            let mut word = word;
            while word != 0 {
                let slot = block * 64 + word.trailing_zeros() as usize;
                out.push(self.slots[slot].0);
                word &= word - 1;
            }
        }
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

/// A split borrow of one human and its indexed contribution. The index still
/// holds the old mask until the edit ends. Dropping the borrow refreshes it.
pub(crate) struct HumanEdit<'a> {
    id: HumanID,
    human: &'a mut HumanEnt,
    activity: &'a mut HumanActivity,
    refresh: bool,
}

impl HumanEdit<'_> {
    pub(crate) fn decision_mut(&mut self) -> &mut HumanDecision {
        &mut self.human.decision
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
        self.refresh = true;
        self.human
    }
}

impl Drop for HumanEdit<'_> {
    fn drop(&mut self) {
        if self.refresh {
            self.activity.refresh(self.id, self.human);
        }
    }
}

impl HumanStorage {
    pub(crate) fn edit<R>(
        &mut self,
        id: HumanID,
        edit: impl FnOnce(&mut HumanEnt) -> R,
    ) -> Option<R> {
        let mut human = HumanEdit {
            id,
            human: self.entities.get_mut(id)?,
            activity: &mut self.activity,
            refresh: true,
        };
        Some(edit(&mut human))
    }

    pub(crate) fn edit_each(&mut self, mut edit: impl FnMut(HumanID, &mut HumanEnt)) {
        self.edit_each_split(|id, mut human| edit(id, &mut human));
    }

    pub(crate) fn edit_each_split(&mut self, mut edit: impl FnMut(HumanID, HumanEdit<'_>)) {
        for (id, human) in &mut self.entities {
            edit(
                id,
                HumanEdit {
                    id,
                    human,
                    activity: &mut self.activity,
                    refresh: false,
                },
            );
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
        mut edit: impl FnMut(HumanID, &mut HumanEnt),
    ) {
        // These passes only change the current human. Cross-entity callbacks
        // drain after the pass, before the next phase collects its own IDs.
        let mut ids = std::mem::take(&mut self.activity.scratch[kind as usize]);
        self.collect_active(kind, &mut ids);
        for &id in &ids {
            self.edit(id, |human| edit(id, human));
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
