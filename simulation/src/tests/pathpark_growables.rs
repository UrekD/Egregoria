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
    use crate::map::building_parking::{parking_entrance_near, GarageKind};
    use crate::map_dynamic::Destination;
    use crate::transportation::{DrivewayPhase, Location};
    use crate::world_command::{CommandOutcomes, WorldCommand};
    use common::saveload::{Bincode, Encoder};

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
    ctx.g.world.edit_human(human, |human| {
        human
            .router
            .go_to_for(Destination::Building(garage), TripPurpose::Work);
        human.decision.wait = u8::MAX;
    });
    let mut saw_exit = false;
    let mut saw_ingress = false;
    let mut arrived = false;
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
                ctx.g = Bincode::decode(&Bincode::encode(&ctx.g).unwrap()).unwrap();
            }
            _ => {}
        }
        if ctx.g.world.humans[human].location == Location::Building(garage) {
            arrived = true;
            break;
        }
    }
    assert!(
        saw_exit && saw_ingress && arrived,
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
