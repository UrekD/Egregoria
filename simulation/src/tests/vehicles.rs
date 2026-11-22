/*
use crate::map_dynamic::{Destination, Itinerary, ParkingManagement, Router};
use prototypes::GameTime;
use crate::vehicles::{spawn_parked_vehicle, unpark, VehicleKind};
use geom::{vec2, vec3, Vec3};
use crate::map::{Map, PathKind};

use super::*;
use crate::pedestrians::Location;
use crate::souls::desire::{BuyFood, Home};
use crate::souls::human::spawn_human;
use crate::ParCommandBuffer;

#[test]
fn test_car_simple() {
    let mut ctx = TestCtx::init();

    ctx.build_roads(&[vec2(0.0, 0.0), vec2(100.0, 0.0), vec2(100.0, 50.0)]);

    let g = &mut ctx.g;

    let car = spawn_parked_vehicle(g, VehicleKind::Car, Vec3::ZERO).unwrap();
    unpark(g, car);

    let pos = g.pos(car.0).unwrap();

    let spot_id = g
        .write::<ParkingManagement>()
        .reserve_near(vec3(100.0, 50.0, 0.0), &*g.map())
        .unwrap();
    let end_pos = spot_id.park_pos(&*g.map()).unwrap();

    let itin = Itinerary::route(pos, end_pos, &*g.read::<Map>(), PathKind::Vehicle).unwrap();
    *g.comp_mut::<Itinerary>(car.0).unwrap() = itin;

    for _ in 0..1000 {
        ctx.tick();
        if ctx
            .g
            .comp::<Itinerary>(car.0)
            .unwrap()
            .has_ended(ctx.g.read::<GameTime>().timestamp)
        {
            return;
        }
    }

    panic!("car has not arrived after 1000 ticks.")
}

#[test]
fn test_router_and_back() {
    let mut ctx = TestCtx::init();

    ctx.build_roads(&[vec2(0.0, 0.0), vec2(100.0, 0.0), vec2(100.0, 50.0)]);

    let b1 = ctx.build_house_near(vec2(0.0, 0.0));
    let human = spawn_human(&mut ctx.g, b1).unwrap();

    ctx.g
        .write::<ParCommandBuffer>()
        .remove_component::<Desire<Home>>(human.0);
    ctx.g
        .write::<ParCommandBuffer>()
        .remove_component::<Desire<BuyFood>>(human.0);

    let b2 = ctx.build_house_near(vec2(100.0, 5.0));

    for _ in 0..3 {
        ctx.g
            .comp_mut::<Router>(human.0)
            .unwrap()
            .go_to(Destination::Building(b2));

        for i in 0..1000 {
            ctx.tick();
            //log::info!("{}: {:?}", i, ctx.g.comp::<Location>(human.0).unwrap());
            if ctx.g.comp::<Location>(human.0).unwrap() == &Location::Building(b2) {
                break;
            }
            if i == 999 {
                panic!("ped has not arrived after 1000 ticks")
            }
        }

        ctx.g
            .comp_mut::<Router>(human.0)
            .unwrap()
            .go_to(Destination::Building(b1));

        for i in 0..2000 {
            ctx.tick();
            //log::info!("{}: {:?}", i, ctx.g.comp::<Location>(human.0).unwrap());
            if ctx.g.comp::<Location>(human.0).unwrap() == &Location::Building(b1) {
                break;
            }
            if i == 1999 {
                panic!("ped has not arrived after 1000 ticks")
            }
        }
    }
}

#[test]
fn test_router_and_back_change_middle() {
    let mut ctx = TestCtx::init();

    ctx.build_roads(&[vec2(0.0, 0.0), vec2(100.0, 0.0), vec2(100.0, 50.0)]);

    let b1 = ctx.build_house_near(vec2(0.0, 0.0));
    let human = spawn_human(&mut ctx.g, b1).unwrap();

    ctx.g
        .write::<ParCommandBuffer>()
        .remove_component::<Desire<Home>>(human.0);
    ctx.g
        .write::<ParCommandBuffer>()
        .remove_component::<Desire<BuyFood>>(human.0);

    let b2 = ctx.build_house_near(vec2(100.0, 5.0));

    for _ in 0..3 {
        ctx.g
            .comp_mut::<Router>(human.0)
            .unwrap()
            .go_to(Destination::Building(b2));
        for i in 0..1000 {
            ctx.tick();
            //log::info!("{}: {:?}", i, ctx.g.comp::<Location>(human.0).unwrap());

            if matches!(
                ctx.g.comp::<Location>(human.0).unwrap(),
                &Location::Vehicle(_)
            ) && i > 300
            {
                ctx.g
                    .comp_mut::<Router>(human.0)
                    .unwrap()
                    .go_to(Destination::Building(b1));
                break;
            }
            if i == 999 {
                panic!("not arrived")
            }
        }

        for i in 0..1000 {
            ctx.tick();
            //log::info!("{}: {:?}", i, ctx.g.comp::<Location>(human.0).unwrap());
            if ctx.g.comp::<Location>(human.0).unwrap() == &Location::Building(b1) {
                break;
            }
            if i == 999 {
                panic!("not arrived")
            }
        }
    }
}*/

#[test]
fn pf_d_parked_vehicle_revalidates_reused_spot_and_after_load() {
    use crate::transportation::road::vehicle_state_update_system;
    use crate::transportation::{spawn_parked_vehicle, VehicleKind, VehicleState};
    use crate::{Simulation, VehicleID};
    use common::saveload::{Bincode, Encoder};
    use geom::vec3;

    fn validate(sim: &mut Simulation) {
        let (world, resources) = sim.world_res();
        vehicle_state_update_system(world, resources);
        sim.apply_commands([]);
    }

    fn spot(sim: &Simulation, car: VehicleID) -> crate::map::ParkingSpot {
        let VehicleState::Parked(spot) = &sim.get(car).unwrap().vehicle.state else {
            panic!("expected a parked car");
        };
        *spot.get(&sim.map().parking).unwrap()
    }

    let mut ctx = super::TestCtx::new_fast();
    ctx.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(400.0, 100.0, 0.0)]);
    let car = spawn_parked_vehicle(&mut ctx.g, VehicleKind::Car, vec3(230.0, 100.0, 0.0)).unwrap();
    validate(&mut ctx.g);
    let original = spot(&ctx.g, car);
    {
        let mut map = ctx.g.map_mut();
        let mut lane = map.lanes()[original.parent].clone();
        lane.points = lane
            .points
            .iter()
            .map(|p| *p + vec3(0.0, 0.0, 1.0))
            .collect::<Vec<_>>()
            .into();
        map.parking.generate_spots(&lane);
        map.parking.clean_reuse();
    }
    let moved = spot(&ctx.g, car);
    assert_ne!(original.trans, moved.trans);
    validate(&mut ctx.g);
    assert_eq!(ctx.g.get(car).unwrap().trans, moved.trans);

    ctx.g.world.vehicles[car].trans = original.trans;
    let mut restored: Simulation = Bincode::decode(&Bincode::encode(&ctx.g).unwrap()).unwrap();
    validate(&mut restored);
    assert_eq!(restored.get(car).unwrap().trans, moved.trans);
    restored.map_mut().parking.remove_spots(moved.parent);
    validate(&mut restored);
    assert!(restored.get(car).is_none());
}
