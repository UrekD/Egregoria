use super::TestCtx;
use crate::map::{Density, Lot, ZoneCell, ZoneKind};
use crate::transportation::VehicleState;
use geom::{vec2, vec3};

#[test]
fn pathpark_home_car_uses_its_own_parcel() {
    let mut ctx = TestCtx::new();
    ctx.build_roads(&[vec3(75., 100., 0.), vec3(450., 100., 0.)]);
    let lot = {
        let map = ctx.g.map();
        map.lots()
            .values()
            .filter(|lot| !Lot::footprint_obstructed(&map, lot.shape, false))
            .min_by_key(|lot| lot.shape.center().distance2(vec2(250., 135.)) as i32)
            .unwrap()
            .clone()
    };
    let house = ctx
        .g
        .place_zoned_growable(
            lot.id,
            ZoneCell {
                kind: ZoneKind::Residential,
                density: Density::Low,
            },
        )
        .unwrap()
        .building;
    use crate::world_command::WorldCommand;
    use common::saveload::{Bincode, Encoder};
    let checkpoint = Bincode::encode(&ctx.g).unwrap();
    let mut vacant: crate::Simulation = Bincode::decode(&checkpoint).unwrap();
    assert_eq!(vacant.building_parking(house).unwrap().occupied, 0);
    vacant.apply_commands([&WorldCommand::MapRemoveRoad(lot.parent)]);
    assert!(vacant.map().get(lot.parent).is_none());
    assert!(vacant.building_parking(house).is_none());
    assert!(Bincode::decode::<crate::Simulation>(&Bincode::encode(&vacant).unwrap()).is_ok());

    crate::souls::human::spawn_human(&mut ctx.g, house).unwrap();
    let car = ctx
        .g
        .world
        .vehicles
        .values()
        .next()
        .expect("new resident's car");
    assert!(
        matches!(car.vehicle.state, VehicleState::ParkedAtDepot),
        "the resident should park on the property, got {:?}",
        car.vehicle.state
    );
    assert!(lot.shape.contains(car.trans.pos.xy()));
    ctx.g
        .apply_commands([&WorldCommand::MapRemoveRoad(lot.parent)]);
    assert!(ctx.g.map().get(lot.parent).is_some());
    assert_eq!(ctx.g.building_parking(house).unwrap().occupied, 1);
}

#[test]
fn pathpark_two_public_garage_forms_commit_once_and_reload() {
    use crate::map::building_parking::{parking_entrance_near, BuildingParkingKind, GarageKind};
    use crate::world_command::{CommandOutcomes, WorldCommand};
    use crate::Simulation;
    use common::saveload::{Bincode, Encoder};

    let mut ctx = TestCtx::new();
    ctx.build_roads(&[vec3(75., 100., 0.), vec3(450., 100., 0.)]);
    let coords = [vec3(170., 115., 0.), vec3(330., 85., 0.)];
    for (kind, pick) in [GarageKind::MultiStorey, GarageKind::Underground]
        .into_iter()
        .zip(coords)
    {
        let entrance = parking_entrance_near(&ctx.g.map(), pick).unwrap();
        let command = WorldCommand::MapPlaceParkingGarage { kind, entrance };
        assert_eq!(command.validate_and_price(&ctx.g).unwrap(), kind.cost());
        let before = ctx.g.read::<crate::economy::Government>().money;
        ctx.g.apply_commands([&command]);
        let building = ctx
            .g
            .read::<CommandOutcomes>()
            .parking_garage(0)
            .unwrap()
            .unwrap();
        assert_eq!(
            ctx.g.read::<crate::economy::Government>().money,
            before - kind.cost()
        );
        let parking = ctx.g.building_parking(building).unwrap();
        assert_eq!(parking.kind, BuildingParkingKind::PublicGarage(kind));
        assert_eq!(parking.capacity, kind.spaces());
        assert_eq!(parking.occupied, 0);
        assert_eq!(parking.daily_upkeep, kind.daily_upkeep());
        let committed = ctx.g.hashes();
        ctx.g.apply_commands([&command]);
        assert!(ctx
            .g
            .read::<CommandOutcomes>()
            .parking_garage(0)
            .unwrap()
            .is_err());
        assert_eq!(ctx.g.hashes(), committed);
        let snapshot = Bincode::encode(&ctx.g).unwrap();
        let restored: Simulation = Bincode::decode(&snapshot).unwrap();
        assert_eq!(restored.hashes(), committed);
    }
}

#[test]
fn pathpark_home_car_leaves_through_the_apron_and_uses_a_destination_garage() {
    use crate::economy::TripPurpose;
    use crate::map_dynamic::Destination;
    use crate::transportation::{DrivewayPhase, Location};
    use crate::world_command::{CommandOutcomes, WorldCommand};
    use common::saveload::{Bincode, Encoder};

    let (mut ctx, home, human, car, garage) = property_trip();
    ctx.g.world.edit_human(human, |human| {
        human
            .router
            .go_to_for(Destination::Building(garage), TripPurpose::Work);
        human.decision.wait = u8::MAX;
    });
    let mut saw_exit = false;
    let mut saw_ingress = false;
    let mut saw_gate = false;
    let mut arrived = false;
    let gate = ctx.g.map().get(garage).unwrap().door_pos;
    for _ in 0..4_000 {
        ctx.g
            .world
            .edit_human(human, |human| human.decision.wait = u8::MAX);
        ctx.tick_fast();
        match ctx.g.world.vehicles[car].it.driveway_phase() {
            Some(DrivewayPhase::Exiting) if !saw_exit => {
                saw_exit = true;
                assert_eq!(ctx.g.building_parking(home).unwrap().occupied, 1);
                assert_eq!(ctx.g.building_parking(garage).unwrap().held, 1);
                ctx.g = Bincode::decode(&Bincode::encode(&ctx.g).unwrap()).unwrap();
            }
            Some(DrivewayPhase::Entering) if !saw_ingress => {
                saw_ingress = true;
                assert_eq!(ctx.g.building_parking(home).unwrap().occupied, 0);
                ctx.g.apply_commands([&WorldCommand::SetBuildingParkingOpen {
                    building: garage,
                    open: false,
                }]);
                assert_eq!(ctx.g.building_parking(garage).unwrap().held, 1);
                ctx.g = Bincode::decode(&Bincode::encode(&ctx.g).unwrap()).unwrap();
            }
            _ => {}
        }
        if !saw_gate && ctx.g.car_stored_inside_garage(car)
            && ctx.g.world.humans[human].location == Location::Outside
        {
            saw_gate = true;
            assert_eq!(ctx.g.world.humans[human].trans.pos, gate);
        }
        if ctx.g.world.humans[human].location == Location::Building(garage) {
            arrived = true;
            break;
        }
    }
    assert!(
        saw_exit && saw_ingress && saw_gate && arrived,
        "exit={saw_exit} ingress={saw_ingress} arrived={arrived}, current={:?} car={:?}",
        ctx.g.world.humans[human].router.cur_step(),
        ctx.g.world.vehicles[car].trans
    );
    assert_eq!(ctx.g.building_parking(garage).unwrap().occupied, 1);
    assert!(ctx.g.car_stored_inside_garage(car));
    assert!(ctx.g.world.vehicles[car].collider.is_none());
    ctx.g
        .apply_commands([&WorldCommand::MapRemoveBuilding(garage)]);
    assert!(ctx.g.read::<CommandOutcomes>().accepted.is_empty());
    ctx.g
        .apply_commands([&WorldCommand::SetBuildingParkingOpen {
            building: garage,
            open: false,
        }]);
    assert!(!ctx.g.building_parking(garage).unwrap().open);
    ctx.g = Bincode::decode(&Bincode::encode(&ctx.g).unwrap()).unwrap();
    let drained = (0..5_000).any(|_| {
        ctx.g
            .world
            .edit_human(human, |human| human.decision.wait = u8::MAX);
        ctx.tick_fast();
        ctx.g.building_parking(garage).unwrap().occupied == 0
    });
    assert!(
        drained,
        "an idle driver should retrieve their vehicle after closure"
    );
    assert_eq!(ctx.g.world.humans[human].router.personal_car, Some(car));
    ctx.g
        .apply_commands([&WorldCommand::MapRemoveBuilding(garage)]);
    assert!(ctx.g.map().get(garage).is_none());
}


fn property_trip() -> (TestCtx, crate::map::BuildingID, crate::world::HumanID, crate::world::VehicleID, crate::map::BuildingID) {
    use crate::map::building_parking::{parking_entrance_near, GarageKind};
    use crate::world_command::{CommandOutcomes, WorldCommand};
    let mut ctx = TestCtx::new();
    ctx.build_roads(&[
        vec3(75., 100., 0.),
        vec3(450., 100., 0.),
        vec3(450., 280., 0.),
        vec3(75., 280., 0.),
        vec3(75., 100., 0.),
    ]);
    let lot = {
        let map = ctx.g.map();
        map.lots()
            .values()
            .filter(|lot| !Lot::footprint_obstructed(&map, lot.shape, false))
            .min_by_key(|lot| lot.shape.center().distance2(vec2(280., 135.)) as i32)
            .unwrap()
            .id
    };
    let home = ctx
        .g
        .place_zoned_growable(
            lot,
            ZoneCell {
                kind: ZoneKind::Residential,
                density: Density::Low,
            },
        )
        .unwrap()
        .building;
    let human = crate::souls::human::spawn_human(&mut ctx.g, home).unwrap();
    let car = ctx.g.world.humans[human].router.personal_car.unwrap();
    let entrance = parking_entrance_near(&ctx.g.map(), vec3(150., 119., 0.)).unwrap();
    ctx.g.apply_commands([&WorldCommand::MapPlaceParkingGarage {
        kind: GarageKind::MultiStorey,
        entrance,
    }]);
    let garage = ctx
        .g
        .read::<CommandOutcomes>()
        .parking_garage(0)
        .unwrap()
        .unwrap();
    (ctx, home, human, car, garage)
}

#[test]
fn pathpark_closing_cancels_an_uncommitted_hold() {
    use crate::map_dynamic::Destination;
    use crate::world_command::WorldCommand;
    use common::saveload::{Bincode, Encoder};

    let (mut ctx, home, human, car, garage) = property_trip();
    ctx.g.world.edit_human(human, |h| { h.router.go_to(Destination::Building(garage)); });
    assert!((0..500).any(|_| {
        ctx.g.world.edit_human(human, |h| h.decision.wait = u8::MAX);
        ctx.tick_fast();
        ctx.g.building_parking(garage).unwrap().held == 1
    }));
    assert_eq!(ctx.g.building_parking(home).unwrap().occupied, 1);
    ctx.g.apply_commands([&WorldCommand::SetBuildingParkingOpen { building: garage, open: false }]);
    assert_eq!(ctx.g.building_parking(garage).unwrap().held, 0);
    ctx.g = Bincode::decode(&Bincode::encode(&ctx.g).unwrap()).unwrap();
    for _ in 0..1_200 {
        ctx.g.world.edit_human(human, |h| h.decision.wait = u8::MAX);
        ctx.tick_fast();
    }
    assert_eq!(ctx.g.building_parking(garage).unwrap().occupied, 0);
    assert!(ctx.g.world.vehicles.contains_key(car));
}

#[test]
fn pathpark_despawn_preserves_the_original_stored_car() {
    use crate::utils::par_command_buffer::ParCommandBuffer;
    use crate::world::HumanEnt;
    use crate::world_command::WorldCommand;
    use common::saveload::{Bincode, Encoder};

    let (mut ctx, home, human, car, _) = property_trip();
    ctx.g.read::<ParCommandBuffer<HumanEnt>>().kill(human);
    ctx.g.apply_commands([&WorldCommand::SetBuildingParkingOpen { building: home, open: false }]);
    assert!(!ctx.g.world.humans.contains_key(human));
    assert!(ctx.g.world.vehicles.contains_key(car));
    assert_eq!(ctx.g.building_parking(home).unwrap().occupied, 1);
    let restored: crate::Simulation = Bincode::decode(&Bincode::encode(&ctx.g).unwrap()).unwrap();
    assert!(restored.world.vehicles.contains_key(car));
    assert_eq!(restored.building_parking(home).unwrap().occupied, 1);
    assert_eq!(restored.building_parking(home).unwrap().missing_owners, 1);
}

#[test]
fn pathpark_property_departure_uses_shared_garage_when_curbs_are_full() {
    use crate::map_dynamic::{Destination, ParkingManagement};

    let (mut ctx, _, human, _, garage) = property_trip();
    let door = ctx.g.map().get(garage).unwrap().door_pos;
    {
        let mut parking = ctx.g.write::<ParkingManagement>();
        for (spot, _) in ctx.g.map().parking.all_spots() {
            parking.mask(spot);
        }
    }
    ctx.g.world.edit_human(human, |h| { h.router.go_to(Destination::Outside(door)); });
    assert!((0..500).any(|_| {
        ctx.g.world.edit_human(human, |h| h.decision.wait = u8::MAX);
        ctx.tick_fast();
        ctx.g.building_parking(garage).unwrap().held == 1
    }));
}

#[test]
fn pathpark_oversized_garage_batch_has_no_side_effects() {
    use crate::map::building_parking::GarageKind;
    use crate::world_command::{CommandOutcomes, WorldCommand, MAX_TILED_COMMANDS};

    let (mut ctx, _, _, _, garage) = property_trip();
    let mut entrance = ctx.g.building_parking(garage).unwrap().entrance;
    entrance.station_cm += 16_000;
    entrance = ctx.g.map().pick_curb(entrance.road, entrance.side, entrance.station_cm).unwrap();
    let command = WorldCommand::MapPlaceParkingGarage { kind: GarageKind::Underground, entrance };
    assert!(command.validate_and_price(&ctx.g).is_ok());
    ctx.g.apply_commands(std::iter::repeat_n(&command, MAX_TILED_COMMANDS + 1));
    assert!(ctx.g.read::<CommandOutcomes>().accepted.is_empty());
}

#[test]
fn pathpark_imports_only_suppress_parking_for_the_benchmark_profile() {
    use crate::map::BuildingKind;

    for curb_only in [false, true] {
        let mut ctx = TestCtx::new();
        if curb_only {
            ctx.g.use_curb_only_benchmark_parking();
        }
        ctx.build_roads(&[vec3(75., 100., 0.), vec3(450., 100., 0.)]);
        let lot = ctx.g.map().lots().values()
            .filter(|lot| !Lot::footprint_obstructed(&ctx.g.map(), lot.shape, false))
            .min_by_key(|lot| lot.shape.center().distance2(vec2(250., 135.)) as i32)
            .unwrap().id;
        let home = ctx.g.seed_imported_growable(lot, BuildingKind::House, Density::Low).unwrap();
        assert_eq!(ctx.g.building_parking(home).is_some(), !curb_only);
    }
}
