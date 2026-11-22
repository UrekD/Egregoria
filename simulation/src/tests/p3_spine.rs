use common::saveload::{Bincode, Encoder};
use geom::{Transform, Vec2, Vec3};
use prototypes::{Money, TICKS_PER_HOUR};

use super::TestCtx;
use crate::economy::{transit_stats::TransitStats, TripStats};
use crate::map::LaneKind;
use crate::map_dynamic::{LineID, Transit};
use crate::transportation::{spawn_driving_vehicle, TransportGrid, VehicleKind, VehicleState};
use crate::world_command::{CommandOutcomes, Rejection, WorldCommand};
use crate::Simulation;

#[test]
fn p3_spine_rejects_unimplemented_commands_atomically() {
    let mut ctx = TestCtx::new();
    ctx.g.write::<crate::Replay>().enabled = true;
    let stop = Default::default();
    let line = Default::default();
    let commands = [
        WorldCommand::TransitAddStop {
            pos: Vec3::ZERO,
            dir: Vec2::X,
        },
        WorldCommand::TransitRemoveStop { stop },
        WorldCommand::TransitCreateLine {
            stops: vec![stop, stop],
        },
        WorldCommand::TransitEditLine {
            line,
            stops: vec![stop, stop],
        },
        WorldCommand::TransitRemoveLine { line },
        WorldCommand::TransitSetVehicles { line, n: 1 },
        WorldCommand::TransitSetFare {
            line,
            fare: Money::new_bucks(1),
        },
    ];
    let names = [
        "TransitAddStop",
        "TransitRemoveStop",
        "TransitCreateLine",
        "TransitEditLine",
        "TransitRemoveLine",
        "TransitSetVehicles",
        "TransitSetFare",
    ];
    let before = ctx.g.hashes();

    ctx.g.apply_commands(&commands);

    let outcomes = ctx.g.read::<CommandOutcomes>();
    assert_eq!(
        outcomes.last,
        names.map(|name| (name, Rejection::TransitNotImplemented))
    );
    assert!(outcomes.accepted.is_empty());
    assert_eq!(ctx.g.hashes(), before);
}

#[test]
fn p3_spine_registers_and_round_trips_transit_resources() {
    let mut ctx = TestCtx::new();
    assert!(ctx.g.read::<Transit>().stops.is_empty());
    ctx.g.write::<TripStats>().transit.completed = 1;
    ctx.g
        .write::<TransitStats>()
        .boardings
        .insert(Default::default(), 1);
    ctx.tick_fast();
    let systems: Vec<_> = ctx.sched.latest_samples().map(|(name, _)| name).collect();
    assert!(systems.windows(4).any(|names| names
        == [
            "vehicle_state_update_system",
            "transit_stop_update",
            "transit_dispatch_update",
            "transit_board_update",
        ]));
    let hashes = ctx.g.hashes();
    for key in ["transit", "trip_stats", "transit_stats"] {
        assert!(hashes.contains_key(key), "missing {key}");
    }

    let loaded: Simulation = Bincode::decode(&Bincode::encode(&ctx.g).unwrap()).unwrap();
    assert_eq!(loaded.hashes(), hashes);
    assert_eq!(loaded.read::<TripStats>().transit.completed, 1);
    assert_eq!(
        loaded
            .read::<TransitStats>()
            .boardings
            .values()
            .sum::<u64>(),
        1
    );
}

#[test]
fn p3_spine_spawns_driving_bus_only_at_clear_berth() {
    let mut ctx = TestCtx::new();
    let trans = Transform::zero();
    let before = ctx.g.hashes();
    let bus = spawn_driving_vehicle(&mut ctx.g, VehicleKind::Bus, trans).unwrap();
    let entity = ctx.g.get(bus).unwrap();
    assert_eq!(entity.trans, trans);
    assert!(matches!(entity.vehicle.state, VehicleState::Driving));
    assert!(ctx
        .g
        .read::<TransportGrid>()
        .get(entity.collider.unwrap().0)
        .is_some());
    let after = ctx.g.hashes();
    assert_eq!(before["pmanagement"], after["pmanagement"]);

    assert!(spawn_driving_vehicle(&mut ctx.g, VehicleKind::Bus, trans).is_none());
    assert_eq!(ctx.g.hashes(), after);
}

#[test]
#[ignore = "P3a must implement stops, dispatch, dwell, boarding, and detach"]
fn p3a_bus_loop_serves_both_stops_and_dwells() {
    let (mut ctx, line) = loop_fixture();
    ctx.apply(&[WorldCommand::TransitSetVehicles { line, n: 1 }]);
    let mut served = [false; 2];
    for _ in 0..TICKS_PER_HOUR {
        ctx.tick_fast();
        for run in ctx.g.read::<Transit>().runs.values() {
            if run.line == line && run.dwell_until.is_some() {
                served[run.next_stop_idx] = true;
            }
        }
        if served == [true, true] {
            return;
        }
    }
    panic!("bus did not dwell at both stops: {served:?}");
}

#[test]
#[ignore = "P3d performance check requires a running transit line"]
fn p3d_active_transit_tick_cost_stays_within_ten_percent() {
    let (ctx, line) = loop_fixture();
    let save = Bincode::encode(&ctx.g).unwrap();
    let mut legacy: Simulation = Bincode::decode(&save).unwrap();
    let mut active: Simulation = Bincode::decode(&save).unwrap();
    WorldCommand::TransitSetVehicles { line, n: 1 }.apply(&mut active);

    let measure = |sim: &mut Simulation| -> std::time::Duration {
        let mut schedule = Simulation::schedule();
        for _ in 0..1_000 {
            sim.tick(&mut schedule, &[]);
        }
        (0..10_000).map(|_| sim.tick(&mut schedule, &[])).sum()
    };
    let legacy_time = measure(&mut legacy);
    let active_time = measure(&mut active);

    assert!(legacy.read::<Transit>().runs.is_empty());
    assert!(!active.read::<Transit>().runs.is_empty());
    assert_eq!(legacy.world().humans.len(), active.world().humans.len());
    assert!(
        active_time <= legacy_time.mul_f64(1.10),
        "{active_time:?} vs {legacy_time:?}"
    );
}

fn loop_fixture() -> (TestCtx, LineID) {
    let mut ctx = TestCtx::new();
    ctx.build_roads(&[
        Vec3::new(100.0, 100.0, 0.0),
        Vec3::new(300.0, 100.0, 0.0),
        Vec3::new(300.0, 300.0, 0.0),
        Vec3::new(100.0, 300.0, 0.0),
        Vec3::new(100.0, 100.0, 0.0),
    ]);
    ctx.build_house_near(Vec2::new(200.0, 100.0));
    let add_stops = {
        let map = ctx.g.map();
        let lane = map
            .lanes()
            .values()
            .find(|lane| {
                lane.kind == LaneKind::Driving
                    && lane
                        .points
                        .middle()
                        .xy()
                        .is_close(Vec2::new(200.0, 100.0), 20.0)
            })
            .unwrap();
        [0.3, 0.7].map(|fraction| {
            let (pos, dir) = lane.points.point_dir_along(lane.points.length() * fraction);
            WorldCommand::TransitAddStop { pos, dir: dir.xy() }
        })
    };
    ctx.apply(&add_stops);
    let stops: Vec<_> = ctx.g.read::<Transit>().stops.keys().collect();
    assert_eq!(stops.len(), 2);
    ctx.apply(&[WorldCommand::TransitCreateLine { stops }]);
    let line = ctx.g.read::<Transit>().lines.keys().next().unwrap();
    (ctx, line)
}
