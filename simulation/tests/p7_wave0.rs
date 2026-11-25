use geom::{vec2, vec3, Color, Transform, Vec2};
use prototypes::{GameTime, Tick};
use simulation::map::Map;
use simulation::map_dynamic::Itinerary;
use simulation::transportation::road::calc_decision;
use simulation::transportation::{
    TransportState, TransportationGroup, Vehicle, VehicleKind, VehicleState,
};
use simulation::VehicleID;

#[test]
fn p7_junction_receiving() {
    let now = GameTime::new(Tick(1));
    let car_position = Transform::new(vec3(0.0, 0.0, 0.0));
    let car = TransportState {
        group: TransportationGroup::Vehicles,
        radius: VehicleKind::Car.width() * 0.5,
        ..TransportState::default()
    };
    let crossing = TransportState {
        dir: Vec2::Y,
        speed: 1.0,
        ..car
    };
    let itinerary = Itinerary::simple(vec![vec3(10.0, 0.0, 0.0)]);
    let map = Map::empty();
    let decide = |state, neighbours: &[(Vec2, &TransportState)]| {
        let mut vehicle = Vehicle {
            ang_velocity: 0.0,
            wait_time: 0.0,
            max_speed_multiplier: 1.0,
            state,
            kind: VehicleKind::Car,
            tint: Color::WHITE,
            flag: 0,
        };
        calc_decision(
            VehicleID::default(),
            &mut vehicle,
            &map,
            &now,
            &car_position,
            &car,
            &itinerary,
            neighbours.iter().copied(),
        )
        .0
    };
    assert!(decide(VehicleState::Driving, &[]) > 0.0);
    let conflict = [(vec2(2.0, -2.0), &crossing)];
    assert_eq!(decide(VehicleState::Driving, &conflict), 0.0);
    assert_eq!(
        decide(VehicleState::Panicking(now.instant()), &conflict),
        0.0,
        "panic must hold the approach while the conflict is occupied",
    );
    let leader = [(vec2(1.5, 0.0), &car)];
    assert!(decide(VehicleState::Panicking(now.instant()), &leader) > 0.0);
    let blocked_view = [leader[0], conflict[0]];
    assert_eq!(
        decide(VehicleState::Panicking(now.instant()), &blocked_view),
        0.0
    );
    let different_deck = TransportState {
        height: 10.0,
        ..crossing
    };
    assert!(
        decide(
            VehicleState::Panicking(now.instant()),
            &[(conflict[0].0, &different_deck)]
        ) > 0.0
    );
}
