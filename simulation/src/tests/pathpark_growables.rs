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
                ctx.g
                    .apply_commands([&WorldCommand::SetBuildingParkingOpen {
                        building: garage,
                        open: false,
                    }]);
                assert_eq!(ctx.g.building_parking(garage).unwrap().held, 1);
                ctx.g = Bincode::decode(&Bincode::encode(&ctx.g).unwrap()).unwrap();
            }
            _ => {}
        }
        if !saw_gate
            && ctx.g.car_stored_inside_garage(car)
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

fn property_trip() -> (
    TestCtx,
    crate::map::BuildingID,
    crate::world::HumanID,
    crate::world::VehicleID,
    crate::map::BuildingID,
) {
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
    ctx.g.world.edit_human(human, |h| {
        h.router.go_to(Destination::Building(garage));
    });
    assert!((0..500).any(|_| {
        ctx.g.world.edit_human(human, |h| h.decision.wait = u8::MAX);
        ctx.tick_fast();
        ctx.g.building_parking(garage).unwrap().held == 1
    }));
    assert_eq!(ctx.g.building_parking(home).unwrap().occupied, 1);
    ctx.g
        .apply_commands([&WorldCommand::SetBuildingParkingOpen {
            building: garage,
            open: false,
        }]);
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
    ctx.build_roads(&[vec3(450.0, 280.0, 0.0), vec3(450.0, 360.0, 0.0)]);
    ctx.g.read::<ParCommandBuffer<HumanEnt>>().kill(human);
    ctx.g
        .apply_commands([&WorldCommand::SetBuildingParkingOpen {
            building: home,
            open: false,
        }]);
    assert!(!ctx.g.world.humans.contains_key(human));
    assert!(ctx.g.world.vehicles.contains_key(car));
    assert_eq!(ctx.g.building_parking(home).unwrap().occupied, 1);
    let restored: crate::Simulation = Bincode::decode(&Bincode::encode(&ctx.g).unwrap()).unwrap();
    assert!(restored.world.vehicles.contains_key(car));
    assert_eq!(restored.building_parking(home).unwrap().occupied, 1);
    assert_eq!(restored.building_parking(home).unwrap().missing_owners, 1);
}

#[test]
fn pathpark_closing_retrieves_a_car_after_an_idle_walk_only_trip() {
    use crate::world_command::WorldCommand;

    let (mut ctx, home, human, car, _) = property_trip();
    ctx.g.world.edit_human(human, |h| {
        h.router.walk_only_pickup = true;
        h.decision.wait = u8::MAX;
    });
    ctx.g
        .apply_commands([&WorldCommand::SetBuildingParkingOpen {
            building: home,
            open: false,
        }]);

    assert!((0..4_000).any(|_| {
        ctx.g.world.edit_human(human, |h| h.decision.wait = u8::MAX);
        ctx.tick_fast();
        ctx.g.building_parking(home).unwrap().occupied == 0
    }));
    assert_eq!(ctx.g.world.humans[human].router.personal_car, Some(car));
}

#[test]
fn pathpark_owner_despawn_does_not_interrupt_committed_ingress() {
    use crate::map_dynamic::Destination;
    use crate::transportation::DrivewayPhase;
    use crate::utils::par_command_buffer::ParCommandBuffer;
    use crate::world::HumanEnt;
    use common::saveload::{Bincode, Encoder};

    let (mut ctx, _, human, car, garage) = property_trip();
    ctx.g.world.edit_human(human, |h| {
        h.router.go_to(Destination::Building(garage));
    });
    assert!((0..4_000).any(|_| {
        ctx.g.world.edit_human(human, |h| h.decision.wait = u8::MAX);
        ctx.tick_fast();
        ctx.g.world.vehicles[car].it.driveway_phase() == Some(DrivewayPhase::Entering)
    }));

    ctx.g.read::<ParCommandBuffer<HumanEnt>>().kill(human);
    ctx.g.apply_commands([]);
    assert!(!ctx.g.world.humans.contains_key(human));
    assert_eq!(ctx.g.building_parking(garage).unwrap().held, 1);
    assert!(ctx.g.world.vehicles[car].collider.is_some());
    ctx.g = Bincode::decode(&Bincode::encode(&ctx.g).unwrap()).unwrap();

    assert!((0..240).any(|_| {
        ctx.tick_fast();
        ctx.g.car_stored_inside_garage(car)
    }));
    let parking = ctx.g.building_parking(garage).unwrap();
    assert_eq!(
        (parking.held, parking.occupied, parking.missing_owners),
        (0, 1, 1)
    );
}

#[test]
fn pathpark_owner_despawn_during_egress_recovers_the_car_after_reload() {
    use crate::map_dynamic::Destination;
    use crate::transportation::DrivewayPhase;
    use crate::utils::par_command_buffer::ParCommandBuffer;
    use crate::world::HumanEnt;
    use common::saveload::{Bincode, Encoder};

    let (mut ctx, home, human, car, garage) = property_trip();
    ctx.g.world.edit_human(human, |h| {
        h.router.go_to(Destination::Building(garage));
    });
    assert!((0..4_000).any(|_| {
        ctx.g.world.edit_human(human, |h| h.decision.wait = u8::MAX);
        ctx.tick_fast();
        ctx.g.world.vehicles[car].it.driveway_phase() == Some(DrivewayPhase::Exiting)
    }));

    ctx.g.read::<ParCommandBuffer<HumanEnt>>().kill(human);
    ctx.g.apply_commands([]);
    assert!(!ctx.g.world.humans.contains_key(human));
    assert_eq!(ctx.g.building_parking(home).unwrap().occupied, 1);
    assert_eq!(ctx.g.building_parking(garage).unwrap().held, 0);
    ctx.g = Bincode::decode(&Bincode::encode(&ctx.g).unwrap()).unwrap();

    assert!(
        (0..500).any(|_| {
            ctx.tick_fast();
            ctx.g.world.vehicles[car].vehicle.state.is_parked()
        }),
        "an ownerless car stayed on the road after rear clearance"
    );
    assert_eq!(ctx.g.building_parking(home).unwrap().occupied, 0);
}

#[test]
fn pathpark_property_departure_finds_reachable_curb_for_opposite_sidewalk() {
    use crate::map::{LaneKind, LanePatternBuilder, ProjectFilter};
    use crate::map_dynamic::{Destination, RoutingStep};
    use crate::transportation::Location;
    use crate::world_command::{CommandOutcomes, WorldCommand};

    let (mut ctx, home, human, car, _) = property_trip();
    let [from, to] = {
        let map = ctx.g.map();
        [vec3(450.0, 280.0, 0.0), vec3(450.0, 360.0, 0.0)]
            .map(|pos| map.project(pos, 0.0, ProjectFilter::ALL))
    };
    // Add a marked crossing without giving the car a turnaround to the
    // opposite lane. The driver must park on a reachable approach and walk.
    ctx.g.apply_commands([&WorldCommand::MapMakeConnection {
        from,
        to,
        inter: None,
        pat: LanePatternBuilder::default().one_way(true).build(),
    }]);
    assert_eq!(ctx.g.read::<CommandOutcomes>().accepted.as_slice(), [0]);
    let destination = {
        let map = ctx.g.map();
        let door = map.get(home).unwrap().door_pos;
        let opposite = vec3(door.x + 10.0, 92.0, door.z);
        let lane = map
            .nearest_lane(opposite, LaneKind::Walking, Some(24.0))
            .unwrap();
        map.get(lane).unwrap().points.project(opposite)
    };
    ctx.g.world.edit_human(human, |person| {
        person.router.go_to(Destination::Outside(destination));
    });
    assert!(
        (0..16_000).any(|_| {
            ctx.g.world.edit_human(human, |person| person.decision.wait = u8::MAX);
            ctx.tick_fast();
            let person = &ctx.g.world.humans[human];
            person.location == Location::Outside
                && matches!(person.router.cur_step(),
                    Some(RoutingStep::WalkProperty(target, _)) if *target == destination)
                && matches!(ctx.g.world.vehicles[car].vehicle.state, VehicleState::Parked(_))
        }),
        "driver at {:?}, car {:?}, parking error {:?}",
        ctx.g.world.humans[human].trans.pos,
        ctx.g.world.vehicles[car].vehicle.state,
        ctx.g.world.humans[human].router.last_error,
    );
}

#[test]
fn pathpark_closing_in_transit_replans_to_another_public_garage() {
    use crate::map::building_parking::{parking_entrance_near, GarageKind};
    use crate::map_dynamic::{Destination, ParkingManagement};
    use crate::world_command::{CommandOutcomes, WorldCommand};

    let (mut ctx, home, human, car, garage) = property_trip();
    let entrance = parking_entrance_near(&ctx.g.map(), vec3(430., 194., 0.)).unwrap();
    ctx.g.apply_commands([&WorldCommand::MapPlaceParkingGarage {
        kind: GarageKind::Underground,
        entrance,
    }]);
    let alternate = ctx
        .g
        .read::<CommandOutcomes>()
        .parking_garage(0)
        .unwrap()
        .unwrap();
    {
        let mut parking = ctx.g.write::<ParkingManagement>();
        for (spot, _) in ctx.g.map().parking.all_spots() {
            parking.mask(spot);
        }
    }
    ctx.g.world.edit_human(human, |h| {
        h.router.go_to(Destination::Building(garage));
    });
    assert!((0..4_000).any(|_| {
        ctx.g.world.edit_human(human, |h| h.decision.wait = u8::MAX);
        ctx.tick_fast();
        ctx.g.building_parking(home).unwrap().occupied == 0
            && ctx.g.building_parking(garage).unwrap().held == 1
            && ctx.g.world.vehicles[car].it.driveway_phase().is_none()
    }));
    ctx.g
        .apply_commands([&WorldCommand::SetBuildingParkingOpen {
            building: garage,
            open: false,
        }]);

    assert!(
        (0..6_000).any(|_| {
            ctx.g.world.edit_human(human, |h| h.decision.wait = u8::MAX);
            ctx.tick_fast();
            ctx.g.building_parking(alternate).unwrap().occupied == 1
        }),
        "the original car did not reach the alternate garage"
    );
    assert_eq!(ctx.g.world.humans[human].router.personal_car, Some(car));
    assert_eq!(ctx.g.building_parking(garage).unwrap().occupied, 0);
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
    ctx.g.world.edit_human(human, |h| {
        h.router.go_to(Destination::Outside(door));
    });
    assert!((0..500).any(|_| {
        ctx.g.world.edit_human(human, |h| h.decision.wait = u8::MAX);
        ctx.tick_fast();
        ctx.g.building_parking(garage).unwrap().held == 1
    }));
}

#[test]
fn pathpark_property_departure_uses_distant_street_bays_when_nearby_supply_is_closed() {
    use crate::map_dynamic::{Destination, ParkingManagement, RoutingStep, NEAR_PARKING_RADIUS};
    use crate::transportation::{Location, VehicleState};
    use crate::world_command::WorldCommand;

    let (mut ctx, home, human, car, garage) = property_trip();
    let door = ctx.g.map().get(garage).unwrap().door_pos;
    ctx.g
        .apply_commands([&WorldCommand::SetBuildingParkingOpen {
            building: garage,
            open: false,
        }]);
    {
        let map = ctx.g.map();
        let mut parking = ctx.g.write::<ParkingManagement>();
        for (spot, data) in map.parking.all_spots() {
            if data.trans.pos.distance(door) <= NEAR_PARKING_RADIUS {
                parking.mask(spot);
            }
        }
        let mut work = 1_024;
        assert!(!parking.property_curb_candidates(&map, door, &mut work).1.is_empty());
    }
    ctx.g.world.edit_human(human, |h| {
        h.router.go_to(Destination::Outside(door));
    });

    let parked = (0..7_000).any(|_| {
        ctx.g.world.edit_human(human, |h| h.decision.wait = u8::MAX);
        ctx.tick_fast();
        let driver = &ctx.g.world.humans[human];
        let vehicle = &ctx.g.world.vehicles[car];
        let walking_to_destination = matches!(
            driver.router.cur_step(),
            Some(RoutingStep::WalkProperty(target, _)) if *target == door
        );
        driver.location == Location::Outside
            && walking_to_destination
            && matches!(vehicle.vehicle.state, VehicleState::Parked(_))
            && vehicle.trans.pos.distance(door) > NEAR_PARKING_RADIUS
    });
    let driver = &ctx.g.world.humans[human];
    let vehicle = &ctx.g.world.vehicles[car];
    assert!(
        parked,
        "driver at {:?} ({:?}) with step {:?}, error {:?}; car at {:?} with state {:?}",
        driver.trans.pos,
        driver.location,
        driver.router.cur_step(),
        driver.router.last_error,
        vehicle.trans.pos,
        vehicle.vehicle.state,
    );
    assert_eq!(ctx.g.world.humans[human].router.personal_car, Some(car));
    assert_eq!(ctx.g.building_parking(home).unwrap().occupied, 0);
    assert_eq!(ctx.g.building_parking(garage).unwrap().occupied, 0);
}

#[test]
fn pathpark_oversized_garage_batch_has_no_side_effects() {
    use crate::map::building_parking::GarageKind;
    use crate::world_command::{
        CommandOutcomes, PrepareBudget, Rejection, WorldCommand, MAX_TILED_COMMANDS,
    };

    let (mut ctx, _, _, _, garage) = property_trip();
    let mut entrance = ctx.g.building_parking(garage).unwrap().entrance;
    entrance.station_cm += 16_000;
    entrance = ctx
        .g
        .map()
        .pick_curb(entrance.road, entrance.side, entrance.station_cm)
        .unwrap();
    let command = WorldCommand::MapPlaceParkingGarage {
        kind: GarageKind::Underground,
        entrance,
    };
    assert!(command.validate_and_price(&ctx.g).is_ok());
    assert!(matches!(
        command.prepare_single(&ctx.g, &mut PrepareBudget::for_preview(1)),
        Err(Rejection::WaterWorkLimit)
    ));
    ctx.g
        .apply_commands(std::iter::repeat_n(&command, MAX_TILED_COMMANDS + 1));
    assert!(ctx.g.read::<CommandOutcomes>().accepted.is_empty());
}

#[test]
fn pathpark_imports_only_suppress_parking_for_the_benchmark_profile() {
    use crate::map::BuildingKind;
    use prototypes::{GrowableChainKey, GrowableZoneKind};

    for curb_only in [false, true] {
        let mut ctx = TestCtx::new();
        if curb_only {
            ctx.g.use_curb_only_benchmark_parking();
        }
        let company = prototypes::prototypes()
            .growable_company_for_chain(GrowableChainKey {
                zone_kind: GrowableZoneKind::Pure(ZoneKind::Commercial),
                density: Density::Low,
                level: 1,
            })
            .unwrap()
            .base
            .id;
        ctx.build_roads(&[vec3(75., 100., 0.), vec3(450., 100., 0.)]);
        let map = ctx.g.map();
        let lot = map
            .lots()
            .values()
            .filter(|lot| {
                let center = lot.shape.center();
                lot.shape.axis().iter().all(|axis| axis.mag() >= 29.5)
                    && center.y < 100.0
                    && (175.0..310.0).contains(&center.x)
                    && !Lot::footprint_obstructed(&map, lot.shape, false)
            })
            .max_by(|a, b| {
                let area = |lot: &Lot| {
                    lot.shape
                        .axis()
                        .iter()
                        .map(|axis| axis.mag())
                        .product::<f32>()
                };
                area(a).total_cmp(&area(b))
            })
            .unwrap()
            .id;
        drop(map);
        let shop = ctx
            .g
            .seed_imported_growable(lot, BuildingKind::GoodsCompany(company), Density::Low)
            .unwrap();
        assert_eq!(ctx.g.building_parking(shop).is_some(), !curb_only);
    }
}

#[test]
fn pathpark_wave3_surface_lot_quote_modes_and_reload() {
    use crate::map::building_parking::{
        lot_footprint, parking_entrance_near, BuildingParkingKind, LotFrame, LotMouth, MouthUse,
        SurfaceLotDraft,
    };
    use crate::world_command::{CommandOutcomes, Rejection, WorldCommand};
    use common::saveload::{Bincode, Encoder};

    let mut ctx = TestCtx::new();
    ctx.build_roads(&[vec3(75., 100., 0.), vec3(450., 100., 0.)]);
    let (frame, inbound, outbound, gate) = {
        let map = ctx.g.map();
        let at = |x| parking_entrance_near(&map, vec3(x, 115., 0.)).unwrap();
        (
            LotFrame::Curb(at(245.)),
            LotMouth {
                at: at(238.),
                mode: MouthUse::In,
            },
            LotMouth {
                at: at(252.),
                mode: MouthUse::Out,
            },
            crate::map::PathEndpoint::Sidewalk(at(255.)),
        )
    };
    let mut draft = SurfaceLotDraft {
        frame,
        width_blocks: 3,
        mouths: [Some(inbound), None],
        foot_gate: Some(gate),
    };
    assert!(
        ctx.g.quote_surface_lot(draft).is_err(),
        "a lot needs a legal exit"
    );
    draft.mouths[1] = Some(outbound);
    let pad = lot_footprint(&ctx.g.map(), frame, draft.width_blocks).unwrap();
    assert_eq!(pad.blocks().count(), 6);
    let (quote, price) = ctx.g.quote_surface_lot(draft).unwrap();
    let command = WorldCommand::MapPlaceParkingLot { draft, quote };
    let rejected = WorldCommand::MapPlaceParkingLot {
        draft,
        quote: quote ^ 1,
    };
    assert_eq!(
        rejected.validate_and_price(&ctx.g),
        Err(Rejection::StaleRoadQuote)
    );
    let before = ctx.g.read::<crate::economy::Government>().money;
    ctx.g.apply_commands([&command]);
    let id = ctx
        .g
        .read::<CommandOutcomes>()
        .parking_lot(0)
        .unwrap()
        .unwrap();
    let summary = ctx.g.building_parking(id).unwrap();
    assert_eq!(summary.kind, BuildingParkingKind::SurfaceLot(draft));
    assert_eq!(usize::from(summary.capacity), pad.stall_centers().count());
    assert_eq!((summary.occupied, summary.held), (0, 0));
    assert_eq!(
        ctx.g.read::<crate::economy::Government>().money,
        before - price
    );

    let checkpoint = Bincode::encode(&ctx.g).unwrap();
    let mut restored: crate::Simulation = Bincode::decode(&checkpoint).unwrap();
    assert_eq!(ctx.g.hashes(), restored.hashes());
    restored.apply_commands([&command]);
    assert!(restored
        .read::<CommandOutcomes>()
        .parking_lot(0)
        .unwrap()
        .is_err());
    assert_eq!(
        restored.read::<crate::economy::Government>().money,
        before - price
    );
    restored.apply_commands([&WorldCommand::MapRemoveBuilding(id)]);
    assert!(restored.building_parking(id).is_none());
    assert!(restored.quote_surface_lot(draft).is_ok());
}

#[test]
fn pathpark_wave3_local_one_way_reverses_with_optional_curb_bays() {
    use crate::map::road_edit::{preview_road_upgrade, road_edit_stamp, RoadUpgrade};
    use crate::map::{LaneKind, SmallRoadPreset};
    use crate::world_command::WorldCommand;

    for curb_parking in [false, true] {
        let mut ctx = TestCtx::new();
        let pat = SmallRoadPreset::LocalOneWay { curb_parking }.lane_pattern();
        ctx.g.apply_commands([&WorldCommand::MapMakeConnection {
            from: crate::map::MapProject::ground(vec3(75., 100., 0.)),
            to: crate::map::MapProject::ground(vec3(245., 100., 0.)),
            inter: None,
            pat,
        }]);
        let (id, src, dst, old_bay) = {
            let map = ctx.g.map();
            let road = map
                .roads()
                .values()
                .find(|road| road.points().first().xy() == vec2(75., 100.))
                .unwrap();
            assert_eq!(road.width, if curb_parking { 12.5 } else { 10.0 });
            assert_eq!(
                road.outgoing_lanes_from(road.src)
                    .iter()
                    .filter(|(_, k)| *k == LaneKind::Driving)
                    .count(),
                1
            );
            assert!(!road
                .outgoing_lanes_from(road.dst)
                .iter()
                .any(|(_, k)| *k == LaneKind::Driving));
            let old_bay = map.parking.all_spots().find_map(|(spot, bay)| {
                road.lanes_iter()
                    .any(|(id, kind)| id == bay.parent && kind == LaneKind::Parking)
                    .then_some(spot)
            });
            assert_eq!(old_bay.is_some(), curb_parking);
            (road.id, road.src, road.dst, old_bay)
        };
        let change = RoadUpgrade::ReverseOneWay;
        let mut budget = crate::world_command::PrepareBudget::for_preview(1_000_000);
        let expected = road_edit_stamp(&ctx.g, id, &mut budget).unwrap();
        let preview = preview_road_upgrade(&ctx.g, id, expected, &change, &mut budget).unwrap();
        let command = WorldCommand::MapUpgradeRoad {
            road: id,
            expected,
            change,
            quote: preview.quote,
        };
        if let Some(spot) = old_bay {
            use crate::map_dynamic::ParkingManagement;
            let reservation = ctx
                .g
                .write::<ParkingManagement>()
                .reserve_exact(spot)
                .unwrap();
            ctx.g.apply_commands([&command]);
            assert!(matches!(
                ctx.g
                    .read::<crate::world_command::CommandOutcomes>()
                    .road_edit(0),
                Some(Err(crate::world_command::Rejection::BusyConflict)),
            ));
            ctx.g.write::<ParkingManagement>().free(reservation);
        }
        ctx.g.apply_commands([&command]);
        {
            let outcomes = ctx.g.read::<crate::world_command::CommandOutcomes>();
            assert_eq!(outcomes.accepted, [0], "{:?}", outcomes.last);
        }
        let map = ctx.g.map();
        let road = map.get(id).unwrap();
        let lane = road
            .lanes_iter()
            .find_map(|(id, k)| (k == LaneKind::Driving).then(|| map.get(id).unwrap()))
            .unwrap();
        assert_eq!((lane.src, lane.dst), (dst, src));
        if let Some(old_bay) = old_bay {
            assert!(map.parking.get(old_bay).is_none());
            let parking_lane = road
                .outgoing_lanes_from(dst)
                .iter()
                .find(|(_, kind)| *kind == LaneKind::Parking)
                .unwrap()
                .0;
            assert!(map.parking.spots(parking_lane).unwrap().next().is_some());
        }
    }
}

#[test]
fn pathpark_wave3_surface_lot_parks_a_visible_car_and_releases_it_after_reload() {
    use crate::economy::TripPurpose;
    use crate::map::building_parking::{
        parking_entrance_near, LotFrame, LotMouth, MouthUse, SurfaceLotDraft,
    };
    use crate::map_dynamic::Destination;
    use crate::transportation::Location;
    use crate::world_command::{CommandOutcomes, WorldCommand};
    use common::saveload::{Bincode, Encoder};

    let (mut ctx, home, human, car, garage) = property_trip();
    ctx.g
        .apply_commands([&WorldCommand::MapRemoveBuilding(garage)]);
    let draft = {
        let map = ctx.g.map();
        let at = |x| parking_entrance_near(&map, vec3(x, 115., 0.)).unwrap();
        SurfaceLotDraft {
            frame: LotFrame::Curb(at(150.)),
            width_blocks: 3,
            mouths: [
                Some(LotMouth {
                    at: at(157.),
                    mode: MouthUse::In,
                }),
                Some(LotMouth {
                    at: at(143.),
                    mode: MouthUse::Out,
                }),
            ],
            foot_gate: Some(crate::map::PathEndpoint::Sidewalk(at(150.))),
        }
    };
    let (quote, _) = ctx.g.quote_surface_lot(draft).unwrap();
    ctx.g
        .apply_commands([&WorldCommand::MapPlaceParkingLot { draft, quote }]);
    let id = ctx
        .g
        .read::<CommandOutcomes>()
        .parking_lot(0)
        .unwrap()
        .unwrap();
    ctx.g.world.edit_human(human, |actor| {
        actor
            .router
            .go_to_for(Destination::Building(id), TripPurpose::Work);
        actor.decision.wait = u8::MAX;
    });
    let arrived = (0..5_000).any(|_| {
        ctx.g
            .world
            .edit_human(human, |actor| actor.decision.wait = u8::MAX);
        ctx.tick_fast();
        ctx.g.world.humans[human].location == Location::Building(id)
    });
    assert!(
        arrived,
        "driver did not arrive: {:?}",
        ctx.g.world.humans[human].router.cur_step()
    );
    assert_eq!(ctx.g.building_parking(id).unwrap().occupied, 1);
    assert!(!ctx.g.car_stored_inside_garage(car));
    assert!(ctx
        .g
        .map()
        .get(id)
        .unwrap()
        .obb
        .contains(ctx.g.world.vehicles[car].trans.pos.xy()));
    ctx.g.apply_commands([&WorldCommand::MapRemoveBuilding(id)]);
    assert!(ctx.g.map().get(id).is_some());
    ctx.g = Bincode::decode(&Bincode::encode(&ctx.g).unwrap()).unwrap();
    ctx.g
        .apply_commands([&WorldCommand::SetBuildingParkingOpen {
            building: id,
            open: false,
        }]);
    ctx.g.world.edit_human(human, |actor| {
        actor
            .router
            .go_to_for(Destination::Building(home), TripPurpose::Home);
        actor.decision.wait = u8::MAX;
    });
    let returned = (0..10_000).any(|_| {
        ctx.g
            .world
            .edit_human(human, |actor| actor.decision.wait = u8::MAX);
        ctx.tick_fast();
        ctx.g.world.humans[human].location == Location::Building(home)
    });
    assert!(returned);
    assert_eq!(ctx.g.world.humans[human].router.personal_car, Some(car));
    assert_eq!(ctx.g.building_parking(id).unwrap().occupied, 0);
    ctx.g.apply_commands([&WorldCommand::MapRemoveBuilding(id)]);
    assert!(ctx.g.map().get(id).is_none());
}
