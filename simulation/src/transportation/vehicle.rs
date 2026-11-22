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
