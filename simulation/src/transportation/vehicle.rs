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

    pub(crate) fn with_state(
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
    use crate::map::{
        CurbAnchor, LaneKind, Map, PreparedWaterEdit, ProjectFilter, ProjectKind, RoadID, RoadSide,
        WaterEditError,
    };
    use crate::map_dynamic::{
        DirectedEndpoint, DirectedPlan, DirectedSearch, DirectedSearchResult, Itinerary,
    };
    use crate::utils::resources::Resources;
    use crate::world_command::bounded_command_vec;
    use crate::{Simulation, VehicleID, World};
    use common::saveload::{Bincode, Encoder};
    use flat_spatial::grid::GridHandle;
    use geom::{Intersect, Segment3, Shape, Spline3, Transform, Vec2, Vec3, AABB, OBB};
    use serde::{Deserialize, Serialize};
    use std::collections::{BTreeMap, BTreeSet};

    const MAX_PORTALS: usize = 8;
    const SWEEP_STEPS: usize = 16;
    const MAX_LOCAL_COLLIDERS: usize = 128;
    pub(crate) const MOTION_TICKS: u16 = 160;
    const EXIT_SEARCH_TICKS: u64 = 64;
    const EXIT_WORK_PER_TICK: usize = 32;
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

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
        BudgetExhausted,
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
    pub(crate) struct Sweep {
        pub(crate) points: [Vec3; SWEEP_STEPS + 1],
        end_dir: Vec3,
    }

    impl Sweep {
        pub(crate) fn between(from: Transform, to: Transform) -> Self {
            Self::with_tangent(from, to, from.pos.distance(to.pos) * 0.9)
        }

        pub(crate) fn with_tangent(from: Transform, to: Transform, tangent_m: f32) -> Self {
            let curve = Spline3 {
                from: from.pos,
                to: to.pos,
                from_derivative: from.dir * tangent_m,
                to_derivative: to.dir * tangent_m,
            };
            Self {
                points: std::array::from_fn(|i| curve.get(i as f32 / SWEEP_STEPS as f32)),
                end_dir: to.dir,
            }
        }

        pub(crate) fn pose(self, tick: u16) -> Transform {
            let step = tick as usize * SWEEP_STEPS;
            let index = step / MOTION_TICKS as usize;
            if index == SWEEP_STEPS {
                return Transform::new_dir(self.points[index], self.end_dir);
            }
            let [a, b] = [self.points[index], self.points[index + 1]];
            let t = (step % MOTION_TICKS as usize) as f32 / MOTION_TICKS as f32;
            Transform::new_dir(a + (b - a) * t, (b - a).normalize())
        }

        pub(crate) fn segments(&self) -> impl Iterator<Item = Segment3> + '_ {
            self.points.windows(2).map(|p| Segment3::new(p[0], p[1]))
        }

        pub(crate) fn bounds(&self) -> AABB {
            let (ll, ur) = self.points.iter().fold(
                (self.points[0].xy(), self.points[0].xy()),
                |(ll, ur), &p| (ll.min(p.xy()), ur.max(p.xy())),
            );
            AABB { ll, ur }
        }

        pub(crate) fn occupied(&self, grid: &TransportGrid, actor: Option<Transporter>) -> bool {
            let Some(samples) = self.sample_grid(grid, actor, || true) else {
                return true;
            };
            samples
                .iter()
                .any(|sample| (0..SWEEP_STEPS).any(|segment| self.hits(segment, sample)))
        }

        fn sample_grid(
            &self,
            grid: &TransportGrid,
            actor: Option<Transporter>,
            mut charge: impl FnMut() -> bool,
        ) -> Option<Vec<SweepSample>> {
            let centerline = self.bounds();
            let bounds = centerline.expand(8.0);
            let mut nearby = grid.query(bounds.ll, bounds.ur);
            let mut samples = Vec::new();
            for index in 0..=MAX_LOCAL_COLLIDERS {
                if !charge() {
                    return None;
                }
                let Some((id, pos)) = nearby.next() else {
                    return Some(samples);
                };
                if index == MAX_LOCAL_COLLIDERS {
                    return None;
                }
                if !bounds.contains(pos) || actor == Some(Transporter(id)) {
                    continue;
                }
                let Some((_, state)) = grid.get(id) else {
                    continue;
                };
                let radius = state.radius + car_extent() + 0.35;
                if centerline.expand(radius).contains(pos) {
                    samples.push(SweepSample {
                        id,
                        pos,
                        height: state.height,
                        radius,
                    });
                }
            }
            None
        }

        fn hits(&self, segment: usize, sample: &SweepSample) -> bool {
            let segment = Segment3::new(self.points[segment], self.points[segment + 1]);
            let point = segment.project(sample.pos.z(sample.height));
            (point.z - sample.height).abs() <= SAME_DECK
                && point.xy().distance2(sample.pos) < sample.radius * sample.radius
        }
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    struct SweepSample {
        id: GridHandle,
        pos: Vec2,
        height: f32,
        radius: f32,
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    struct SweepCheck {
        #[serde(deserialize_with = "bounded_command_vec::<_, _, MAX_LOCAL_COLLIDERS>")]
        samples: Vec<SweepSample>,
        candidate: usize,
        segment: usize,
    }

    impl SweepCheck {
        fn advance(&mut self, sweep: &Sweep, mut charge: impl FnMut() -> bool) -> Option<bool> {
            while let Some(sample) = self.samples.get(self.candidate) {
                if !charge() {
                    return None;
                }
                if sweep.hits(self.segment, sample) {
                    return Some(true);
                }
                self.segment += 1;
                if self.segment == SWEEP_STEPS {
                    self.segment = 0;
                    self.candidate += 1;
                }
            }
            Some(false)
        }

        fn refresh(&mut self, samples: Vec<SweepSample>) {
            if self.samples == samples {
                return;
            }
            // The old view has been fully checked. Reuse unchanged samples
            // even when the grid has reordered its cell entries.
            let cleared: BTreeMap<_, _> = self
                .samples
                .iter()
                .map(|sample| (sample.id, sample))
                .collect();
            let (mut unchanged, changed): (Vec<_>, Vec<_>) = samples
                .into_iter()
                .partition(|sample| cleared.get(&sample.id) == Some(&sample));
            self.candidate = unchanged.len();
            unchanged.extend(changed);
            self.samples = unchanged;
        }

        fn valid(&self, sweep: &Sweep) -> bool {
            self.segment < SWEEP_STEPS
                && (self.candidate < self.samples.len()
                    || (self.candidate == self.samples.len() && self.segment == 0))
                && self.samples.iter().enumerate().all(|(candidate, sample)| {
                    let checked = match candidate.cmp(&self.candidate) {
                        std::cmp::Ordering::Less => SWEEP_STEPS,
                        std::cmp::Ordering::Equal => self.segment,
                        std::cmp::Ordering::Greater => 0,
                    };
                    sample.pos.x.is_finite()
                        && sample.pos.y.is_finite()
                        && sample.height.is_finite()
                        && sample.radius.is_finite()
                        && (0..checked).all(|segment| !sweep.hits(segment, sample))
                })
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
    struct Geometry {
        approach: DirectedEndpoint,
        release: DirectedEndpoint,
        into: Sweep,
        out: Sweep,
        footprint: AABB,
    }

    pub(crate) fn prepare_driveway_mouth(
        map: &Map,
        anchor: CurbAnchor,
    ) -> Result<
        (
            Vec3,
            DirectedEndpoint,
            DirectedEndpoint,
            Transform,
            Transform,
            Vec2,
        ),
        DrivewayError,
    > {
        use DrivewayError::*;
        let road = map.get(anchor.road).ok_or(Stale)?;
        if map.pick_curb(anchor.road, anchor.side, anchor.station_cm) != Some(anchor)
            || road.lanes_iter().any(|(_, kind)| kind.is_rail())
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
        Ok((center, approach, release, a, b, direction))
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
            let (center, approach, release, a, b, direction) = prepare_driveway_mouth(map, anchor)?;
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
                footprint: footprint.bbox(),
            })
        }
    }

    impl Geometry {
        fn conflicts(self, other: Self) -> bool {
            self.into
                .bounds()
                .expand(6.0)
                .intersects(&other.into.bounds())
        }
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    struct ExitRequest {
        search: DirectedSearch,
        ready: Option<DirectedPlan>,
        cleanup_work: usize,
        deadline: u64,
        #[serde(skip)] // Stored in the optional driveway snapshot extension.
        sweep: Option<SweepCheck>,
    }

    impl ExitRequest {
        fn check_sweep(
            &mut self,
            sweep: &Sweep,
            grid: &TransportGrid,
            slice: &mut usize,
        ) -> Option<bool> {
            if *slice == 0 {
                return None;
            }
            // Enumerate one bounded local view atomically. Charge all cell records
            // to the request lifetime, with their own cap alongside the segment slice.
            let mut local = MAX_LOCAL_COLLIDERS + 1;
            let fresh = self.sweep.is_none();
            if fresh {
                let samples = sweep.sample_grid(grid, None, || self.search.charge(&mut local))?;
                self.sweep = Some(SweepCheck {
                    samples,
                    candidate: 0,
                    segment: 0,
                });
            }
            let check = self.sweep.as_mut().unwrap();
            let mut blocked = check.advance(sweep, || self.search.charge(slice))?;
            if !blocked && !fresh {
                // Across slices the grid can change. Only grant against a complete
                // current view, including new arrivals and changes in height or radius.
                let samples = sweep.sample_grid(grid, None, || self.search.charge(&mut local))?;
                check.refresh(samples);
                blocked = check.advance(sweep, || self.search.charge(slice))?;
            }
            if blocked {
                self.sweep = None;
            }
            Some(blocked)
        }
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
        #[serde(skip)]
        pub(crate) properties: crate::map::building_parking::BuildingParking,
    }

    #[derive(Deserialize)]
    #[serde(transparent)]
    struct SavedSweepChecks(
        #[serde(deserialize_with = "bounded_command_vec::<_, _, MAX_PORTALS>")]
        Vec<(DrivewayPortal, SweepCheck)>,
    );

    impl DrivewayPortal {
        pub fn place(sim: &mut Simulation, anchor: CurbAnchor) -> Result<Self, DrivewayError> {
            let geometry = Geometry::prepare(&sim.map(), anchor)?;
            let mut traffic = sim.write::<DrivewayTraffic>();
            if traffic
                .properties
                .overlapping_road(&geometry.footprint)
                .is_some()
            {
                return Err(DrivewayError::InvalidGeometry);
            }
            if traffic.facilities.len() == MAX_PORTALS {
                return Err(DrivewayError::Full);
            }
            if traffic
                .facilities
                .iter()
                .any(|f| f.geometry.conflicts(geometry))
            {
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
            if traffic.owns(vehicle) || traffic.properties.parked_at(vehicle).is_some() {
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
            drop(traffic);
            sim.world.vehicles[vehicle].it = Itinerary::at_driveway(DrivewayPhase::Held);
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
            drop(traffic);
            sim.world.vehicles[vehicle].it = Itinerary::NONE;
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
                ready: None,
                cleanup_work: 0,
                deadline: crate::map_dynamic::transit::elapsed_ticks(&sim.resources)
                    .0
                    .saturating_add(EXIT_SEARCH_TICKS),
                sweep: None,
            });
            Ok(())
        }
    }

    impl DrivewayTraffic {
        pub(crate) fn save(
            &self,
            revision: Option<(u64, u64)>,
            output: impl std::io::Write,
        ) -> std::io::Result<()> {
            let pending: Vec<_> = self
                .facilities
                .iter()
                .filter_map(|facility| {
                    let Motion::Stored {
                        exit: Some(request),
                        ..
                    } = &facility.claim.as_ref()?.motion
                    else {
                        return None;
                    };
                    Some((facility.id, request.sweep.as_ref()?))
                })
                .collect();
            if pending.is_empty() {
                Bincode::encode_writer(&(self, revision), output)
            } else {
                Bincode::encode_writer(&(self, revision, pending), output)
            }
        }

        pub(crate) fn load(data: &[u8]) -> std::io::Result<(Self, Option<(u64, u64)>)> {
            if let Ok(legacy) = Bincode::decode(data) {
                return Ok(legacy);
            }
            let (mut traffic, revision, SavedSweepChecks(checks)): (Self, _, _) =
                Bincode::decode(data)?;
            for (portal, check) in checks {
                let index = traffic
                    .index(portal)
                    .map_err(|_| std::io::Error::other("unknown saved driveway sweep"))?;
                let facility = &mut traffic.facilities[index];
                let Some(Claim {
                    motion:
                        Motion::Stored {
                            exit: Some(request),
                            ..
                        },
                    ..
                }) = &mut facility.claim
                else {
                    return Err(std::io::Error::other(
                        "saved driveway sweep has no departure",
                    ));
                };
                if request.sweep.is_some()
                    || request.ready.is_none()
                    || !check.valid(&facility.geometry.out)
                {
                    return Err(std::io::Error::other("invalid saved driveway sweep"));
                }
                request.sweep = Some(check);
            }
            Ok((traffic, revision))
        }

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

        pub(crate) fn protected_roads(&self, map: &Map, retire_vacant: bool) -> BTreeSet<RoadID> {
            let mut roads = BTreeSet::new();
            for facility in &self.facilities {
                roads.insert(facility.anchor.road);
                if let Some(Claim {
                    motion: Motion::Exiting { route, .. },
                    ..
                }) = &facility.claim
                {
                    roads.extend(route.roads(map));
                }
            }
            self.properties.protected_roads(map, &mut roads, retire_vacant);
            roads
        }

        pub(crate) fn overlaps(&self, bounds: &AABB) -> bool {
            self.properties.overlapping_road(bounds).is_some() || self.overlaps_proof_pad(bounds)
        }

        pub(crate) fn overlaps_proof_pad(&self, bounds: &AABB) -> bool {
            self.facilities
                .iter()
                .any(|f| f.geometry.footprint.intersects(bounds))
        }

        pub(crate) fn guard_water(
            &self,
            prepared: &PreparedWaterEdit,
            remaining: &mut usize,
        ) -> Result<(), WaterEditError> {
            if self.is_empty() || !prepared.changed() {
                return Ok(());
            }
            for tile in prepared.affected_tiles() {
                let ll = tile.corner();
                let bounds = AABB {
                    ll,
                    ur: tile.center() * 2.0 - ll,
                };
                crate::map::spend_water_work(remaining, self.properties.site_count())?;
                if let Some(road) = self.properties.overlapping_road(&bounds) {
                    return Err(WaterEditError::TopologyConflict {
                        object: ProjectKind::Road(road),
                        tile,
                    });
                }
                for facility in &self.facilities {
                    crate::map::spend_water_work(remaining, 1)?;
                    if facility.geometry.footprint.intersects(&bounds) {
                        return Err(WaterEditError::TopologyConflict {
                            object: ProjectKind::Road(facility.anchor.road),
                            tile,
                        });
                    }
                }
            }
            Ok(())
        }

        pub(crate) fn proof_is_empty(&self) -> bool {
            self.facilities.is_empty()
        }

        pub(crate) fn is_empty(&self) -> bool {
            self.proof_is_empty() && self.properties.is_empty()
        }

        pub(crate) fn has_crossing(&self) -> bool {
            self.properties.has_crossing()
                || self.facilities.iter().any(|facility| {
                    facility.claim.as_ref().is_some_and(|claim| {
                        matches!(claim.motion, Motion::Entering(_) | Motion::Exiting { .. })
                    })
                })
        }

        pub(crate) fn vehicle_removed(&mut self, vehicle: VehicleID) {
            self.properties.vehicle_removed(vehicle);
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

        pub(crate) fn blocks_step(
            &self,
            actor: Option<VehicleID>,
            half_extent: f32,
            from: Vec3,
            to: Vec3,
        ) -> bool {
            let radius = car_extent() + half_extent + 0.35;
            let blocked = |sweep: &Sweep| {
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
                    !near(from, segment)
                        && (near(to, segment)
                            || near(segment.src, movement)
                            || near(segment.dst, movement)
                            || {
                                let a = movement.dst - movement.src;
                                let b = segment.dst - segment.src;
                                let delta = segment.src - movement.src;
                                let cross = |u: Vec3, v: Vec3| u.x * v.y - u.y * v.x;
                                let divisor = cross(a, b);
                                if divisor.abs() <= f32::EPSILON {
                                    false
                                } else {
                                    let t = cross(delta, b) / divisor;
                                    let u = cross(delta, a) / divisor;
                                    (0.0..=1.0).contains(&t)
                                        && (0.0..=1.0).contains(&u)
                                        && (from.z + a.z * t - segment.src.z - b.z * u).abs()
                                            < SAME_DECK
                                }
                            })
                })
            };
            self.facilities.iter().any(|f| {
                f.claim
                    .as_ref()
                    .filter(|c| Some(c.vehicle) != actor)
                    .and_then(|c| c.motion.sweep(&f.geometry))
                    .is_some_and(|(_, sweep)| blocked(sweep))
            }) || self
                .properties
                .active_sweeps()
                .any(|(car, sweep)| Some(car) != actor && blocked(&sweep))
        }

        pub(crate) fn update(world: &mut World, resources: &mut Resources) {
            let mut traffic = resources.write::<Self>();
            if traffic.proof_is_empty() && !traffic.properties.has_runtime_work() {
                return;
            }
            let map = resources.read::<Map>();
            let now = crate::map_dynamic::transit::elapsed_ticks(resources).0;
            let mut grid = resources.write::<TransportGrid>();
            traffic.properties.update(world, &mut grid);
            for facility in &mut traffic.facilities {
                if facility.advance(world, &map, &mut grid, now) {
                    facility.claim = None;
                }
            }
        }

        pub(crate) fn validate_loaded(
            &self,
            map: &Map,
            world: &World,
            grid: &TransportGrid,
            now: u64,
        ) -> Result<(), &'static str> {
            let mut owners = BTreeSet::new();
            let mut claim_colliders = BTreeSet::new();
            if !self.facilities.windows(2).all(|p| p[0].id < p[1].id) {
                return Err("unordered driveway identities");
            }
            for (index, facility) in self.facilities.iter().enumerate() {
                if facility.id.0 == 0
                    || facility.id.0 > self.next_id
                    || Geometry::prepare(map, facility.anchor).ok() != Some(facility.geometry)
                    || self
                        .properties
                        .overlapping_road(&facility.geometry.footprint)
                        .is_some()
                {
                    return Err("invalid saved driveway geometry");
                }
                if self.facilities[..index]
                    .iter()
                    .any(|other| other.geometry.conflicts(facility.geometry))
                {
                    return Err("overlapping saved driveway pads");
                }
                let Some(claim) = &facility.claim else {
                    continue;
                };
                let actor = world
                    .vehicles
                    .get(claim.vehicle)
                    .ok_or("missing driveway vehicle")?;
                if !owners.insert(claim.vehicle)
                    || !matches!(actor.vehicle.kind, VehicleKind::Car)
                    || actor.it.driveway_phase() != Some(claim.motion.phase())
                {
                    return Err("invalid driveway vehicle");
                }

                let expected = match &claim.motion {
                    Motion::Held => facility.geometry.approach.transform(map),
                    Motion::Entering(tick) if *tick < MOTION_TICKS => {
                        Some(facility.geometry.into.pose(*tick))
                    }
                    Motion::Exiting { tick, route }
                        if *tick < MOTION_TICKS
                            && route.origin() == facility.geometry.release
                            && route.valid(map) =>
                    {
                        Some(facility.geometry.out.pose(*tick))
                    }
                    Motion::Stored { exit, blocker } => {
                        if exit.as_ref().is_some_and(|request| {
                            request.search.endpoints().0 != facility.geometry.release
                                || request.deadline <= now
                                || request.deadline.saturating_sub(now) > EXIT_SEARCH_TICKS
                                || request.search.exhausted()
                                || (request.cleanup_work != 0
                                    && (!request.search.stale(map)
                                        || request.ready.as_ref().is_none_or(|plan| {
                                            request.cleanup_work >= plan.retained_work()
                                        })))
                                || matches!(
                                    blocker,
                                    Some(
                                        DrivewayBlocker::NoRoute | DrivewayBlocker::BudgetExhausted
                                    )
                                )
                                || !request.search.validate_loaded(map, request.ready.as_ref())
                        }) {
                            return Err("invalid saved driveway departure");
                        }
                        if actor.collider.is_some()
                            || !matches!(actor.vehicle.state, VehicleState::ParkedAtDepot)
                            || actor.trans != facility.geometry.into.pose(MOTION_TICKS)
                            || !(actor.speed.0.abs() <= 0.001)
                        {
                            return Err("invalid stored driveway vehicle");
                        }
                        continue;
                    }
                    _ => None,
                };
                let expected = expected.ok_or("invalid saved driveway vehicle stage")?;
                let handle = actor.collider.ok_or("missing driveway collider")?.0;
                let (pos, collider) = grid.get(handle).ok_or("missing driveway collider")?;
                if !claim_colliders.insert(handle)
                    || !matches!(actor.vehicle.state, VehicleState::Driving)
                    || collider.group != TransportationGroup::Vehicles
                    || collider.radius != car_extent()
                    || !actor.speed.0.is_finite()
                    || actor.speed.0.abs() > 0.2
                    || !(actor.trans.pos.distance2(expected.pos) <= 0.15_f32.powi(2)
                        && actor.trans.dir.distance2(expected.dir) <= 0.02
                        && pos.distance2(actor.trans.pos.xy()) <= 0.15_f32.powi(2)
                        && (collider.height - actor.trans.pos.z).abs() <= 0.15
                        && collider.dir.distance2(actor.trans.dir.xy()) <= 0.02
                        && collider.speed.abs() <= 0.2)
                {
                    return Err("invalid saved driveway collider pose or owner");
                }
            }

            for (id, actor) in &world.vehicles {
                if owners.contains(&id) || self.properties.owns_stage(id, actor.it.driveway_phase())
                {
                    continue;
                }
                if actor.it.driveway_phase().is_some()
                    || (matches!(actor.vehicle.kind, VehicleKind::Car)
                        && matches!(actor.vehicle.state, VehicleState::ParkedAtDepot))
                    || actor
                        .collider
                        .is_some_and(|handle| claim_colliders.contains(&handle.0))
                {
                    return Err("driveway vehicle or collider has no matching claim");
                }
            }
            if !claim_colliders.is_empty()
                && world.humans.values().any(|human| {
                    human
                        .collider
                        .is_some_and(|handle| claim_colliders.contains(&handle.0))
                })
            {
                return Err("driveway collider is shared with a pedestrian");
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
                        actor.it = Itinerary::at_driveway(DrivewayPhase::Entering);
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
                        actor.it = Itinerary::at_driveway(DrivewayPhase::Stored);
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
                    if request.deadline <= now {
                        *exit = None;
                        *blocker = Some(DrivewayBlocker::BudgetExhausted);
                        return false;
                    }
                    let mut slice = EXIT_WORK_PER_TICK;
                    if request.search.stale(map) {
                        if let Some(plan) = &request.ready {
                            while request.cleanup_work < plan.retained_work() {
                                if !request.search.charge(&mut slice) {
                                    if request.search.exhausted() {
                                        *exit = None;
                                        *blocker = Some(DrivewayBlocker::BudgetExhausted);
                                    }
                                    return false;
                                }
                                request.cleanup_work += 1;
                            }
                            request.ready = None;
                            request.cleanup_work = 0;
                        }
                        request.sweep = None;
                        *blocker = None;
                    }
                    if request.ready.is_none() {
                        match request.search.advance(map, &mut slice) {
                            DirectedSearchResult::Deferred => return false,
                            DirectedSearchResult::Unavailable => {
                                *exit = None;
                                *blocker = Some(DrivewayBlocker::NoRoute);
                                return false;
                            }
                            DirectedSearchResult::BudgetExhausted => {
                                *exit = None;
                                *blocker = Some(DrivewayBlocker::BudgetExhausted);
                                return false;
                            }
                            DirectedSearchResult::Feasible(plan) => request.ready = Some(plan),
                        }
                    }
                    match request.check_sweep(&geometry.out, grid, &mut slice) {
                        Some(false) => {
                            let route = request.ready.take().unwrap();
                            actor.trans = geometry.out.pose(0);
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
                            actor.it = Itinerary::at_driveway(DrivewayPhase::Exiting);
                            claim.motion = Motion::Exiting { tick: 0, route };
                        }
                        _ if request.search.exhausted() => {
                            *exit = None;
                            *blocker = Some(DrivewayBlocker::BudgetExhausted);
                        }
                        Some(true) => *blocker = Some(DrivewayBlocker::ReceivingSpace),
                        None => {}
                    }
                }
                Motion::Exiting { tick, .. } => {
                    *tick += 1;
                    actor.trans = geometry.out.pose(*tick);
                    if *tick == MOTION_TICKS {
                        let Motion::Exiting { route, .. } =
                            std::mem::replace(&mut claim.motion, Motion::Held)
                        else {
                            unreachable!()
                        };
                        actor.it = route.into_itinerary();
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

    pub(crate) fn sync_actor(grid: &mut TransportGrid, car: &mut crate::world::VehicleEnt) {
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

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::tests::pathpark_lifecycle::road_chain;
        use common::saveload::{Bincode, Encoder};

        #[test]
        fn pathpark_rejects_overlapping_saved_pads() {
            let (ctx, _, portal) = road_chain(1);
            let mut traffic = ctx.g.write::<DrivewayTraffic>();
            let original = &traffic.facilities[traffic.index(portal).unwrap()];
            let duplicate = Facility {
                id: DrivewayPortal(2),
                anchor: original.anchor,
                geometry: original.geometry,
                claim: None,
            };
            traffic.next_id = 2;
            traffic.facilities.push(duplicate);
            drop(traffic);
            assert!(Bincode::decode::<Simulation>(&Bincode::encode(&ctx.g).unwrap()).is_err());
        }

        #[test]
        fn pathpark_rejects_shared_maneuver_collider() {
            let (mut ctx, _, portal) = road_chain(1);
            let approach = portal.approach(&ctx.g).unwrap();
            let car = super::super::spawn_driving_vehicle(&mut ctx.g, VehicleKind::Car, approach)
                .unwrap();
            let other = super::super::spawn_driving_vehicle(
                &mut ctx.g,
                VehicleKind::Car,
                Transform::new_dir(approach.pos + approach.dir * 30., approach.dir),
            )
            .unwrap();
            portal.hold(&mut ctx.g, car).unwrap();
            ctx.tick_fast();
            assert_eq!(
                ctx.g.read::<DrivewayTraffic>().phase(portal),
                Some(DrivewayPhase::Entering)
            );
            ctx.g.world.vehicles[other].collider = ctx.g.world.vehicles[car].collider;
            assert!(Bincode::decode::<Simulation>(&Bincode::encode(&ctx.g).unwrap()).is_err());
        }

        #[test]
        fn pathpark_rejects_collider_displaced_from_maneuver() {
            let (mut ctx, _, portal) = road_chain(1);
            let approach = portal.approach(&ctx.g).unwrap();
            let car = super::super::spawn_driving_vehicle(&mut ctx.g, VehicleKind::Car, approach)
                .unwrap();
            portal.hold(&mut ctx.g, car).unwrap();
            ctx.tick_fast();
            assert_eq!(
                ctx.g.read::<DrivewayTraffic>().phase(portal),
                Some(DrivewayPhase::Entering)
            );
            let handle = ctx.g.world.vehicles[car].collider.unwrap();
            let mut grid = ctx.g.write::<TransportGrid>();
            grid.set_position(handle.0, (approach.pos - approach.dir * 25.).xy());
            grid.maintain_deterministic();
            drop(grid);
            assert!(Bincode::decode::<Simulation>(&Bincode::encode(&ctx.g).unwrap()).is_err());
        }
    }

    fn car_extent() -> f32 {
        VehicleKind::Car.width() * 0.5
    }
}
