use crate::inputmap::{InputAction, InputMap};
use crate::uiworld::UiWorld;
use geom::Vec3;
use simulation::map::BuildingID;
use simulation::transportation::Location;
use simulation::{AnyEntity, Simulation, VehicleID};

/// FollowEntity is a component that tells the camera to follow an entity
#[derive(Default)]
pub struct FollowEntity(pub Option<AnyEntity>);

impl FollowEntity {
    pub fn update_camera(uiw: &UiWorld, sim: &Simulation) {
        let just = &uiw.read::<InputMap>().just_act;
        if [
            InputAction::CameraMove,
            InputAction::GoForward,
            InputAction::GoBackward,
            InputAction::GoLeft,
            InputAction::GoRight,
        ]
        .iter()
        .any(|x| just.contains(x))
        {
            uiw.write::<FollowEntity>().0.take();
        }

        Self::track(uiw, sim);
    }

    /// Move the camera to the followed entity, and stop following once it is despawned.
    pub(crate) fn track(uiw: &UiWorld, sim: &Simulation) {
        let Some(e) = uiw.read::<FollowEntity>().0 else {
            return;
        };
        if !sim.contains(e) {
            uiw.write::<FollowEntity>().0 = None;
            return;
        }
        if let Some(pos) = follow_pos(sim, e) {
            uiw.camera_mut().follow(pos);
        }
    }
}

/// Where the camera should look when following `e`.
pub(crate) fn follow_pos(sim: &Simulation, e: AnyEntity) -> Option<Vec3> {
    let AnyEntity::HumanID(id) = e else {
        return sim.pos_any(e);
    };
    let human = sim.get(id)?;
    if let Location::RailTransit(train) = human.location {
        return sim.pos(train).or(Some(human.trans.pos));
    }
    Some(human_follow_pos(
        human.trans.pos,
        &human.location,
        |v| sim.pos(v),
        |b| sim.map().buildings().get(b).map(|b| b.door_pos),
    ))
}

/// A human's transform stays where they left the street, so follow whatever carries them.
pub(crate) fn human_follow_pos(
    human_pos: Vec3,
    location: &Location,
    vehicle_pos: impl FnOnce(VehicleID) -> Option<Vec3>,
    door_pos: impl FnOnce(BuildingID) -> Option<Vec3>,
) -> Vec3 {
    match *location {
        Location::Outside | Location::RailTransit(_) | Location::TransitHeld => Some(human_pos),
        Location::Vehicle(v) | Location::Transit(v) => vehicle_pos(v),
        Location::Building(b) => door_pos(b),
    }
    .unwrap_or(human_pos)
}

#[cfg(test)]
mod tests {
    use super::human_follow_pos;
    use geom::Vec3;
    use simulation::map::BuildingID;
    use simulation::transportation::Location;
    use simulation::VehicleID;

    const HUMAN: Vec3 = Vec3::new(1.0, 2.0, 3.0);
    const CAR: Vec3 = Vec3::new(40.0, 50.0, 0.0);
    const DOOR: Vec3 = Vec3::new(-7.0, 8.0, 1.0);

    fn resolve(location: Location, vehicle: Option<Vec3>, door: Option<Vec3>) -> Vec3 {
        human_follow_pos(HUMAN, &location, |_| vehicle, |_| door)
    }

    #[test]
    fn follow1_human_outside_follows_human() {
        assert_eq!(resolve(Location::Outside, Some(CAR), Some(DOOR)), HUMAN);
    }

    #[test]
    fn follow1_human_in_vehicle_follows_vehicle() {
        let car = VehicleID::default();
        assert_eq!(resolve(Location::Vehicle(car), Some(CAR), None), CAR);
        assert_eq!(resolve(Location::Transit(car), Some(CAR), None), CAR);
    }

    #[test]
    fn follow1_human_in_building_follows_door() {
        let b = BuildingID::default();
        assert_eq!(resolve(Location::Building(b), None, Some(DOOR)), DOOR);
    }

    #[test]
    fn follow1_missing_vehicle_or_building_falls_back_to_human() {
        let car = VehicleID::default();
        assert_eq!(resolve(Location::Vehicle(car), None, None), HUMAN);
        assert_eq!(resolve(Location::Transit(car), None, None), HUMAN);
        let b = BuildingID::default();
        assert_eq!(resolve(Location::Building(b), None, None), HUMAN);
    }

    #[test]
    fn follow1_track_keeps_following_a_human_inside_a_car_until_despawn() {
        use super::FollowEntity;
        use crate::rendering::OrbitCamera;
        use crate::uiworld::UiWorld;
        use geom::{vec2, vec3, Camera, Transform};
        use scenarios::builders::{fill_houses, grid_city};
        use scenarios::ScenarioCtx;
        use simulation::souls::human::spawn_human;
        use simulation::souls::lifecycle::despawn_human;
        use simulation::transportation::{spawn_driving_vehicle, VehicleKind};
        use simulation::SimulationOptions;

        let mut ctx = ScenarioCtx::new(SimulationOptions {
            terrain_size: 16,
            save_replay: false,
            ..SimulationOptions::LEGACY
        });
        crate::init::init();
        let mut uiw = UiWorld::with_defaults();
        uiw.insert(OrbitCamera::from_camera(Camera::new(
            Vec3::ZERO,
            1280.0,
            720.0,
        )));
        grid_city(&mut ctx, vec2(704.0, 704.0), 2, 2, 200.0);
        let house = fill_houses(&mut ctx, 1)[0];
        let sim = &mut ctx.sim;
        let human = spawn_human(sim, house).unwrap();
        let car_pos = vec3(800.0, 900.0, 0.0);
        let car = spawn_driving_vehicle(sim, VehicleKind::Car, Transform::new(car_pos)).unwrap();
        let human_pos = sim.pos(human).unwrap();
        assert!(human_pos.distance(car_pos) > 10.0);

        uiw.write::<FollowEntity>().0 = Some(human.into());
        FollowEntity::track(&uiw, sim);
        assert_eq!(uiw.camera().targetpos, human_pos);

        sim.world_mut_unchecked().humans[human].location = Location::Vehicle(car);
        FollowEntity::track(&uiw, sim);
        assert_eq!(uiw.camera().targetpos, car_pos);

        let moved = car_pos + vec3(50.0, 0.0, 0.0);
        sim.world_mut_unchecked().vehicles[car].trans.pos = moved;
        FollowEntity::track(&uiw, sim);
        assert_eq!(uiw.camera().targetpos, moved);
        assert_eq!(uiw.read::<FollowEntity>().0, Some(human.into()));

        despawn_human(sim, human);
        ctx.run_ticks(1);
        FollowEntity::track(&uiw, &ctx.sim);
        assert_eq!(uiw.read::<FollowEntity>().0, None);
    }
}
