use geom::{vec2, vec3, Color, Transform, Vec2};
use prototypes::{GameTime, Tick};
use simulation::map::road_edit::{junction_edit_stamp, preview_junction_edit};
use simulation::map::{
    ApproachRole, JunctionRules, LaneKind, LanePatternBuilder, MapProject, ProjectKind,
};
use simulation::map_dynamic::Itinerary;
use simulation::transportation::road::calc_decision;
use simulation::transportation::{
    TransportState, TransportationGroup, Vehicle, VehicleKind, VehicleState,
};
use simulation::world_command::{CommandOutcomes, PrepareBudget, WorldCommand};
use simulation::{MapPreset, NewGameOptions, Simulation, VehicleID};

#[test]
fn p7_junction_receiving() {
    std::env::set_current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/..")).unwrap();
    simulation::init::init();
    let mut city = Simulation::new_game(&NewGameOptions {
        preset: MapPreset::Flat,
        terrain_chunks: 16,
        ..NewGameOptions::default()
    })
    .unwrap();
    let at = vec3(350.0, 300.0, 0.0);
    city.apply_commands([&WorldCommand::MapMakeConnection {
        from: MapProject::ground(vec3(100.0, 300.0, 0.0)),
        to: MapProject::ground(at),
        inter: None,
        pat: LanePatternBuilder::new().build(),
    }]);
    assert_eq!(city.read::<CommandOutcomes>().accepted, [0]);
    let (junction, junction_position) = {
        let map = city.map();
        let junction = map
            .intersections()
            .values()
            .find(|junction| junction.pos.xy() == at.xy())
            .unwrap();
        (junction.id, junction.pos)
    };
    city.apply_commands([&WorldCommand::MapMakeConnection {
        from: MapProject {
            kind: ProjectKind::Intersection(junction),
            pos: junction_position,
        },
        to: MapProject::ground(vec3(600.0, 300.0, 0.0)),
        inter: None,
        pat: LanePatternBuilder::new().n_lanes(2).build(),
    }]);
    assert_eq!(city.read::<CommandOutcomes>().accepted, [0]);
    let (road, position, direction, mut itinerary) = {
        let map = city.map();
        let inbound = map
            .lanes()
            .values()
            .find(|lane| {
                lane.dst == junction
                    && lane.kind == LaneKind::Driving
                    && lane.points.first().x < at.x
            })
            .unwrap();
        let outbound = map
            .lanes()
            .values()
            .find(|lane| {
                lane.src == junction
                    && lane.kind == LaneKind::Driving
                    && lane.points.last().x > at.x
            })
            .unwrap();
        let (position, direction) = inbound
            .points
            .point_dir_along(inbound.points.length() - 8.0);
        let itinerary =
            Itinerary::route_to_lane(Tick(1), position, &map, outbound.id, 30.0).unwrap();
        (inbound.parent, position, direction, itinerary)
    };
    itinerary.update(position, 0.0, Tick(1), 0, &city.map());
    let now = GameTime::new(Tick(1));
    let car_position = Transform::new_dir(position, direction);
    let car = TransportState {
        dir: direction.xy(),
        height: position.z,
        group: TransportationGroup::Vehicles,
        radius: VehicleKind::Car.width() * 0.5,
        ..TransportState::default()
    };
    let crossing = TransportState {
        dir: Vec2::Y,
        speed: 1.0,
        ..car
    };
    let decide = |city: &Simulation, state, neighbours: &[(Vec2, &TransportState)]| {
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
            &city.map(),
            &now,
            &car_position,
            &car,
            &itinerary,
            neighbours.iter().copied(),
        )
        .0
    };
    let cross_pos = position.xy() + vec2(2.0, -2.0);
    let conflict = [(cross_pos, &crossing)];
    let panic = || VehicleState::Panicking(now.instant());
    assert_eq!(decide(&city, VehicleState::Driving, &conflict), 0.0);
    assert!(decide(&city, panic(), &conflict) > 0.0);

    let rules = JunctionRules::new(vec![(road, ApproachRole::Stop)], vec![]).unwrap();
    let budget = &mut PrepareBudget::for_preview(10_000);
    let expected = junction_edit_stamp(&city, junction, budget).unwrap();
    let preview = preview_junction_edit(&city, junction, expected, &rules, budget).unwrap();
    city.apply_commands([&WorldCommand::MapSetJunctionRules {
        junction,
        expected,
        rules,
        quote: preview.quote,
    }]);
    assert_eq!(city.read::<CommandOutcomes>().accepted, [0]);
    assert!(decide(&city, panic(), &[]) > 0.0);
    assert_eq!(decide(&city, panic(), &conflict), 0.0);

    let leader = (position.xy() + vec2(1.5, 0.0), &car);
    assert_eq!(decide(&city, panic(), &[leader, conflict[0]]), 0.0);
    let different_deck = TransportState {
        height: position.z + 10.0,
        ..crossing
    };
    assert!(decide(&city, panic(), &[(cross_pos, &different_deck)]) > 0.0);
}
