use super::*;
use crate::map::{
    LaneKind, LanePatternBuilder, MapProject, TrafficControl, TrafficLightSchedule,
    TraverseDirection, TraverseKind,
};
use crate::map_dynamic::transit::elapsed_ticks;
use crate::map_dynamic::Itinerary;
use crate::transportation::{
    spawn_driving_vehicle, transport_grid_synchronize, unpark, VehicleKind,
};
use crate::world_command::WorldCommand;
use geom::{vec3, Transform};
use prototypes::Tick;

fn straight() -> (Simulation, RoadID, LaneID) {
    crate::tests::init_test_simulation();
    let mut sim = Simulation::new_authored_flat(2, 6042).unwrap();
    sim.apply_commands([&WorldCommand::MapMakeConnection {
        from: MapProject::ground(vec3(60.0, 150.0, 0.0)),
        to: MapProject::ground(vec3(285.0, 150.0, 0.0)),
        inter: None,
        pat: LanePatternBuilder::new().build(),
    }]);
    let (road, lane) = {
        let map = sim.map();
        let road = map.roads().values().next().unwrap();
        let (lane, _) = road
            .lanes_iter()
            .find(|&(_, kind)| kind == LaneKind::Driving)
            .unwrap();
        (road.id, lane)
    };
    assert!(watch_queue(&sim, lane));
    (sim, road, lane)
}

fn car(sim: &mut Simulation, lane: LaneID, from_end: f32) -> VehicleID {
    let (trans, it) = {
        let map = sim.map();
        let points = &map.get(lane).unwrap().points;
        let length = points.length();
        let start = length - from_end;
        let position = points.point_along(start);
        let direction = points.last_dir().unwrap();
        let it = Itinerary::route_between_lanes(Tick(0), &map, (lane, start), (lane, length - 0.1))
            .unwrap();
        (Transform::new_dir(position, direction), it)
    };
    // Normal spawning rejects occupied approach space. Stage the fixture's
    // bumper gaps explicitly, then use ordinary collider synchronization.
    let offset = 30.0 * (sim.world.vehicles.len() + 1) as f32;
    let staged = Transform::new_dir(trans.pos + vec3(0.0, offset, 0.0), trans.dir);
    let id = spawn_driving_vehicle(sim, VehicleKind::Car, staged).unwrap();
    let vehicle = &mut sim.world.vehicles[id];
    vehicle.trans = trans;
    vehicle.it = it;
    id
}

fn publish(sim: &mut Simulation) {
    let (world, resources) = sim.world_res();
    transport_grid_synchronize(world, resources);
    crate::map_dynamic::itinerary_update(world, resources);
}

fn exact(sim: &Simulation, lane: LaneID, limits: QueueLimits) -> QueueExplanation {
    let revision = QueueRevision::current(&sim.map());
    explain_queue(sim, lane, revision, limits)
}

fn trace(sim: &Simulation, lane: LaneID, id: VehicleID, reason: QueueHoldReason) {
    sim.write::<QueueDiagnostics>().record_hold(
        id,
        Some(Traversable::new(
            TraverseKind::Lane(lane),
            TraverseDirection::Forward,
        )),
        elapsed_ticks(&sim.resources).0,
        reason,
    );
}

#[test]
fn p7_diag_exact_cutoff_uses_current_positions_between_periodic_samples() {
    let (mut sim, _, lane) = straight();
    publish(&mut sim);
    let (world, resources) = sim.world_res();
    queue_diagnostics_update(world, resources);
    assert_eq!(
        exact(&sim, lane, QueueLimits::default()).coverage,
        QueueCoverage::Complete
    );
    assert!(sim
        .read::<QueueDiagnostics>()
        .presentation(lane)
        .unwrap()
        .stop_line
        .is_empty());
    let head = car(&mut sim, lane, 4.0);
    publish(&mut sim);
    assert_eq!(
        exact(&sim, lane, QueueLimits::default()).stop_line,
        vec![head]
    );
    assert!(sim
        .read::<QueueDiagnostics>()
        .presentation(lane)
        .unwrap()
        .stop_line
        .is_empty());
}

#[test]
fn p7_diag_blocker_chain_resolves_handles_and_cycles() {
    let (mut sim, _, lane) = straight();
    let a = car(&mut sim, lane, 4.0);
    let b = car(&mut sim, lane, 9.0);
    publish(&mut sim);
    let a_handle = sim.world.vehicles[a].collider.unwrap().0;
    let b_handle = sim.world.vehicles[b].collider.unwrap().0;
    trace(&sim, lane, a, QueueHoldReason::Leader(b_handle));
    trace(&sim, lane, b, QueueHoldReason::Leader(a_handle));
    let readout = exact(&sim, lane, QueueLimits::default());
    assert_eq!(readout.stop_line, [a, b]);
    assert_eq!(
        readout.chain_end,
        BlockerChainEnd::Cycle(TraversalOwner::Vehicle(a))
    );

    sim.write::<TransportGrid>().remove_maintain(b_handle);
    let readout = exact(&sim, lane, QueueLimits::default());
    assert_eq!(readout.chain_end, BlockerChainEnd::Unseen);
}

#[test]
fn p7_diag_lawful_wait_uses_actual_trace_and_preserves_hashes() {
    let (mut sim, _, lane) = straight();
    car(&mut sim, lane, 4.0);
    sim.map_mut().lanes[lane].control =
        TrafficControl::Light(TrafficLightSchedule::from_basic(0, 0, 100, 0));
    publish(&mut sim);
    let (world, resources) = sim.world_res();
    crate::transportation::road::vehicle_decision_system(world, resources);
    let before = sim.hashes();
    let readout = exact(&sim, lane, QueueLimits::default());
    assert_eq!(readout.reason, Some(QueueHoldReason::Signal));
    assert_eq!(readout.chain_end, BlockerChainEnd::LawfulWait);
    assert_eq!(sim.hashes(), before);
}

#[test]
fn p7_diag_optional_observer_does_not_change_movement() {
    use common::saveload::{Bincode, Encoder};

    let (mut sim, _, lane) = straight();
    car(&mut sim, lane, 4.0);
    car(&mut sim, lane, 9.0);
    sim.map_mut().lanes[lane].control =
        TrafficControl::Light(TrafficLightSchedule::from_basic(0, 0, 100, 0));
    let bytes = Bincode::encode(&sim).unwrap();
    let mut baseline: Simulation = Bincode::decode(&bytes).unwrap();
    let mut watched: Simulation = Bincode::decode(&bytes).unwrap();
    assert!(watch_queue(&watched, lane));
    let (mut baseline_schedule, mut watched_schedule) =
        (Simulation::schedule(), Simulation::schedule());
    for _ in 0..2 {
        baseline.tick(&mut baseline_schedule, &[]);
        watched.tick(&mut watched_schedule, &[]);
        assert_eq!(watched.hashes(), baseline.hashes());
    }
    assert!(watched
        .read::<QueueDiagnostics>()
        .presentation(lane)
        .is_some());
}

#[test]
fn p7_diag_coverage_budget_marks_omitted_occupants_and_curb() {
    let (mut sim, road, lane) = straight();
    for offset in [4.0, 9.0, 14.0, 19.0] {
        car(&mut sim, lane, offset);
    }
    publish(&mut sim);
    let limits = QueueLimits {
        occupants: 2,
        parking_spots: 1,
        ..QueueLimits::default()
    };
    let readout = exact(&sim, lane, limits);
    assert_eq!(readout.coverage, QueueCoverage::Truncated);
    assert_eq!(readout.visited_occupants, 2);
    assert_eq!(readout.omitted_occupants, 2);

    let parking = parking_pressure(&sim, road, limits).unwrap();
    assert_eq!(parking.free, 1);
    assert!(parking.total > 1);
    assert_eq!(parking.omitted_spots + parking.free, parking.total);
}

#[test]
fn p7_diag_parking_claim_counts_once_and_records_attempts() {
    let (mut sim, road, lane) = straight();
    let near = sim.map().get(lane).unwrap().points.point_along(50.0);
    assert!(watch_parking(&sim, road));
    let vehicle =
        crate::transportation::spawn_parked_vehicle(&mut sim, VehicleKind::Car, near).unwrap();
    let pressure = parking_pressure(&sim, road, QueueLimits::default()).unwrap();
    assert_eq!(pressure.parked, 1);
    assert_eq!(pressure.reserved_inbound, 0);
    assert_eq!(pressure.free + pressure.parked, pressure.total);
    assert_eq!(
        pressure.search.unwrap(),
        ParkingSearchStats {
            attempts: 1,
            exhausted: 0,
            other_road: 0,
        }
    );
    unpark(&mut sim, vehicle);
    let pressure = parking_pressure(&sim, road, QueueLimits::default()).unwrap();
    assert_eq!(pressure.free, pressure.total);
}

#[test]
fn p7_diag_spillback_counts_permissive_ticks_until_discharge() {
    let mut watch = SpillbackWatch::default();
    let id = VehicleID::default();
    let sample = |offset, permissive| {
        Some(SpillbackSample {
            standing: Some((id, offset)),
            permissive,
        })
    };
    watch.update(1, sample(10.0, true));
    watch.update(2, sample(9.9, false));
    watch.update(3, sample(10.4, true));
    assert_eq!(watch.counts.active_permissive_ticks, 2);

    // A physical discharge from the movement ends the episode even when
    // another vehicle keeps the receiving interval full.
    watch.last_discharge = Some(4);
    watch.update(4, sample(10.4, true));
    assert_eq!(watch.counts.active_permissive_ticks, 0);
    watch.update(5, sample(10.7, true));
    watch.update(6, sample(11.4, true));
    watch.update(7, None);
    assert_eq!(watch.counts.longest_permissive_ticks, 2);
    assert_eq!(watch.counts.episodes, 3);
    assert_eq!(watch.counts.incomplete_ticks, 1);
}
