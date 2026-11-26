pub use pathpark_driveway::*;

use crate::map_dynamic::{Itinerary, ParkingManagement, SpotReservation};
use crate::transportation::{TransportGrid, TransportState, TransportationGroup, Transporter};
use crate::utils::rand_provider::RandProvider;
use crate::world::{VehicleEnt, VehicleID};
use crate::Simulation;
use egui_inspect::Inspect;
use geom::Transform;
use geom::{Color, Spline3, Vec2, Vec3};
use prototypes::GameInstant;
use serde::{Deserialize, Serialize};

/// The duration for the parking animation.
pub const TIME_TO_PARK: f32 = 4.0;

#[derive(Debug, Serialize, Deserialize)]
pub enum VehicleState {
    Parked(SpotReservation),
    Driving,
    /// Panicked when it notices it's in a gridlock
    Panicking(GameInstant),
    RoadToPark(Spline3, f32, SpotReservation),
    ReturningToDepot {
        spot: SpotReservation,
        stopped_ticks: u16,
    },
    ParkedAtDepot,
}

impl VehicleState {
    pub fn is_parked(&self) -> bool {
        matches!(self, Self::Parked(_) | Self::ParkedAtDepot)
    }
}

debug_inspect_impl!(VehicleState);

#[derive(Copy, Clone, Debug, Serialize, Deserialize, Inspect)]
pub enum VehicleKind {
    Car,
    Truck,
    Bus,
}

#[derive(Debug, Serialize, Deserialize, Inspect)]
pub struct Vehicle {
    pub ang_velocity: f32,
    pub wait_time: f32,
    pub max_speed_multiplier: f32,

    pub state: VehicleState,
    pub kind: VehicleKind,
    pub tint: Color,

    /// Used to detect gridlock
    pub flag: u64,
}

#[must_use]
pub fn put_vehicle_in_transport_grid(sim: &Simulation, w: f32, trans: Transform) -> Transporter {
    Transporter(sim.write::<TransportGrid>().insert(
        trans.pos.xy(),
        TransportState {
            dir: trans.dir.xy(),
            radius: w * 0.5,
            group: TransportationGroup::Vehicles,
            ..Default::default()
        },
    ))
}

impl VehicleKind {
    pub fn width(self) -> f32 {
        match self {
            VehicleKind::Car => 4.5,
            VehicleKind::Truck => 6.0,
            VehicleKind::Bus => 9.0,
        }
    }

    pub fn acceleration(self) -> f32 {
        match self {
            VehicleKind::Car => 3.0,
            VehicleKind::Truck => 2.5,
            VehicleKind::Bus => 2.0,
        }
    }

    pub fn deceleration(self) -> f32 {
        match self {
            VehicleKind::Car | VehicleKind::Bus | VehicleKind::Truck => 6.0,
        }
    }

    pub fn min_turning_radius(self) -> f32 {
        match self {
            VehicleKind::Car => 0.5,
            VehicleKind::Truck => 3.0,
            VehicleKind::Bus => 4.0,
        }
    }

    pub fn speed_factor(self) -> f32 {
        match self {
            VehicleKind::Car => 1.0,
            VehicleKind::Truck | VehicleKind::Bus => 0.8,
        }
    }

    pub fn ang_acc(self) -> f32 {
        match self {
            VehicleKind::Car => 1.0,
            VehicleKind::Truck => 0.9,
            VehicleKind::Bus => 0.8,
        }
    }
}

impl VehicleEnt {
    pub(crate) fn begin_parking(&mut self, trans: Transform, spot: SpotReservation) {
        let spline = Spline3 {
            from: self.trans.pos,
            to: trans.pos,
            from_derivative: self.trans.dir * 2.0,
            to_derivative: trans.dir * 2.0,
        };
        self.vehicle.state = VehicleState::RoadToPark(spline, 0.0, spot);
        self.speed.0 = 0.0;
    }
}

pub fn unpark(sim: &mut Simulation, vehicle: VehicleID) {
    if sim.read::<DrivewayTraffic>().owns(vehicle) {
        return;
    }
    let v = unwrap_ret!(sim.world.vehicles.get_mut(vehicle));
    let w = v.vehicle.kind.width();
    let trans = v.trans;

    match std::mem::replace(&mut v.vehicle.state, VehicleState::Driving) {
        VehicleState::Parked(spot) => sim.write::<ParkingManagement>().free(spot),
        VehicleState::ParkedAtDepot => {}
        state => {
            v.vehicle.state = state;
            log::warn!("Trying to unpark {:?} that wasn't parked", vehicle);
            return;
        }
    }

    let coll = put_vehicle_in_transport_grid(sim, w, trans);

    let v = unwrap_ret!(sim.world.vehicles.get_mut(vehicle));
    v.collider = Some(coll);
}

pub fn spawn_parked_vehicle(
    sim: &mut Simulation,
    kind: VehicleKind,
    near: Vec3,
) -> Option<VehicleID> {
    let map = sim.map();
    let mut pm = sim.write::<ParkingManagement>();
    let spot_id = pm.reserve_near(near, &map).ok()?;
    drop((map, pm));

    spawn_parked_vehicle_with_spot(sim, kind, spot_id)
}

pub fn spawn_parked_vehicle_with_spot(
    sim: &mut Simulation,
    kind: VehicleKind,
    spot_id: SpotReservation,
) -> Option<VehicleID> {
    let map = sim.map();
    let it = Itinerary::NONE;
    let pos = spot_id.get(&map.parking).unwrap().trans; // Unwrap ok: Gotten using reserve_near
    drop(map);

    let tint = match kind {
        VehicleKind::Car => get_random_car_color(&mut sim.write::<RandProvider>()),
        _ => Color::WHITE,
    };

    let vehicle = Vehicle::new(kind, spot_id, tint, &mut sim.write::<RandProvider>());

    Some(make_vehicle_entity(sim, pos, vehicle, it, false))
}

pub fn spawn_driving_vehicle(
    sim: &mut Simulation,
    kind: VehicleKind,
    mut trans: Transform,
) -> Option<VehicleID> {
    let length_sq = trans.dir.mag2();
    if !trans.pos.is_finite() || !length_sq.is_finite() || trans.dir.xy().mag2() < f32::EPSILON {
        return None;
    }
    trans.dir /= length_sq.sqrt();

    let radius = kind.width() * 0.5;
    {
        let grid = sim.read::<TransportGrid>();
        let center = trans.pos.xy();
        let extent = Vec2::splat(radius + VehicleKind::Bus.width() * 0.5);
        if grid
            .query_aabb(center - extent, center + extent)
            .any(|(id, pos)| {
                grid.get(id).is_some_and(|(_, other)| {
                    other.group == TransportationGroup::Vehicles
                        && (trans.pos.z - other.height).abs() <= 5.0
                        && center.is_close(pos, radius + other.radius)
                })
            })
        {
            return None;
        }
    }

    let mut rng = sim.write::<RandProvider>();
    let tint = match kind {
        VehicleKind::Car => get_random_car_color(&mut rng),
        VehicleKind::Truck | VehicleKind::Bus => Color::WHITE,
    };
    let vehicle = Vehicle::with_state(kind, VehicleState::Driving, tint, &mut rng);
    drop(rng);

    let id = make_vehicle_entity(sim, trans, vehicle, Itinerary::NONE, true);
    let collider = sim.world.vehicles[id].collider.unwrap();
    sim.write::<TransportGrid>()
        .get_mut(collider.0)
        .unwrap()
        .1
        .height = trans.pos.z;
    Some(id)
}

pub(crate) fn spawn_truck_at_depot(sim: &mut Simulation, door: Vec3) -> VehicleID {
    let vehicle = Vehicle::with_state(
        VehicleKind::Truck,
        VehicleState::ParkedAtDepot,
        Color::WHITE,
        &mut sim.write::<RandProvider>(),
    );
    make_vehicle_entity(sim, Transform::new(door), vehicle, Itinerary::NONE, false)
}

pub fn make_vehicle_entity(
    sim: &mut Simulation,
    trans: Transform,
    vehicle: Vehicle,
    it: Itinerary,
    mk_collider: bool,
) -> VehicleID {
    let w = vehicle.kind.width();

    let mut collider = None;
    if mk_collider {
        collider = Some(put_vehicle_in_transport_grid(sim, w, trans));
    }
    sim.world.insert(VehicleEnt {
        trans,
        speed: Default::default(),
        vehicle,
        it,
        collider,
    })
}

pub fn get_random_car_color(r: &mut RandProvider) -> Color {
    let car_colors: [(Color, f32); 9] = [
        (Color::from_hex(0x22_22_22), 0.22),  // Black
        (Color::from_hex(0xff_ff_ff), 0.19),  // White
        (Color::from_hex(0x66_66_66), 0.17),  // Gray
        (Color::from_hex(0xb8_b8_b8), 0.14),  // Silver
        (Color::from_hex(0x1a_3c_70), 0.1),   // Blue
        (Color::from_hex(0xd8_22_00), 0.1),   // Red
        (Color::from_hex(0x7c_4b_24), 0.02),  // Brown
        (Color::from_hex(0xd4_c6_78), 0.015), // Gold
        (Color::from_hex(0x72_cb_19), 0.015), // Green
    ];

    let total: f32 = car_colors.iter().map(|x| x.1).sum();

    let r = r.next_f32() * total;
    let mut partial = 0.0;
    for (col, freq) in &car_colors {
        partial += freq;
        if partial >= r {
            return *col;
        }
    }
    unreachable!();
}

impl Vehicle {
    pub fn new(
        kind: VehicleKind,
        spot: SpotReservation,
        tint: Color,
        rng: &mut RandProvider,
    ) -> Vehicle {
        Self::with_state(kind, VehicleState::Parked(spot), tint, rng)
    }

    fn with_state(
        kind: VehicleKind,
        state: VehicleState,
        tint: Color,
        rng: &mut RandProvider,
    ) -> Self {
        Self {
            ang_velocity: 0.0,
            wait_time: 0.0,
            max_speed_multiplier: 0.95 + 0.1 * rng.next_f32(),
            state,
            kind,
            tint,
            flag: 0,
        }
    }
}

mod pathpark_driveway {
    use super::{
        TransportGrid, TransportState, TransportationGroup, Transporter, VehicleKind, VehicleState,
    };
    use crate::map::{CurbAnchor, LaneKind, Map, ProjectFilter, RoadID, RoadSide};
    use crate::map_dynamic::{DirectedEndpoint, DirectedPlan, DirectedSearch, DirectedSearchResult};
    use crate::utils::resources::Resources;
    use crate::world_command::bounded_command_vec;
    use crate::{Simulation, VehicleID, World};
    use geom::{Intersect, Segment3, Spline3, Transform, Vec3, AABB, OBB};
    use serde::{Deserialize, Serialize};
    use std::collections::BTreeSet;

    const MAX_PORTALS: usize = 8;
    const SWEEP_STEPS: usize = 16;
    const MAX_LOCAL_COLLIDERS: usize = 128;
    const MOTION_TICKS: u16 = 160;
    const EXIT_SEARCH_TICKS: u64 = 240;
    const SAME_DECK: f32 = 1.25;

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
    pub struct DrivewayPortal(u64);

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum DrivewayDirection {
        In,
        Out,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct DrivewayBoundary {
        pub portal: DrivewayPortal,
        pub direction: DrivewayDirection,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum DrivewayPhase {
        Held,
        Entering,
        Stored,
        Exiting,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum DrivewayBlocker {
        ReceivingSpace,
        NoRoute,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum DrivewayError {
        InvalidGeometry,
        Stale,
        Full,
        InvalidVehicle,
        InvalidRoute,
        Busy,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
    struct Sweep {
        points: [Vec3; SWEEP_STEPS + 1],
        end_dir: Vec3,
    }

    impl Sweep {
        fn between(from: Transform, to: Transform) -> Self {
            let radius = from.pos.distance(to.pos) * 0.9;
            let curve = Spline3 {
                from: from.pos,
                to: to.pos,
                from_derivative: from.dir * radius,
                to_derivative: to.dir * radius,
            };
            Self {
                points: std::array::from_fn(|i| curve.get(i as f32 / SWEEP_STEPS as f32)),
                end_dir: to.dir,
            }
        }

        fn pose(self, tick: u16) -> Transform {
            let step = tick as usize * SWEEP_STEPS;
            let index = step / MOTION_TICKS as usize;
            if index == SWEEP_STEPS {
                return Transform::new_dir(self.points[index], self.end_dir);
            }
            let [a, b] = [self.points[index], self.points[index + 1]];
            let t = (step % MOTION_TICKS as usize) as f32 / MOTION_TICKS as f32;
            Transform::new_dir(a + (b - a) * t, (b - a).normalize())
        }

        fn segments(&self) -> impl Iterator<Item = Segment3> + '_ {
            self.points.windows(2).map(|p| Segment3::new(p[0], p[1]))
        }

        fn bounds(&self) -> AABB {
            let (ll, ur) = self.points.iter().fold(
                (self.points[0].xy(), self.points[0].xy()),
                |(ll, ur), &p| (ll.min(p.xy()), ur.max(p.xy())),
            );
            AABB { ll, ur }
        }

        fn occupied(&self, grid: &TransportGrid, actor: Option<Transporter>) -> bool {
            let bounds = self.bounds().expand(8.0);
            for (index, (id, pos)) in grid.query_aabb(bounds.ll, bounds.ur).enumerate() {
                if index >= MAX_LOCAL_COLLIDERS {
                    return true;
                }
                if actor == Some(Transporter(id)) {
                    continue;
                }
                let Some((_, state)) = grid.get(id) else {
                    continue;
                };
                let radius = state.radius + car_extent() + 0.35;
                if self.segments().any(|segment| {
                    let point = segment.project(pos.z(state.height));
                    (point.z - state.height).abs() <= SAME_DECK
                        && point.xy().distance2(pos) < radius * radius
                }) {
                    return true;
                }
            }
            false
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
    struct Geometry {
        approach: DirectedEndpoint,
        release: DirectedEndpoint,
        into: Sweep,
        out: Sweep,
    }

    impl Geometry {
        fn prepare(map: &Map, anchor: CurbAnchor) -> Result<Self, DrivewayError> {
            use DrivewayError::*;
            let road = map.get(anchor.road).ok_or(Stale)?;
            if map.pick_curb(anchor.road, anchor.side, anchor.station_cm) != Some(anchor) {
                return Err(Stale);
            }
            // The proof uses a single passenger aisle and a curb without bays.
            if road
                .lanes_iter()
                .any(|(_, kind)| kind == LaneKind::Parking || kind.is_rail())
            {
                return Err(InvalidGeometry);
            }
            let (center, _) = map
                .path_station(road, anchor.station_cm)
                .ok_or(InvalidGeometry)?;
            let endpoint = match anchor.side {
                RoadSide::Right => road.dst,
                RoadSide::Left => road.src,
            };
            let lane = road
                .incoming_lanes_to(endpoint)
                .iter()
                .rev()
                .find(|(_, kind)| *kind == LaneKind::Driving)
                .and_then(|(lane, _)| map.get(*lane))
                .ok_or(InvalidGeometry)?;
            let (projection, index) = lane.points.project_segment_2d(center.xy());
            let segment_start = index.checked_sub(1).ok_or(InvalidGeometry)?;
            let offset = lane
                .points
                .segments()
                .take(segment_start)
                .map(|p| p.src.distance(p.dst))
                .sum::<f32>()
                + lane.points[segment_start].distance(projection);
            let along = (offset * 100.0).round() as u32;
            let before = along.checked_sub(550).ok_or(InvalidGeometry)?;
            let after = along.checked_add(850).ok_or(InvalidGeometry)?;
            let approach = DirectedEndpoint {
                lane: lane.id,
                offset_cm: before,
            };
            let release = DirectedEndpoint {
                lane: lane.id,
                offset_cm: after,
            };
            let a = approach.transform(map).ok_or(InvalidGeometry)?;
            let b = release.transform(map).ok_or(InvalidGeometry)?;
            let direction = (b.pos - a.pos)
                .xy()
                .try_normalize()
                .ok_or(InvalidGeometry)?;
            if a.dir.xy().dot(direction) < 0.99
                || b.dir.xy().dot(direction) < 0.99
                || (a.pos.z - b.pos.z).abs() > 0.35
            {
                return Err(InvalidGeometry);
            }
            let outward = direction.perpendicular();
            let storage = center + (outward * (road.width * 0.5 + 8.0)).z0();
            let park = Transform::new_dir(storage, direction.z0());
            let into = Sweep::between(a, park);
            let out = Sweep::between(park, b);

            let footprint = OBB::new(
                center.xy() + outward * (road.width * 0.25 + 4.0),
                direction,
                26.0,
                road.width * 0.5 + 20.0,
            );
            let env = map.environment();
            if !map.authorize_driveway_pad(&footprint.corners)
                || map
                    .spatial_map()
                    .query(
                        footprint,
                        ProjectFilter::ROAD | ProjectFilter::INTER | ProjectFilter::BUILDING,
                    )
                    .any(|obj| obj != anchor.road.into())
                || footprint.corners.iter().any(|&p| {
                    !env.bounds().contains(p)
                        || env.is_water(p)
                        || env
                            .height(p)
                            .is_none_or(|z| (z + crate::map::ROAD_Z_OFFSET - center.z).abs() > 0.6)
                })
                || into.points.into_iter().chain(out.points).any(|p| {
                    env.is_water(p.xy())
                        || env
                            .height(p.xy())
                            .is_none_or(|z| (z + crate::map::ROAD_Z_OFFSET - p.z).abs() > 0.6)
                })
            {
                return Err(InvalidGeometry);
            }
            Ok(Self {
                approach,
                release,
                into,
                out,
            })
        }
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    struct ExitRequest {
        search: DirectedSearch,
        deadline: u64,
    }

    #[derive(Debug, Serialize, Deserialize)]
    enum Motion {
        Held,
        Entering(u16),
        Stored {
            exit: Option<ExitRequest>,
            blocker: Option<DrivewayBlocker>,
        },
        Exiting {
            tick: u16,
            route: DirectedPlan,
        },
    }

    impl Motion {
        fn phase(&self) -> DrivewayPhase {
            match self {
                Self::Held => DrivewayPhase::Held,
                Self::Entering(_) => DrivewayPhase::Entering,
                Self::Stored { .. } => DrivewayPhase::Stored,
                Self::Exiting { .. } => DrivewayPhase::Exiting,
            }
        }

        fn sweep<'a>(&self, geometry: &'a Geometry) -> Option<(DrivewayDirection, &'a Sweep)> {
            match self {
                Self::Entering(_) => Some((DrivewayDirection::In, &geometry.into)),
                Self::Exiting { .. } => Some((DrivewayDirection::Out, &geometry.out)),
                _ => None,
            }
        }
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct Claim {
        vehicle: VehicleID,
        motion: Motion,
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct Facility {
        id: DrivewayPortal,
        anchor: CurbAnchor,
        geometry: Geometry,
        claim: Option<Claim>,
    }

    #[derive(Default, Serialize, Deserialize)]
    pub struct DrivewayTraffic {
        next_id: u64,
        #[serde(deserialize_with = "bounded_command_vec::<_, _, MAX_PORTALS>")]
        facilities: Vec<Facility>,
    }

    impl DrivewayPortal {
        pub fn place(sim: &mut Simulation, anchor: CurbAnchor) -> Result<Self, DrivewayError> {
            let geometry = Geometry::prepare(&sim.map(), anchor)?;
            let mut traffic = sim.write::<DrivewayTraffic>();
            if traffic.facilities.len() == MAX_PORTALS {
                return Err(DrivewayError::Full);
            }
            if traffic.facilities.iter().any(|f| {
                f.geometry
                    .into
                    .bounds()
                    .expand(6.0)
                    .intersects(&geometry.into.bounds())
            }) {
                return Err(DrivewayError::InvalidGeometry);
            }
            let next_id = traffic.next_id.checked_add(1).ok_or(DrivewayError::Full)?;
            let id = Self(next_id);
            traffic.facilities.push(Facility {
                id,
                anchor,
                geometry,
                claim: None,
            });
            traffic.next_id = next_id;
            Ok(id)
        }

        pub fn remove(self, sim: &mut Simulation) -> Result<(), DrivewayError> {
            let mut traffic = sim.write::<DrivewayTraffic>();
            let index = traffic.index(self)?;
            if traffic.facilities[index].claim.is_some() {
                return Err(DrivewayError::Busy);
            }
            traffic.facilities.remove(index);
            Ok(())
        }

        pub fn approach(self, sim: &Simulation) -> Option<Transform> {
            let traffic = sim.read::<DrivewayTraffic>();
            traffic.facilities[traffic.index(self).ok()?]
                .geometry
                .approach
                .transform(&sim.map())
        }

        pub fn release(self, sim: &Simulation) -> Option<Transform> {
            let traffic = sim.read::<DrivewayTraffic>();
            traffic.facilities[traffic.index(self).ok()?]
                .geometry
                .release
                .transform(&sim.map())
        }

        pub fn hold(self, sim: &mut Simulation, vehicle: VehicleID) -> Result<(), DrivewayError> {
            let approach = self.approach(sim).ok_or(DrivewayError::Stale)?;
            let actor = sim
                .world
                .vehicles
                .get(vehicle)
                .ok_or(DrivewayError::InvalidVehicle)?;
            if !matches!(actor.vehicle.kind, VehicleKind::Car)
                || !matches!(actor.vehicle.state, VehicleState::Driving)
                || actor.collider.is_none()
                || !actor.it.is_inert()
                || actor.speed.0.abs() > 0.2
                || actor.trans.pos.distance2(approach.pos) > 0.15_f32.powi(2)
                || actor.trans.dir.dot(approach.dir) < 0.99
            {
                return Err(DrivewayError::InvalidVehicle);
            }
            let mut traffic = sim.write::<DrivewayTraffic>();
            if traffic.owns(vehicle) {
                return Err(DrivewayError::Busy);
            }
            let index = traffic.index(self)?;
            let facility = &mut traffic.facilities[index];
            if facility.claim.is_some() {
                return Err(DrivewayError::Full);
            }
            facility.claim = Some(Claim {
                vehicle,
                motion: Motion::Held,
            });
            Ok(())
        }

        pub fn cancel_hold(
            self,
            sim: &mut Simulation,
            vehicle: VehicleID,
        ) -> Result<(), DrivewayError> {
            let mut traffic = sim.write::<DrivewayTraffic>();
            let index = traffic.index(self)?;
            let facility = &mut traffic.facilities[index];
            if !facility
                .claim
                .as_ref()
                .is_some_and(|c| c.vehicle == vehicle && matches!(c.motion, Motion::Held))
            {
                return Err(DrivewayError::Busy);
            }
            facility.claim = None;
            Ok(())
        }

        pub fn depart(
            self,
            sim: &mut Simulation,
            vehicle: VehicleID,
            target: DirectedEndpoint,
        ) -> Result<(), DrivewayError> {
            target
                .transform(&sim.map())
                .ok_or(DrivewayError::InvalidRoute)?;
            let mut traffic = sim.write::<DrivewayTraffic>();
            let index = traffic.index(self)?;
            let facility = &mut traffic.facilities[index];
            let claim = facility
                .claim
                .as_mut()
                .filter(|c| c.vehicle == vehicle)
                .ok_or(DrivewayError::InvalidVehicle)?;
            let Motion::Stored { exit, blocker } = &mut claim.motion else {
                return Err(DrivewayError::Busy);
            };
            if exit.is_some() {
                return Err(DrivewayError::Busy);
            }
            *blocker = None;
            *exit = Some(ExitRequest {
                search: DirectedSearch::new(facility.geometry.release, target),
                deadline: crate::map_dynamic::transit::elapsed_ticks(&sim.resources)
                    .0
                    .saturating_add(EXIT_SEARCH_TICKS),
            });
            Ok(())
        }
    }

    impl DrivewayTraffic {
        fn index(&self, portal: DrivewayPortal) -> Result<usize, DrivewayError> {
            self.facilities
                .binary_search_by_key(&portal, |f| f.id)
                .map_err(|_| DrivewayError::Stale)
        }

        pub fn phase(&self, portal: DrivewayPortal) -> Option<DrivewayPhase> {
            Some(
                self.facilities[self.index(portal).ok()?]
                    .claim
                    .as_ref()?
                    .motion
                    .phase(),
            )
        }

        pub fn blocker(&self, portal: DrivewayPortal) -> Option<DrivewayBlocker> {
            let claim = self.facilities[self.index(portal).ok()?].claim.as_ref()?;
            match claim.motion {
                Motion::Stored { blocker, .. } => blocker,
                _ => None,
            }
        }

        pub fn crossing(&self, portal: DrivewayPortal) -> Option<DrivewayBoundary> {
            let facility = &self.facilities[self.index(portal).ok()?];
            let (direction, _) = facility.claim.as_ref()?.motion.sweep(&facility.geometry)?;
            Some(DrivewayBoundary { portal, direction })
        }

        pub(crate) fn owns(&self, vehicle: VehicleID) -> bool {
            self.facilities
                .iter()
                .any(|f| f.claim.as_ref().is_some_and(|c| c.vehicle == vehicle))
        }

        pub(crate) fn protects(&self, road: RoadID) -> bool {
            self.facilities.iter().any(|f| f.anchor.road == road)
        }

        pub(crate) fn overlaps(&self, bounds: &AABB) -> bool {
            self.facilities.iter().any(|f| {
                f.geometry
                    .into
                    .bounds()
                    .union(f.geometry.out.bounds())
                    .expand(8.0)
                    .intersects(bounds)
            })
        }

        pub(crate) fn is_empty(&self) -> bool {
            self.facilities.is_empty()
        }

        pub(crate) fn vehicle_removed(&mut self, vehicle: VehicleID) {
            for facility in &mut self.facilities {
                if facility
                    .claim
                    .as_ref()
                    .is_some_and(|claim| claim.vehicle == vehicle)
                {
                    facility.claim = None;
                }
            }
        }

        pub(crate) fn blocks_step(&self, actor: Option<VehicleID>, from: Vec3, to: Vec3) -> bool {
            let radius = if actor.is_some() {
                car_extent() * 2.0
            } else {
                car_extent() + 0.3
            };
            self.facilities.iter().any(|f| {
                let Some(claim) = f.claim.as_ref().filter(|c| Some(c.vehicle) != actor) else {
                    return false;
                };
                let Some((_, sweep)) = claim.motion.sweep(&f.geometry) else {
                    return false;
                };
                let min = from.xy().min(to.xy());
                let max = from.xy().max(to.xy());
                if !sweep
                    .bounds()
                    .expand(radius)
                    .intersects(&AABB { ll: min, ur: max })
                {
                    return false;
                }
                let movement = Segment3::new(from, to);
                sweep.segments().any(|segment| {
                    let near = |point: Vec3, swept: Segment3| {
                        let p = swept.project(point);
                        (point.z - p.z).abs() < SAME_DECK
                            && point.xy().distance2(p.xy()) < radius * radius
                    };
                    // Let an actor already on the edge move out under the retained grant.
                    !near(from, segment) && (near(to, segment) || near(segment.src, movement))
                })
            })
        }

        pub(crate) fn update(world: &mut World, resources: &mut Resources) {
            let mut traffic = resources.write::<Self>();
            if traffic.is_empty() {
                return;
            }
            let map = resources.read::<Map>();
            let now = crate::map_dynamic::transit::elapsed_ticks(resources).0;
            let mut grid = resources.write::<TransportGrid>();
            for facility in &mut traffic.facilities {
                if facility.advance(world, &map, &mut grid, now) {
                    facility.claim = None;
                }
            }
        }

        pub(crate) fn validate_loaded(
            &mut self,
            map: &Map,
            world: &World,
            grid: &TransportGrid,
        ) -> Result<(), &'static str> {
            let mut owners = BTreeSet::new();
            if !self.facilities.windows(2).all(|p| p[0].id < p[1].id) {
                return Err("unordered driveway identities");
            }
            for facility in &mut self.facilities {
                if facility.id.0 == 0
                    || facility.id.0 > self.next_id
                    || Geometry::prepare(map, facility.anchor).ok() != Some(facility.geometry)
                {
                    return Err("invalid saved driveway geometry");
                }
                let Some(claim) = &mut facility.claim else {
                    continue;
                };
                let actor = world
                    .vehicles
                    .get(claim.vehicle)
                    .ok_or("missing driveway vehicle")?;
                if !owners.insert(claim.vehicle) || !matches!(actor.vehicle.kind, VehicleKind::Car) {
                    return Err("invalid driveway vehicle");
                }
                let collider = actor.collider.and_then(|handle| grid.get(handle.0));
                let on_road = collider.is_some_and(|(_, c)| c.group == TransportationGroup::Vehicles)
                    && matches!(actor.vehicle.state, VehicleState::Driving)
                    && actor.it.is_inert();
                let valid = match &mut claim.motion {
                    Motion::Held => {
                        on_road
                            && actor.trans.pos.distance2(facility.geometry.into.points[0])
                                <= 0.15_f32.powi(2)
                    }
                    Motion::Entering(tick) => {
                        on_road
                            && *tick < MOTION_TICKS
                            && actor
                                .trans
                                .pos
                                .distance2(facility.geometry.into.pose(*tick).pos)
                                <= 0.15_f32.powi(2)
                    }
                    Motion::Exiting { tick, route } => {
                        on_road
                            && *tick < MOTION_TICKS
                            && actor
                                .trans
                                .pos
                                .distance2(facility.geometry.out.pose(*tick).pos)
                                <= 0.15_f32.powi(2)
                            && route.origin() == facility.geometry.release
                            && route.itinerary(map).is_some()
                    }
                    Motion::Stored { exit, .. } => {
                        if let Some(request) = exit {
                            let (from, _) = request.search.endpoints();
                            if from != facility.geometry.release {
                                return Err("invalid saved driveway departure");
                            }
                            request.search.restart_after_load();
                        }
                        actor.collider.is_none()
                            && matches!(actor.vehicle.state, VehicleState::ParkedAtDepot)
                            && actor.it.is_inert()
                            && actor.trans.pos == facility.geometry.out.points[0]
                    }
                };
                if !valid {
                    return Err("invalid saved driveway vehicle stage");
                }
            }
            Ok(())
        }
    }

    impl Facility {
        fn advance(
            &mut self,
            world: &mut World,
            map: &Map,
            grid: &mut TransportGrid,
            now: u64,
        ) -> bool {
            let Some(claim) = &mut self.claim else {
                return false;
            };
            let Some(actor) = world.vehicles.get_mut(claim.vehicle) else {
                return true;
            };
            let geometry = self.geometry;
            match &mut claim.motion {
                Motion::Held => {
                    if !geometry.into.occupied(grid, actor.collider) {
                        actor.trans = geometry.into.pose(0);
                        claim.motion = Motion::Entering(0);
                    }
                }
                Motion::Entering(tick) => {
                    *tick += 1;
                    actor.trans = geometry.into.pose(*tick);
                    if *tick == MOTION_TICKS {
                        if let Some(collider) = actor.collider.take() {
                            grid.remove_maintain(collider.0);
                        }
                        actor.vehicle.state = VehicleState::ParkedAtDepot;
                        claim.motion = Motion::Stored {
                            exit: None,
                            blocker: None,
                        };
                    }
                }
                Motion::Stored { exit, blocker } => {
                    let Some(request) = exit else {
                        return false;
                    };
                    let occupied = geometry.out.occupied(grid, None);
                    if request.deadline <= now {
                        *exit = None;
                        *blocker = Some(if occupied {
                            DrivewayBlocker::ReceivingSpace
                        } else {
                            DrivewayBlocker::NoRoute
                        });
                        return false;
                    }
                    if occupied {
                        *blocker = Some(DrivewayBlocker::ReceivingSpace);
                        return false;
                    }
                    *blocker = None;
                    match request.search.advance(map, &mut 32) {
                        DirectedSearchResult::Deferred => {}
                        DirectedSearchResult::Unavailable => {
                            *exit = None;
                            *blocker = Some(DrivewayBlocker::NoRoute);
                        }
                        DirectedSearchResult::Feasible(route) => {
                            let collider = grid.insert(
                                actor.trans.pos.xy(),
                                TransportState {
                                    dir: actor.trans.dir.xy(),
                                    height: actor.trans.pos.z,
                                    radius: car_extent(),
                                    group: TransportationGroup::Vehicles,
                                    ..Default::default()
                                },
                            );
                            actor.collider = Some(Transporter(collider));
                            actor.vehicle.state = VehicleState::Driving;
                            claim.motion = Motion::Exiting { tick: 0, route };
                        }
                    }
                }
                Motion::Exiting { tick, route } => {
                    *tick += 1;
                    actor.trans = geometry.out.pose(*tick);
                    if *tick == MOTION_TICKS {
                        actor.it = route.itinerary(map).unwrap_or_default();
                        sync_actor(grid, actor);
                        // The receiving point includes a full car extent beyond the merge.
                        return true;
                    }
                }
            }
            sync_actor(grid, actor);
            false
        }
    }

    fn sync_actor(grid: &mut TransportGrid, car: &mut crate::world::VehicleEnt) {
        car.speed.0 = 0.0;
        if let Some(handle) = car.collider {
            grid.set_position(handle.0, car.trans.pos.xy());
            if let Some((_, state)) = grid.get_mut(handle.0) {
                state.dir = car.trans.dir.xy();
                state.height = car.trans.pos.z;
                state.speed = 0.0;
            }
            grid.maintain_deterministic();
        }
    }

    fn car_extent() -> f32 {
        VehicleKind::Car.width() * 0.5
    }
}
