//! Property parking is keyed by building identity. Garage capacity needs only a
//! portal and the live cars and reservations using it.
use crate::map::{BuildingID, CurbAnchor, Map, ParkingSpotID, ProjectFilter, RoadID, RoadSide};
use crate::map_dynamic::{DirectedEndpoint, DirectedPlan, Itinerary, ParkingManagement};
use crate::transportation::{
    make_vehicle_entity, prepare_driveway_mouth, sync_actor, DrivewayPhase, Sweep, TransportGrid,
    TransportState, TransportationGroup, Transporter, Vehicle, VehicleKind, VehicleState,
    MOTION_TICKS,
};
use crate::{HumanID, RandProvider, Simulation, VehicleID, World};
use geom::{Intersect, Shape, Transform, Vec2, Vec3, AABB, OBB};
use prototypes::{GrowableParking, Money};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const CAR_RADIUS: f32 = 2.25;
const MAX_SITES: usize = 65_536;
const MAX_LIVE_CLAIMS: usize = 262_144;
const MOUTH_SPACING_CM: u32 = 1_500;

fn bounded_parking_map<'de, D, K, V, const LIMIT: usize>(
    deserializer: D,
) -> Result<BTreeMap<K, V>, D::Error>
where
    D: serde::Deserializer<'de>,
    K: Ord + Deserialize<'de>,
    V: Deserialize<'de>,
{
    use serde::de::{Error, MapAccess, Visitor};
    use std::marker::PhantomData;
    struct BoundedMap<K, V, const LIMIT: usize>(PhantomData<(K, V)>);
    impl<'de, K, V, const LIMIT: usize> Visitor<'de> for BoundedMap<K, V, LIMIT>
    where
        K: Ord + Deserialize<'de>,
        V: Deserialize<'de>,
    {
        type Value = BTreeMap<K, V>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "at most {LIMIT} distinct parking records")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut input: A) -> Result<Self::Value, A::Error> {
            if input.size_hint().is_some_and(|size| size > LIMIT) {
                return Err(A::Error::invalid_length(input.size_hint().unwrap(), &self));
            }
            let mut output = BTreeMap::new();
            while let Some(key) = input.next_key()? {
                if output.len() == LIMIT || output.contains_key(&key) {
                    return Err(A::Error::custom("too many or duplicate parking records"));
                }
                output.insert(key, input.next_value()?);
            }
            Ok(output)
        }
    }
    deserializer.deserialize_map(BoundedMap::<K, V, LIMIT>(PhantomData))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum GarageKind {
    MultiStorey,
    Underground,
}

impl GarageKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::MultiStorey => "Multi-storey garage",
            Self::Underground => "Underground garage",
        }
    }

    pub fn spaces(self) -> u16 {
        match self {
            Self::MultiStorey => 96,
            Self::Underground => 128,
        }
    }

    pub fn cost(self) -> Money {
        Money::new_bucks(match self {
            Self::MultiStorey => 18_000,
            Self::Underground => 32_000,
        })
    }

    pub fn daily_upkeep(self) -> Money {
        Money::new_bucks(match self {
            Self::MultiStorey => 180,
            Self::Underground => 280,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BuildingParkingKind {
    Driveway,
    RearLot,
    IntegratedGarage,
    PublicGarage(GarageKind),
}

impl BuildingParkingKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Driveway => "Driveway",
            Self::RearLot => "Rear lot",
            Self::IntegratedGarage => "Integrated garage",
            Self::PublicGarage(kind) => kind.label(),
        }
    }

    fn stores_inside(self) -> bool {
        matches!(self, Self::IntegratedGarage | Self::PublicGarage(_))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct BuildingParkingSummary {
    pub kind: BuildingParkingKind,
    pub capacity: u16,
    pub occupied: u16,
    pub held: u16,
    pub entrance: CurbAnchor,
    pub entrance_pos: Vec3,
    pub daily_upkeep: Money,
    pub open: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct ParkingLayout {
    kind: BuildingParkingKind,
    anchor: CurbAnchor,
    entrance: Vec3,
    approach: DirectedEndpoint,
    release: DirectedEndpoint,
    #[serde(deserialize_with = "crate::world_command::bounded_command_vec::<_, _, 8>")]
    paths: Vec<StallPath>,
    #[serde(deserialize_with = "crate::world_command::bounded_command_vec::<_, _, 16>")]
    masks: Vec<ParkingSpotID>,
    capacity: u16,
    bounds: AABB,
    door: Vec3,
    depth: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct StallPath {
    into: Sweep,
    out: Sweep,
}

impl ParkingLayout {
    fn path(&self, slot: u8) -> StallPath {
        self.paths[usize::from(slot)]
    }

    pub(crate) fn bounds(&self) -> AABB {
        self.bounds
    }

    pub(crate) fn pavement(&self) -> impl Iterator<Item = [Vec3; 2]> + '_ {
        self.paths.iter().flat_map(|path| {
            path.into.points.windows(2).filter_map(|p| {
                // The driveway joins the outside edge of the existing sidewalk.
                let outward = (self.paths[0].into.pose(MOTION_TICKS).pos - self.entrance).xy();
                (outward.dot((p[0] - self.entrance).xy()) >= 0.0).then_some([p[0], p[1]])
            })
        })
    }
}

/// Convert a world pick into a canonical road-side station.
pub fn parking_entrance_near(map: &Map, cursor: Vec3) -> Option<CurbAnchor> {
    let mut candidates: Vec<_> = map
        .spatial_map()
        .query_around(cursor.xy(), 24.0, ProjectFilter::ROAD)
        .filter_map(|id| match id {
            crate::map::ProjectKind::Road(id) => {
                let road = map.get(id)?;
                let point = road.points().project_2d(cursor.xy());
                Some((ordered_float::OrderedFloat(point.distance2(cursor)), id))
            }
            _ => None,
        })
        .collect();
    candidates.sort_unstable();
    candidates
        .into_iter()
        .find_map(|(_, road)| anchor_near(map, road, cursor.xy()))
}

fn anchor_near(map: &Map, road: RoadID, near: Vec2) -> Option<CurbAnchor> {
    let points = map.get(road)?.points();
    let (p, segment_end) = points.project_segment_2d(near);
    let segment_start = segment_end.checked_sub(1)?;
    let tangent = (points[segment_end] - points[segment_start])
        .xy()
        .try_normalize()?;
    let side = if tangent.perpendicular().dot(near - p.xy()) > 0.0 {
        RoadSide::Right
    } else {
        RoadSide::Left
    };
    let distance = points
        .segments()
        .take(segment_start)
        .map(|s| s.src.distance(s.dst))
        .sum::<f32>()
        + points[segment_start].distance(p);
    map.pick_curb(road, side, (distance * 100.0).round() as u32)
}

pub(crate) fn prepare_property_parking(
    map: &Map,
    road: RoadID,
    parcel: OBB,
    shell: OBB,
    door: Vec3,
    spec: GrowableParking,
    residential: bool,
    clear: impl Fn(&ParkingLayout) -> bool,
) -> Option<ParkingLayout> {
    let front = (parcel.corners[0] + parcel.corners[1]) * 0.5;
    let width = parcel.corners[0].distance(parcel.corners[1]);
    let depth = parcel.corners[1].distance(parcel.corners[2]);
    let across = (parcel.corners[1] - parcel.corners[0]).try_normalize()?;
    let surface = |spaces: u8| {
        let kind = if residential {
            BuildingParkingKind::Driveway
        } else {
            BuildingParkingKind::RearLot
        };
        let offsets = if residential {
            [0.30, -0.30, 0.22, -0.22]
        } else {
            [0.36, -0.36, 0.40, -0.40]
        };
        for offset in offsets.map(|fraction| width * fraction) {
            let target = front + across * offset;
            let Some(anchor) = anchor_near(map, road, target + (parcel.center() - front) * 0.15)
            else {
                continue;
            };
            let setback = if residential { 4.5 } else { depth - 4.0 };
            for capacity in (1..=spaces).rev() {
                let parking = LayoutDraft {
                    kind,
                    anchor,
                    parcel,
                    solid: Some(shell),
                    door,
                    depth: setback,
                    capacity: u16::from(capacity),
                };
                if let Some(layout) = parking.prepare(map).filter(&clear) {
                    return Some(layout);
                }
            }
        }
        None
    };
    match spec {
        GrowableParking::Surface(spaces) => surface(spaces),
        GrowableParking::Garage {
            spaces,
            fallback_surface_spaces,
        } => {
            let garage = [0.23, -0.23, 0.0].into_iter().find_map(|offset| {
                let target = front + across * (offset * width);
                let anchor = anchor_near(map, road, target + (parcel.center() - front) * 0.15)?;
                LayoutDraft {
                    kind: BuildingParkingKind::IntegratedGarage,
                    anchor,
                    parcel,
                    solid: Some(shell),
                    door,
                    depth: 4.0,
                    capacity: spaces,
                }
                .prepare(map)
                .filter(&clear)
            });
            garage.or_else(|| surface(fallback_surface_spaces))
        }
    }
}

/// The cursor selects the entrance. Both garage footprints build outward from it.
pub fn garage_footprint(map: &Map, kind: GarageKind, anchor: CurbAnchor) -> Option<(OBB, Vec3)> {
    let (center, _, _, _, _, direction) = prepare_driveway_mouth(map, anchor).ok()?;
    let outward = direction.perpendicular();
    let entrance = center + outward.z0() * (map.get(anchor.road)?.width * 0.5);
    let (width, depth) = match kind {
        GarageKind::MultiStorey => (32.0, 30.0),
        GarageKind::Underground => (14.0, 14.0),
    };
    Some((
        OBB::new(
            entrance.xy() + outward * (depth * 0.5 + 0.25),
            outward,
            depth,
            width,
        ),
        entrance,
    ))
}

pub(crate) fn prepare_public_garage(
    map: &Map,
    kind: GarageKind,
    anchor: CurbAnchor,
    parcel: OBB,
    door: Vec3,
) -> Option<ParkingLayout> {
    LayoutDraft {
        kind: BuildingParkingKind::PublicGarage(kind),
        anchor,
        parcel,
        solid: None,
        door,
        depth: 4.5,
        capacity: kind.spaces(),
    }
    .prepare(map)
}

struct LayoutDraft {
    kind: BuildingParkingKind,
    anchor: CurbAnchor,
    parcel: OBB,
    solid: Option<OBB>,
    door: Vec3,
    depth: f32,
    capacity: u16,
}

impl LayoutDraft {
    fn prepare(self, map: &Map) -> Option<ParkingLayout> {
        let (center, approach, release, from, to, direction) =
            prepare_driveway_mouth(map, self.anchor).ok()?;
        let road = map.get(self.anchor.road)?;
        let outward = direction.perpendicular();
        let edge = road.width * 0.5;
        let entrance = center + outward.z0() * edge;
        let stored_inside = self.kind.stores_inside();
        let path_count = if stored_inside { 1 } else { self.capacity };
        let mut paths: Vec<StallPath> = Vec::with_capacity(usize::from(path_count));
        let mut bounds = self.parcel.bbox();
        let mut masks = BTreeSet::new();
        let environment = map.environment();

        for slot in 0..path_count {
            // A rear aisle can serve successive spaces farther into the parcel.
            let distance = edge + self.depth - f32::from(slot) * 5.0;
            let pos = center + outward.z0() * distance;
            let stall = OBB::new(pos.xy(), direction, 4.8, 2.6);
            if !stall.corners.iter().all(|&p| self.parcel.contains(p))
                || self.solid.is_some_and(|body| body.intersects(&stall))
            {
                return None;
            }
            let parked = Transform::new_dir(pos, direction.z0());
            let sweep = |a: Transform, b: Transform| {
                if self.kind == BuildingParkingKind::RearLot {
                    // Keep long approaches centered in the narrow side aisle.
                    Sweep::with_tangent(a, b, 9.0)
                } else {
                    Sweep::between(a, b)
                }
            };
            let into = sweep(from, parked);
            let out = sweep(parked, to);
            if paths.iter().any(|other| {
                let other_stall = other.into.pose(MOTION_TICKS).pos;
                let reaches = |sweep: &Sweep, stall: Vec3| {
                    sweep
                        .segments()
                        .any(|segment| segment.project(stall).distance2(stall) <= 2.7_f32.powi(2))
                };
                reaches(&other.out, parked.pos)
                    || reaches(&other.into, parked.pos)
                    || reaches(&out, other_stall)
                    || reaches(&into, other_stall)
            }) {
                return None;
            }
            for point in into.points.iter().chain(&out.points) {
                let xy = point.xy();
                if environment.is_water(xy)
                    || environment
                        .height(xy)
                        .is_none_or(|z| (z + crate::map::ROAD_Z_OFFSET - point.z).abs() > 0.6)
                    || !map.authorize_driveway_pad(&OBB::new(xy, direction, 2.5, 2.5).corners)
                    || self
                        .solid
                        .is_some_and(|body| body.expand(1.25).contains(xy))
                    || (outward.dot(xy - entrance.xy()) > 1.5
                        && !self.parcel.expand(-0.5).contains(xy))
                {
                    return None;
                }
            }
            for sweep in [&into, &out] {
                bounds.ll = bounds.ll.min(sweep.bounds().ll);
                bounds.ur = bounds.ur.max(sweep.bounds().ur);
            }
            let near_lane = map.get(approach.lane)?;
            if let Some(curb) = road.parking_next_to(near_lane) {
                for id in map.parking.closest_spots(curb, entrance)?.take(12) {
                    let spot = map.parking.get(id)?;
                    let conflicts = |sweep: &Sweep| {
                        sweep.segments().any(|segment| {
                            let projection = segment.project(spot.trans.pos);
                            projection.distance2(spot.trans.pos) < 3.55_f32.powi(2)
                        })
                    };
                    if conflicts(&into) || conflicts(&out) {
                        masks.insert(id);
                    }
                }
            }
            paths.push(StallPath { into, out });
        }

        Some(ParkingLayout {
            kind: self.kind,
            anchor: self.anchor,
            entrance,
            approach,
            release,
            paths,
            masks: masks.into_iter().collect(),
            capacity: self.capacity,
            bounds: bounds.expand(CAR_RADIUS),
            door: self.door,
            depth: self.depth,
        })
    }
}

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct BuildingParking {
    #[serde(deserialize_with = "bounded_parking_map::<_, _, _, MAX_SITES>")]
    sites: BTreeMap<BuildingID, Site>,
    #[serde(skip)]
    mouths: BTreeMap<RoadID, BTreeSet<(u32, BuildingID)>>,
    #[serde(skip)]
    public_garages: BTreeSet<BuildingID>,
    #[serde(skip)]
    closing_sites: BTreeSet<BuildingID>,
    #[serde(deserialize_with = "bounded_parking_map::<_, _, _, MAX_LIVE_CLAIMS>")]
    parked: BTreeMap<VehicleID, Occupant>,
    #[serde(deserialize_with = "bounded_parking_map::<_, _, _, MAX_LIVE_CLAIMS>")]
    arrivals: BTreeMap<VehicleID, Arrival>,
    #[serde(deserialize_with = "bounded_parking_map::<_, _, _, MAX_SITES>")]
    motions: BTreeMap<BuildingID, Motion>,
}

#[derive(Serialize, Deserialize)]
struct Site {
    layout: ParkingLayout,
    #[serde(deserialize_with = "bounded_parking_map::<_, _, _, 256>")]
    occupied: BTreeMap<VehicleID, u8>,
    #[serde(deserialize_with = "bounded_parking_map::<_, _, _, 256>")]
    held: BTreeMap<VehicleID, u8>,
    closing: bool,
}

impl Site {
    fn free_slot(&self) -> Option<u8> {
        if self.closing
            || self.occupied.len() + self.held.len() >= usize::from(self.layout.capacity)
        {
            return None;
        }
        if self.layout.kind.stores_inside() {
            return Some(0);
        }
        (0..self.layout.paths.len() as u8).find(|slot| {
            !self
                .occupied
                .values()
                .chain(self.held.values())
                .any(|taken| slot == taken)
        })
    }

    fn sweep_clear(&self, sweep: &Sweep, except: VehicleID) -> bool {
        self.layout.kind.stores_inside()
            || self.occupied.iter().all(|(&car, &slot)| {
                car == except
                    || sweep.segments().all(|segment| {
                        let occupied = self.layout.path(slot).into.pose(MOTION_TICKS).pos;
                        segment.project(occupied).distance2(occupied) > 2.7_f32.powi(2)
                    })
            })
    }
}

#[derive(Serialize, Deserialize)]
struct Occupant {
    site: BuildingID,
    driver: HumanID,
}

#[derive(Serialize, Deserialize)]
struct Arrival {
    site: BuildingID,
    driver: HumanID,
}

#[derive(Serialize, Deserialize)]
struct Motion {
    car: VehicleID,
    tick: u16,
    departure: Option<DirectedPlan>,
}

impl BuildingParking {
    pub(crate) fn is_empty(&self) -> bool {
        self.sites.is_empty()
    }

    pub(crate) fn site_count(&self) -> usize {
        self.sites.len()
    }

    pub(crate) fn has_runtime_work(&self) -> bool {
        !self.motions.is_empty() || !self.arrivals.is_empty()
    }

    pub(crate) fn fits(
        &self,
        parking: &ParkingManagement,
        layout: &ParkingLayout,
        replacing: Option<BuildingID>,
    ) -> bool {
        let station = layout.anchor.station_cm;
        let nearby =
            station.saturating_sub(MOUTH_SPACING_CM)..=station.saturating_add(MOUTH_SPACING_CM);
        (replacing.is_some_and(|id| self.sites.contains_key(&id)) || self.sites.len() < MAX_SITES)
            && layout.masks.iter().all(|&id| parking.is_unreserved(id))
            && self.mouths.get(&layout.anchor.road).is_none_or(|mouths| {
                mouths
                    .range((nearby.start().to_owned(), BuildingID::default())..)
                    .take_while(|(station, _)| nearby.contains(station))
                    .all(|&(_, id)| {
                        Some(id) == replacing
                            || self.sites[&id].layout.anchor.side != layout.anchor.side
                    })
            })
    }

    pub(crate) fn install(
        &mut self,
        id: BuildingID,
        layout: ParkingLayout,
        parking: &mut ParkingManagement,
    ) {
        self.remove_empty(id, parking);
        for &spot in &layout.masks {
            parking.mask(spot);
        }
        self.mouths
            .entry(layout.anchor.road)
            .or_default()
            .insert((layout.anchor.station_cm, id));
        if matches!(layout.kind, BuildingParkingKind::PublicGarage(_)) {
            self.public_garages.insert(id);
        }
        self.sites.insert(
            id,
            Site {
                layout,
                occupied: BTreeMap::new(),
                held: BTreeMap::new(),
                closing: false,
            },
        );
    }

    pub(crate) fn remove_empty(&mut self, id: BuildingID, parking: &mut ParkingManagement) {
        debug_assert!(!self.busy(id));
        if let Some(site) = self.sites.remove(&id) {
            let anchor = site.layout.anchor;
            let mouths = self.mouths.get_mut(&anchor.road).unwrap();
            mouths.remove(&(anchor.station_cm, id));
            if mouths.is_empty() {
                self.mouths.remove(&anchor.road);
            }
            self.public_garages.remove(&id);
            self.closing_sites.remove(&id);
            for spot in site.layout.masks {
                parking.unmask(spot);
            }
        }
    }

    pub(crate) fn busy(&self, id: BuildingID) -> bool {
        self.sites
            .get(&id)
            .is_some_and(|site| !site.occupied.is_empty() || !site.held.is_empty())
    }

    pub(crate) fn set_open(&mut self, id: BuildingID, open: bool) -> bool {
        let Some(site) = self.sites.get_mut(&id) else {
            return false;
        };
        site.closing = !open;
        if open {
            self.closing_sites.remove(&id);
        } else {
            self.closing_sites.insert(id);
        }
        true
    }

    pub(crate) fn summary(&self, id: BuildingID) -> Option<BuildingParkingSummary> {
        let site = self.sites.get(&id)?;
        let layout = &site.layout;
        let daily_upkeep = match layout.kind {
            BuildingParkingKind::PublicGarage(kind) => kind.daily_upkeep(),
            _ => Money::ZERO,
        };
        Some(BuildingParkingSummary {
            kind: layout.kind,
            capacity: layout.capacity,
            occupied: site.occupied.len() as u16,
            held: site.held.len() as u16,
            entrance: layout.anchor,
            entrance_pos: layout.entrance,
            daily_upkeep,
            open: !site.closing,
        })
    }

    pub(crate) fn owns_stage(&self, car: VehicleID, phase: Option<DrivewayPhase>) -> bool {
        match phase {
            Some(DrivewayPhase::Stored | DrivewayPhase::Exiting) => self.parked.contains_key(&car),
            Some(DrivewayPhase::Entering) => self.arrivals.get(&car).is_some_and(|arrival| {
                self.motions
                    .get(&arrival.site)
                    .is_some_and(|motion| motion.car == car)
            }),
            _ => false,
        }
    }

    pub(crate) fn parked_at(&self, car: VehicleID) -> Option<BuildingID> {
        self.parked.get(&car).map(|occupant| occupant.site)
    }

    pub(crate) fn hidden(&self, car: VehicleID) -> bool {
        self.parked_at(car).is_some_and(|site| {
            self.sites[&site].layout.kind.stores_inside()
                && self
                    .motions
                    .get(&site)
                    .is_none_or(|motion| motion.car != car)
        })
    }

    pub(crate) fn release(&self, car: VehicleID) -> Option<DirectedEndpoint> {
        Some(self.sites[&self.parked_at(car)?].layout.release)
    }

    pub(crate) fn has_public_garages(&self) -> bool {
        !self.public_garages.is_empty()
    }

    pub(crate) fn nearby_public_garages(&self, map: &Map, destination: Vec3) -> Vec<BuildingID> {
        if !self.has_public_garages() {
            return Vec::new();
        }
        let mut garages: Vec<_> = map
            .spatial_map()
            .query_around(destination.xy(), 100.0, ProjectFilter::BUILDING)
            .filter_map(|object| {
                let crate::map::ProjectKind::Building(id) = object else {
                    return None;
                };
                if !self.public_garages.contains(&id) {
                    return None;
                }
                let distance = self.sites[&id].layout.door.distance2(destination);
                (distance <= 100.0_f32.powi(2))
                    .then_some((ordered_float::OrderedFloat(distance), id))
            })
            .collect();
        garages.sort_unstable();
        garages.into_iter().take(4).map(|(_, id)| id).collect()
    }

    pub(crate) fn arrival_target(
        &self,
        id: BuildingID,
        car: VehicleID,
    ) -> Option<(DirectedEndpoint, Vec3)> {
        let site = self.sites.get(&id)?;
        let slot = site.free_slot()?;
        let path = site.layout.path(slot);
        site.sweep_clear(&path.into, car)
            .then_some((site.layout.approach, site.layout.door))
    }

    pub(crate) fn hold(&mut self, site: BuildingID, driver: HumanID, car: VehicleID) -> bool {
        let Some(target) = self.sites.get_mut(&site) else {
            return false;
        };
        let Some(slot) = target.free_slot() else {
            return false;
        };
        if self.arrivals.len() == MAX_LIVE_CLAIMS
            || self.parked.len() + self.arrivals.len() >= MAX_LIVE_CLAIMS
            || self.arrivals.contains_key(&car)
            || self
                .parked
                .get(&car)
                .is_some_and(|occupant| occupant.site == site)
        {
            return false;
        }
        self.sites.get_mut(&site).unwrap().held.insert(car, slot);
        self.arrivals.insert(car, Arrival { site, driver });
        true
    }

    pub(crate) fn is_moving(&self, car: VehicleID) -> bool {
        self.parked_at(car)
            .or_else(|| self.arrivals.get(&car).map(|arrival| arrival.site))
            .and_then(|site| self.motions.get(&site))
            .is_some_and(|motion| motion.car == car)
    }

    pub(crate) fn closing_owners(
        &self,
        slice: u64,
    ) -> impl Iterator<Item = (HumanID, VehicleID, BuildingID, Vec3)> + '_ {
        self.closing_sites.iter().copied().flat_map(move |id| {
            let site = &self.sites[&id];
            site.occupied.keys().filter_map(move |&car| {
                use slotmapd::Key;
                (common::hash_u64((id.data().as_ffi(), car)) % 16 == slice % 16)
                    .then(|| (self.parked[&car].driver, car, id, site.layout.door))
            })
        })
    }

    pub(crate) fn vehicle_removed(&mut self, car: VehicleID) {
        self.release_arrival(car);
        if let Some(occupant) = self.parked.remove(&car) {
            self.sites
                .get_mut(&occupant.site)
                .unwrap()
                .occupied
                .remove(&car);
            if self
                .motions
                .get(&occupant.site)
                .is_some_and(|motion| motion.car == car)
            {
                self.motions.remove(&occupant.site);
            }
        }
    }

    pub(crate) fn release_arrival(&mut self, car: VehicleID) {
        if let Some(arrival) = self.arrivals.remove(&car) {
            self.sites.get_mut(&arrival.site).unwrap().held.remove(&car);
            if self
                .motions
                .get(&arrival.site)
                .is_some_and(|motion| motion.car == car)
            {
                self.motions.remove(&arrival.site);
            }
        }
    }

    pub(crate) fn start_arrival(
        &mut self,
        map: &Map,
        grid: &mut TransportGrid,
        car: VehicleID,
        actor: &mut crate::world::VehicleEnt,
    ) -> bool {
        let Some(arrival) = self.arrivals.get(&car) else {
            return false;
        };
        let id = arrival.site;
        let site = &self.sites[&id];
        let path = site.layout.path(site.held[&car]);
        if self.motions.contains_key(&id)
            || actor.collider.is_none()
            || actor.speed.0.abs() > 0.2
            || site
                .layout
                .approach
                .transform(map)
                .is_none_or(|approach| actor.trans.pos.distance2(approach.pos) > 1.5_f32.powi(2))
            || !site.sweep_clear(&path.into, car)
            || path.into.occupied(grid, actor.collider)
        {
            return false;
        }
        actor.trans = path.into.pose(0);
        actor.it = Itinerary::at_driveway(DrivewayPhase::Entering);
        sync_actor(grid, actor);
        self.motions.insert(
            id,
            Motion {
                car,
                tick: 0,
                departure: None,
            },
        );
        true
    }

    pub(crate) fn start_departure(
        &mut self,
        grid: &mut TransportGrid,
        car: VehicleID,
        actor: &mut crate::world::VehicleEnt,
        route: DirectedPlan,
    ) -> bool {
        let Some(id) = self.parked_at(car) else {
            return false;
        };
        let site = &self.sites[&id];
        let path = site.layout.path(site.occupied[&car]);
        if self.motions.contains_key(&id)
            || route.origin() != site.layout.release
            || !site.sweep_clear(&path.out, car)
            || path.out.occupied(grid, None)
        {
            return false;
        }
        actor.trans = path.out.pose(0);
        actor.collider = Some(Transporter(grid.insert(
            actor.trans.pos.xy(),
            TransportState {
                dir: actor.trans.dir.xy(),
                height: actor.trans.pos.z,
                radius: CAR_RADIUS,
                group: TransportationGroup::Vehicles,
                ..Default::default()
            },
        )));
        actor.vehicle.state = VehicleState::Driving;
        actor.it = Itinerary::at_driveway(DrivewayPhase::Exiting);
        self.motions.insert(
            id,
            Motion {
                car,
                tick: 0,
                departure: Some(route),
            },
        );
        true
    }

    pub(crate) fn has_crossing(&self) -> bool {
        !self.motions.is_empty()
    }

    pub(crate) fn active_sweeps(&self) -> impl Iterator<Item = (VehicleID, Sweep)> + '_ {
        self.motions.iter().map(|(&id, motion)| {
            let site = &self.sites[&id];
            let sweep = if motion.departure.is_some() {
                site.layout.path(site.occupied[&motion.car]).out
            } else {
                site.layout.path(site.held[&motion.car]).into
            };
            (motion.car, sweep)
        })
    }

    pub(crate) fn update(&mut self, world: &mut World, grid: &mut TransportGrid) {
        self.motions.retain(|id, motion| {
            let site = self.sites.get_mut(id).unwrap();
            let Some(actor) = world.vehicles.get_mut(motion.car) else {
                return false;
            };
            motion.tick += 1;
            let path = if motion.departure.is_some() {
                site.layout.path(site.occupied[&motion.car]).out
            } else {
                site.layout.path(site.held[&motion.car]).into
            };
            actor.trans = path.pose(motion.tick);
            if motion.tick < MOTION_TICKS {
                sync_actor(grid, actor);
                return true;
            }
            if let Some(route) = motion.departure.take() {
                self.parked.remove(&motion.car);
                site.occupied.remove(&motion.car);
                actor.it = route.into_itinerary();
                sync_actor(grid, actor);
            } else {
                let arrival = self.arrivals.remove(&motion.car).unwrap();
                let slot = site.held.remove(&motion.car).unwrap();
                site.occupied.insert(motion.car, slot);
                self.parked.insert(
                    motion.car,
                    Occupant {
                        site: *id,
                        driver: arrival.driver,
                    },
                );
                if let Some(collider) = actor.collider.take() {
                    grid.remove_maintain(collider.0);
                }
                actor.vehicle.state = VehicleState::ParkedAtDepot;
                actor.it = Itinerary::at_driveway(DrivewayPhase::Stored);
            }
            false
        });
        self.arrivals.retain(|&car, arrival| {
            let live_route = world
                .humans
                .get(arrival.driver)
                .is_some_and(|human| human.router.has_property_arrival(car, arrival.site));
            if live_route
                || self
                    .motions
                    .get(&arrival.site)
                    .is_some_and(|m| m.car == car)
            {
                return true;
            }
            self.sites.get_mut(&arrival.site).unwrap().held.remove(&car);
            false
        });
    }

    pub(crate) fn protected_roads(
        &self,
        map: &Map,
        roads: &mut BTreeSet<RoadID>,
        retire_vacant: bool,
    ) {
        for (&id, site) in &self.sites {
            if !retire_vacant
                || self.busy(id)
                || matches!(site.layout.kind, BuildingParkingKind::PublicGarage(_))
            {
                roads.insert(site.layout.anchor.road);
            }
        }
        for route in self
            .motions
            .values()
            .filter_map(|motion| motion.departure.as_ref())
        {
            roads.extend(route.roads(map));
        }
    }

    pub(crate) fn retire_invalidated_vacant(
        &mut self,
        map: &mut Map,
        affected: &BTreeSet<RoadID>,
        parking: &mut ParkingManagement,
    ) {
        let vacant: Vec<_> = affected
            .iter()
            .filter_map(|road| self.mouths.get(road))
            .flatten()
            .map(|&(_, id)| id)
            .filter(|&id| {
                let layout = &self.sites[&id].layout;
                !self.busy(id) && !matches!(layout.kind, BuildingParkingKind::PublicGarage(_)) && {
                    let anchor = layout.anchor;
                    map.pick_curb(anchor.road, anchor.side, anchor.station_cm) != Some(anchor)
                        || layout.masks.iter().any(|&mask| !map.parking.contains(mask))
                }
            })
            .collect();
        for id in vacant {
            self.remove_empty(id, parking);
            if let Some(building) = map.buildings.get(id) {
                map.subscribers
                    .dispatch(crate::map::UpdateType::Building, building);
            }
        }
    }

    pub(crate) fn overlapping_road(&self, bounds: &AABB) -> Option<RoadID> {
        self.sites
            .values()
            .find(|site| site.layout.bounds.intersects(bounds))
            .map(|site| site.layout.anchor.road)
    }

    pub(crate) fn restore(
        &mut self,
        map: &Map,
        world: &World,
        grid: &TransportGrid,
        parking: &mut ParkingManagement,
    ) -> Result<(), &'static str> {
        if self.parked.len() + self.arrivals.len() > MAX_LIVE_CLAIMS {
            return Err("too many live property parking claims");
        }
        for building in map.buildings().values() {
            if matches!(building.kind, crate::map::BuildingKind::ParkingGarage(_))
                && !self.sites.contains_key(&building.id)
            {
                return Err("parking garage has no parking facility");
            }
        }

        for (&id, site) in &self.sites {
            let layout = &site.layout;
            let building = map.get(id).ok_or("missing property parking building")?;
            let solid = match (layout.kind, building.kind) {
                (
                    BuildingParkingKind::PublicGarage(kind),
                    crate::map::BuildingKind::ParkingGarage(building_kind),
                ) if kind == building_kind
                    && kind.spaces() == layout.capacity
                    && layout.depth == 4.5 =>
                {
                    if garage_footprint(map, kind, layout.anchor)
                        .is_none_or(|(parcel, _)| parcel.corners != building.obb.corners)
                    {
                        return Err("invalid parking garage footprint");
                    }
                    None
                }
                (BuildingParkingKind::PublicGarage(_), _)
                | (_, crate::map::BuildingKind::ParkingGarage(_)) => {
                    return Err("invalid property parking building kind");
                }
                _ => {
                    let shell = building
                        .development()
                        .and_then(|site| site.frozen_shell.as_ref())
                        .ok_or("parking facility has no growable shell")?;
                    Some(shell.fit().footprint())
                }
            };
            if building.connected_road != Some(layout.anchor.road)
                || layout.capacity == 0
                || layout.capacity > 256
                || (!layout.kind.stores_inside() && layout.capacity > 8)
                || !(1.0..=40.0).contains(&layout.depth)
                || site.occupied.len() + site.held.len() > usize::from(layout.capacity)
                || (!layout.kind.stores_inside() && {
                    let cars = site.occupied.len() + site.held.len();
                    site.occupied
                        .values()
                        .chain(site.held.values())
                        .copied()
                        .collect::<BTreeSet<_>>()
                        .len()
                        != cars
                })
            {
                return Err("invalid property parking facility");
            }
            let expected = LayoutDraft {
                kind: layout.kind,
                anchor: layout.anchor,
                parcel: building.obb,
                solid,
                door: building.door_pos,
                depth: layout.depth,
                capacity: layout.capacity,
            }
            .prepare(map);
            if expected.as_ref() != Some(layout) || !self.fits(parking, layout, Some(id)) {
                return Err("invalid property parking geometry");
            }
            self.mouths
                .entry(layout.anchor.road)
                .or_default()
                .insert((layout.anchor.station_cm, id));
            if matches!(layout.kind, BuildingParkingKind::PublicGarage(_)) {
                self.public_garages.insert(id);
            }
            if site.closing {
                self.closing_sites.insert(id);
            }
            for &mask in &layout.masks {
                if !map.parking.contains(mask) || !parking.is_unreserved(mask) {
                    return Err("invalid property parking curb mask");
                }
                parking.mask(mask);
            }
            for (&car, &slot) in &site.occupied {
                let Some(actor) = world.vehicles.get(car) else {
                    return Err("missing property parking vehicle");
                };
                let exiting = self
                    .motions
                    .get(&id)
                    .is_some_and(|motion| motion.car == car && motion.departure.is_some());
                if self.parked.get(&car).is_none_or(|occupant| {
                    occupant.site != id
                        || world
                            .humans
                            .get(occupant.driver)
                            .is_none_or(|human| human.router.personal_car != Some(car))
                }) || usize::from(slot) >= layout.paths.len()
                    || !matches!(actor.vehicle.kind, VehicleKind::Car)
                    || (!exiting
                        && (!matches!(actor.vehicle.state, VehicleState::ParkedAtDepot)
                            || actor.it.driveway_phase() != Some(DrivewayPhase::Stored)
                            || actor.collider.is_some()
                            || actor.speed.0 != 0.0
                            || actor.trans != layout.path(slot).into.pose(MOTION_TICKS)))
                {
                    return Err("invalid property parking occupancy");
                }
            }
            for (&car, &slot) in &site.held {
                let Some(arrival) = self.arrivals.get(&car) else {
                    return Err("missing property parking hold");
                };
                let entering = self
                    .motions
                    .get(&id)
                    .is_some_and(|motion| motion.car == car && motion.departure.is_none());
                if arrival.site != id
                    || self.parked_at(car) == Some(id)
                    || usize::from(slot) >= layout.paths.len()
                    || world
                        .vehicles
                        .get(car)
                        .is_none_or(|vehicle| !matches!(vehicle.vehicle.kind, VehicleKind::Car))
                    || (!entering
                        && world.humans.get(arrival.driver).is_none_or(|human| {
                            human.router.personal_car != Some(car)
                                || !human.router.has_property_arrival(car, id)
                        }))
                {
                    return Err("invalid property parking hold");
                }
            }
        }
        for (&id, motion) in &self.motions {
            let Some(site) = self.sites.get(&id) else {
                return Err("unknown parking motion site");
            };
            if motion.tick >= MOTION_TICKS {
                return Err("invalid property parking maneuver");
            }
            let (expected, phase) = match &motion.departure {
                Some(route) if route.valid(map) && route.origin() == site.layout.release => {
                    let slot = *site
                        .occupied
                        .get(&motion.car)
                        .ok_or("missing property departure")?;
                    (
                        site.layout.path(slot).out.pose(motion.tick),
                        DrivewayPhase::Exiting,
                    )
                }
                None => {
                    let slot = *site
                        .held
                        .get(&motion.car)
                        .ok_or("missing property arrival")?;
                    (
                        site.layout.path(slot).into.pose(motion.tick),
                        DrivewayPhase::Entering,
                    )
                }
                _ => return Err("invalid property parking route"),
            };
            let actor = world
                .vehicles
                .get(motion.car)
                .ok_or("missing moving property car")?;
            let (pos, collider) = actor
                .collider
                .and_then(|handle| grid.get(handle.0))
                .ok_or("missing property parking collider")?;
            if actor.trans != expected
                || actor.it.driveway_phase() != Some(phase)
                || !matches!(actor.vehicle.state, VehicleState::Driving)
                || actor.speed.0.abs() > 0.2
                || collider.group != TransportationGroup::Vehicles
                || pos.distance2(expected.pos.xy()) > 0.15_f32.powi(2)
                || (collider.height - expected.pos.z).abs() > 0.15
            {
                return Err("invalid property parking maneuver pose");
            }
        }
        if self.parked.iter().any(|(car, occupant)| {
            self.sites
                .get(&occupant.site)
                .is_none_or(|site| !site.occupied.contains_key(car))
        }) || self.arrivals.iter().any(|(car, arrival)| {
            self.sites
                .get(&arrival.site)
                .is_none_or(|site| !site.held.contains_key(car))
        }) {
            return Err("orphaned property parking claim");
        }
        Ok(())
    }
}

pub(crate) fn spawn_at_home(
    sim: &mut Simulation,
    home: BuildingID,
    driver: HumanID,
) -> Option<VehicleID> {
    use crate::transportation::DrivewayTraffic;
    let (slot, trans) = {
        let traffic = sim.read::<DrivewayTraffic>();
        let site = traffic.properties.sites.get(&home)?;
        if traffic.properties.parked.len() + traffic.properties.arrivals.len() >= MAX_LIVE_CLAIMS {
            return None;
        }
        let slot = site.free_slot()?;
        (slot, site.layout.path(slot).into.pose(MOTION_TICKS))
    };
    let mut rng = sim.write::<RandProvider>();
    let tint = crate::transportation::get_random_car_color(&mut rng);
    let car = Vehicle::with_state(
        VehicleKind::Car,
        VehicleState::ParkedAtDepot,
        tint,
        &mut rng,
    );
    drop(rng);
    let id = make_vehicle_entity(
        sim,
        trans,
        car,
        Itinerary::at_driveway(DrivewayPhase::Stored),
        false,
    );
    let parking = &mut sim.write::<DrivewayTraffic>().properties;
    parking
        .sites
        .get_mut(&home)
        .unwrap()
        .occupied
        .insert(id, slot);
    parking.parked.insert(id, Occupant { site: home, driver });
    Some(id)
}

impl Simulation {
    pub fn building_parking(&self, id: BuildingID) -> Option<BuildingParkingSummary> {
        self.read::<crate::transportation::DrivewayTraffic>()
            .properties
            .summary(id)
    }

    pub fn car_stored_inside_garage(&self, id: VehicleID) -> bool {
        self.read::<crate::transportation::DrivewayTraffic>()
            .properties
            .hidden(id)
    }

    pub fn building_curb_parking(&self, id: BuildingID) -> Option<(usize, usize)> {
        let map = self.map();
        let building = map.get(id)?;
        let road = map.get(building.connected_road?)?;
        let reservations = self.read::<ParkingManagement>();
        let mut capacity = 0;
        let mut unavailable = 0;
        for (lane, kind) in road.lanes_iter() {
            if kind != crate::map::LaneKind::Parking {
                continue;
            }
            for spot in map
                .parking
                .closest_spots(lane, building.door_pos)
                .into_iter()
                .flatten()
                .take(20)
            {
                if map.parking.get(spot).is_some_and(|spot| {
                    spot.trans.pos.distance2(building.door_pos) <= 45.0_f32.powi(2)
                }) {
                    capacity += 1;
                    unavailable += usize::from(!reservations.is_spot_free(spot));
                }
            }
        }
        (capacity != 0).then_some((capacity, unavailable))
    }

    pub fn parking_pavement(&self, id: BuildingID) -> Vec<[Vec3; 2]> {
        self.read::<crate::transportation::DrivewayTraffic>()
            .properties
            .sites
            .get(&id)
            .map(|site| site.layout.pavement().collect())
            .unwrap_or_default()
    }
}

pub(crate) fn refresh_property(
    sim: &Simulation,
    id: BuildingID,
    spec: Option<GrowableParking>,
    residential: bool,
) {
    use crate::transportation::DrivewayTraffic;
    let map = sim.map();
    let Some(site) = map.get(id) else { return };
    let Some(lot) = map.lot_origin(id) else {
        return;
    };
    let Some(shell) = site
        .development()
        .and_then(|site| site.frozen_shell.as_ref())
    else {
        return;
    };
    let mut traffic = sim.write::<DrivewayTraffic>();
    let parking = &mut sim.write::<ParkingManagement>();
    let properties = &mut traffic.properties;
    let layout = spec.and_then(|spec| {
        prepare_property_parking(
            &map,
            lot.parent_road,
            lot.shape,
            shell.fit().footprint(),
            site.door_pos,
            spec,
            residential,
            |layout| properties.fits(parking, layout, Some(id)),
        )
    });
    if properties.busy(id) {
        let Some(layout) = layout else { return };
        let current = &mut properties.sites.get_mut(&id).unwrap().layout;
        if current.kind == layout.kind
            && current.anchor == layout.anchor
            && current.paths == layout.paths
            && current.masks == layout.masks
            && layout.capacity >= current.capacity
        {
            current.capacity = layout.capacity;
        }
    } else if let Some(layout) = layout {
        let was_closed = properties.sites.get(&id).is_some_and(|site| site.closing);
        properties.install(id, layout, parking);
        if was_closed {
            properties.set_open(id, false);
        }
    } else {
        properties.remove_empty(id, parking);
    }
}

pub(super) fn garage_mesh(
    kind: GarageKind,
    shape: OBB,
    height: f32,
) -> crate::map::procgen::ColoredMesh {
    use geom::Color;
    let mut mesh = crate::map::procgen::ColoredMesh::default();
    let concrete = Color::from_hex(0xaba69c).into();
    let shadow = Color::from_hex(0x4b575e).into();
    let paving = Color::from_hex(0x74777a).into();
    let lawn = Color::from_hex(0x789363).into();
    let mut block = |shape: OBB, bottom: f32, top: f32, wall, roof| {
        for edge in shape.segments() {
            mesh.faces.push((
                vec![
                    edge.src.z(bottom),
                    edge.dst.z(bottom),
                    edge.dst.z(top),
                    edge.src.z(top),
                ],
                wall,
            ));
        }
        mesh.faces
            .push((shape.corners.into_iter().map(|p| p.z(top)).collect(), roof));
    };
    match kind {
        GarageKind::MultiStorey => {
            block(shape, height, height + 0.12, shadow, paving);
            let upper = OBB::new(
                shape.center(),
                (shape.corners[1] - shape.corners[0]).normalize(),
                25.0,
                24.0,
            );
            for floor in 0_u8..4 {
                let bottom = height + f32::from(floor) * 3.0;
                block(upper.expand(-0.6), bottom, bottom + 2.4, shadow, shadow);
                block(upper, bottom + 2.4, bottom + 3.0, concrete, paving);
            }
        }
        GarageKind::Underground => {
            block(shape, height, height + 0.12, concrete, paving);
            let direction = (shape.corners[1] - shape.corners[0]).normalize();
            for side in [-1.0, 1.0] {
                let planter = OBB::new(
                    shape.center() + direction * (side * 5.0),
                    direction,
                    2.7,
                    10.5,
                );
                block(planter, height, height + 0.45, concrete, lawn);
            }
        }
    }
    mesh
}
