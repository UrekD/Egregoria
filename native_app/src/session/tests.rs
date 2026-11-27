use super::*;
use crate::debug_gui::debug_window::DebugState;
use crate::gui::bulldozer::BulldozerState;
use crate::gui::follow::FollowEntity;
use crate::gui::overlays::{self, OverlayMode};
use crate::gui::roadbuild::{BuildState, RoadBuildResource};
use crate::gui::roadeditor::{IntersectionComponent, RoadEditorResource};
use crate::gui::specialbuilding::SpecialBuildingResource;
use crate::gui::terraforming::TerraformingResource;
use crate::gui::toolbox::transit::TransitTools;
use crate::gui::windows::load::LoadState;
use crate::gui::windows::transit_lines::PanelState;
use crate::gui::zoneedit::ZoneEditState;
use crate::gui::zoning::ZoningState;
use crate::gui::{
    ErrorTooltip, ExitState, InspectedBuilding, InspectedEntity, PotentialCommands, ToastState,
    Tool,
};
use crate::inputmap::{InputAction, InputMap};
use crate::rendering::immediate::{ImmediateDraw, ImmediateSound};
use crate::rendering::OrbitCamera;
use crate::uiworld::{ReceivedCommands, SaveLoadState, UiWorld};
use common::saveload::SlotId;
use engine::{AudioKind, InputContext, MouseButton};
use geom::{vec2, vec3, Camera, Color, Transform, Vec2, Vec3, OBB};
use prototypes::Money;
use scenarios::ScenarioCtx;
use simulation::map::{MapSubscriber, UpdateType};
use simulation::map_dynamic::{LineID, Transit, TransitLine, TransitStop};
use simulation::transportation::{spawn_driving_vehicle, VehicleKind};
use simulation::utils::scheduler::SeqSchedule;
use simulation::world_command::{WorldCommand, WorldCommands};
use simulation::{AnyEntity, Simulation, SimulationOptions, VehicleID};

#[test]
fn p9_recorded_p6_snapshot_warns_and_requires_new_slot() {
    use common::saveload::{Bincode, Encoder};

    scenarios::ensure_repo_root_cwd();
    crate::init::init();
    let root = std::env::temp_dir().join(format!("egregoria-p7-recorded-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let slot = SlotId::new("old-recording").unwrap();
    let city = Simulation::new_game_with_replay(&simulation::NewGameOptions {
        terrain_chunks: 16,
        seed: 6042,
        ..simulation::NewGameOptions::default()
    })
    .unwrap();
    // The unwrapped snapshot encodes its version, world and resource payloads in that order.
    let (_, world, mut resources): (String, simulation::World, BTreeMap<String, Vec<u8>>) =
        Bincode::decode(&Bincode::encode(&city).unwrap()).unwrap();
    for name in ["service_incidents", "site_remnants", "fire_fleet"] {
        assert!(resources.remove(name).is_some());
    }
    let registry = prototypes::prototypes().pre_fire_fingerprint();
    resources.insert("prototype_registry".into(), Bincode::encode(&registry).unwrap());
    resources.insert(
        "replay".into(),
        format!(r#"{{"version":"0.11.1","enabled":true,"registry":{registry},"commands":[]}}"#)
            .into_bytes(),
    );
    std::fs::write(
        root.join("old-recording.bc"),
        Bincode::encode(&("0.11.1", world, resources)).unwrap(),
    )
    .unwrap();
    let loaded = Simulation::load_from_slot(&root, &slot);
    std::fs::remove_dir_all(root).unwrap();
    let mut uiw = UiWorld::with_defaults();
    let camera = Camera::new(vec3(4000.0, 4000.0, 25.0), 1280.0, 720.0);
    uiw.insert(camera);
    uiw.insert(OrbitCamera::from_camera(camera));
    let city = install_world(&uiw, WorldReplacement::Snapshot(loaded.unwrap(), slot), 1);
    assert_eq!(city.get_tick(), 1);
    assert!(city.read::<simulation::Replay>().migrated_from_legacy_slot());
    assert_eq!(uiw.read::<SessionState>().city_name, "old-recording");
    assert!(uiw.read::<SessionState>().snapshot_slot().is_none());
    assert!(uiw
        .read::<SaveLoadState>()
        .notice
        .as_ref()
        .is_some_and(|(_, message)| {
            message.contains("Save to a new slot") && message.contains("Replay is unavailable")
        }));
}

#[test]
fn p4_world_replacement_clears_world_identity_and_requires_fresh_input() {
    fn world(fare: i64) -> (Simulation, LineID, VehicleID) {
        let mut sim = ScenarioCtx::new(SimulationOptions {
            terrain_size: 16,
            save_replay: false,
            ..SimulationOptions::LEGACY
        })
        .sim;
        let mut transit = sim.write::<Transit>();
        let stops = [10.0, 30.0].map(|x| {
            transit.stops.insert_with_key(|id| TransitStop {
                id,
                pos: vec3(x, 20.0, 0.0),
                dir: vec2(1.0, 0.0),
                walk_pos: vec3(x, 25.0, 0.0),
                berth: vec3(x, 20.0, 0.0),
            })
        });
        let line = transit.lines.insert_with_key(|id| TransitLine {
            id,
            stops: stops.to_vec(),
            fare: Money::new_bucks(fare),
            target_vehicles: 0,
            color: Color::BLUE,
            suspended: fare == 1,
        });
        drop(transit);
        let vehicle = spawn_driving_vehicle(
            &mut sim,
            VehicleKind::Car,
            Transform::new_dir(vec3(fare as f32 * 10.0, 30.0, 0.0), Vec3::X),
        )
        .unwrap();
        (sim, line, vehicle)
    }

    let (mut sim, line, vehicle) = world(1);
    let (replacement, next_line, next_vehicle) = world(2);
    assert_eq!(line, next_line);
    assert_eq!(vehicle, next_vehicle);
    assert_ne!(
        sim.get(vehicle).unwrap().trans.pos,
        replacement.get(vehicle).unwrap().trans.pos
    );
    crate::init::init();
    let mut uiw = UiWorld::with_defaults();
    let building = sim.map().buildings().keys().next().unwrap();
    let intersection = sim.map().intersections().keys().next().unwrap();
    assert!(replacement.map().buildings().contains_key(building));
    assert!(replacement.map().intersections().contains_key(intersection));
    let mut camera = Camera::new(vec3(4000.0, 4000.0, 25.0), 1280.0, 720.0);
    camera.update();
    uiw.insert(camera);
    uiw.insert(OrbitCamera::from_camera(camera));
    uiw.camera_mut().follow(vec3(1.0, 2.0, 3.0));
    uiw.write::<InputMap>()
        .build_input_tree(&mut uiw.write::<crate::inputmap::Bindings>());
    let entity = AnyEntity::VehicleID(vehicle);
    *uiw.write::<InspectedBuilding>() = InspectedBuilding {
        e: Some(building),
        dontclear: true,
    };
    *uiw.write::<InspectedEntity>() = InspectedEntity {
        e: Some(entity),
        dist2: 100.0,
        dontclear: true,
    };
    uiw.write::<FollowEntity>().0 = Some(entity);
    let quote = simulation::souls::demolition::demolition_quote(&sim, building).unwrap().quote;
    let cmd = WorldCommand::MapStartDemolition { site: building, quote };
    uiw.commands().push(cmd.clone());
    *uiw.write::<ReceivedCommands>() =
        ReceivedCommands::new(WorldCommands::from(vec![cmd.clone()]));
    uiw.write::<PotentialCommands>().set(cmd);
    *uiw.write::<ErrorTooltip>() = ErrorTooltip::new("old placement");
    *uiw.write::<Tool>() = Tool::RoadEditor;
    uiw.write::<RoadBuildResource>().build_state = BuildState::Start(sim.map().project(
        Vec3::ZERO,
        0.0,
        simulation::map::ProjectFilter::ALL,
    ));
    *uiw.write::<RoadEditorResource>() = RoadEditorResource {
        inspect: Some(IntersectionComponent {
            id: intersection,
            light_policy: Default::default(),
            turn_policy: Default::default(),
        }),
        dirty: true,
    };
    uiw.write::<SpecialBuildingResource>().last_obb =
        Some(OBB::new(Vec2::ZERO, Vec2::X, 10.0, 10.0));
    uiw.write::<ImmediateDraw>()
        .circle(Vec3::ZERO, 1.0)
        .persistent();
    uiw.write::<ImmediateDraw>().circle(Vec3::ZERO, 1.0);
    uiw.write::<ImmediateSound>().play("old", AudioKind::Effect);
    *uiw.write::<ExitState>() = ExitState::Saving;
    let old_status = uiw.read::<SaveLoadState>().saving_status.clone();
    old_status.store(true, Ordering::SeqCst);
    let mut replay = simulation::Replay::default();
    replay.push(
        prototypes::Tick(sim.get_tick()),
        WorldCommand::Init(Box::new(SimulationOptions {
            terrain_size: simulation::NewGameOptions::MIN_TERRAIN_CHUNKS,
            ..SimulationOptions::LEGACY
        })),
    );
    let (_, replay) = Simulation::from_replay(replay);
    let mut save_load = uiw.write::<SaveLoadState>();
    save_load.please_save = true;
    save_load.replay = Some(replay);
    drop(save_load);
    uiw.write::<LoadState>().has_save = true;
    uiw.write::<GuiState>().last_save = Instant::now() - std::time::Duration::from_secs(90);
    uiw.write::<BulldozerState>().start_test_gesture();
    uiw.write::<ZoneEditState>().start_test_gesture();
    uiw.write::<ZoningState>().start_test_gesture();
    uiw.write::<TerraformingResource>().start_test_gesture();
    uiw.write::<DebugState>().connectivity = (
        Some(MapSubscriber::new(UpdateType::Road)),
        vec![vec![intersection]],
    );
    let old_line = sim.read::<Transit>().lines[line].clone();
    uiw.write::<TransitTools>()
        .seed_draft(line, &old_line.stops);
    uiw.write::<PanelState>().seed_edit(&old_line);
    sim.apply_commands([&WorldCommand::MapBuildHouse(Default::default())]);
    uiw.write::<ToastState>().track_test_world(&sim);
    assert!(!uiw.read::<ToastState>().world_state_is_clear());
    *uiw.write::<OverlayMode>() = OverlayMode::Power;
    overlays::draw(&sim, &uiw);
    overlays::view::bounds(&uiw.read::<Camera>(), &sim.map());
    assert!(!overlays::world_cache_is_clear());
    uiw.write::<ImmediateDraw>()
        .circle(Vec3::ZERO, 1.0)
        .persistent();

    #[derive(Default)]
    struct ResetGeneration(u64);
    uiw.insert(ResetGeneration::default());
    uiw.write::<WorldResetHooks>()
        .register(|world, generation| world.write::<ResetGeneration>().0 = generation);

    let mut pointer = InputContext::default();
    pointer.mouse.pressed.insert(MouseButton::Left);
    let mut input = uiw.write::<InputMap>();
    input.prepare_frame(&pointer, true, true);
    assert!(input.just_act.contains(&InputAction::Select));
    input.ray = Some(geom::Ray3 {
        from: Vec3::ZERO,
        dir: Vec3::Z,
    });
    input.unprojected = Some(Vec3::ZERO);
    input.unprojected_normal = Some(Vec3::Z);
    drop(input);

    *uiw.write::<NetworkState>() = NetworkState::Singleplayer(common::timestep::Timestep::new(
        std::time::Duration::from_secs(90),
    ));
    let mut schedule = SeqSchedule::default();
    let slot = SlotId::new("replacement").unwrap();
    let expected = replacement.hashes();
    let mut runtime_reset = false;
    apply_world_replacement(
        &uiw,
        &mut sim,
        &mut schedule,
        WorldReplacement::Snapshot(replacement, slot.clone()),
        |sim| {
            assert_eq!(sim.hashes(), expected);
            runtime_reset = true;
        },
    );

    assert!(runtime_reset);
    assert!(!schedule.times().is_empty());
    assert!(
        matches!(&*uiw.read::<NetworkState>(), NetworkState::Singleplayer(step)
        if step.period == common::timestep::debug_up_dt())
    );
    assert_eq!(uiw.read::<ResetGeneration>().0, 1);
    assert!(!old_status.load(Ordering::SeqCst));
    assert!(!uiw.read::<LoadState>().has_save);
    assert!(uiw.read::<SaveLoadState>().replay.is_none());
    assert!(uiw.read::<GuiState>().last_save.elapsed().as_secs() < 90);
    assert!(uiw.read::<DebugState>().connectivity.0.is_none());
    assert!(uiw.read::<DebugState>().connectivity.1.is_empty());
    assert!(uiw.read::<BulldozerState>().gesture_is_clear());
    assert!(uiw.read::<ZoneEditState>().gesture_is_clear());
    assert!(uiw.read::<ZoningState>().gesture_is_clear());
    assert!(uiw.read::<TerraformingResource>().gesture_is_clear());
    assert!(uiw.read::<ToastState>().world_state_is_clear());
    assert!(uiw.read::<TransitTools>().draft_is_clear());
    assert!(uiw.read::<PanelState>().is_clear());
    assert!(overlays::world_cache_is_clear());
    assert_eq!(sim.hashes(), expected);
    assert!(uiw.read::<InspectedBuilding>().e.is_none());
    assert!(!uiw.read::<InspectedBuilding>().dontclear);
    assert!(uiw.read::<InspectedEntity>().e.is_none());
    assert!(!uiw.read::<InspectedEntity>().dontclear);
    assert!(uiw.read::<FollowEntity>().0.is_none());
    assert!(uiw.commands().is_empty());
    assert_eq!(uiw.received_commands().iter().count(), 0);
    assert!(uiw.read::<PotentialCommands>().0.is_empty());
    assert!(uiw.read::<ErrorTooltip>().msg.is_none());
    assert!(matches!(*uiw.read::<Tool>(), Tool::Hand));
    assert!(matches!(
        uiw.read::<RoadBuildResource>().build_state,
        BuildState::Hover
    ));
    assert!(uiw.read::<RoadEditorResource>().inspect.is_none());
    assert!(!uiw.read::<RoadEditorResource>().dirty);
    assert!(uiw.read::<SpecialBuildingResource>().last_obb.is_none());
    assert!(uiw.read::<ImmediateDraw>().persistent_orders.is_empty());
    assert!(uiw.read::<ImmediateDraw>().orders.is_empty());
    assert!(uiw.read::<ImmediateSound>().orders.is_empty());
    assert!(matches!(*uiw.read::<ExitState>(), ExitState::NoExit));
    assert!(!uiw.read::<SaveLoadState>().please_save);
    assert_eq!(uiw.read::<SessionState>().active_slot.as_ref(), Some(&slot));
    assert_eq!(uiw.read::<SessionState>().generation, 1);
    assert!(!world_interaction(&uiw));
    assert_ne!(uiw.read::<Camera>().pos, vec3(1.0, 2.0, 3.0));

    let mut input = uiw.write::<InputMap>();
    assert!(input.ray.is_none());
    assert!(input.unprojected.is_none());
    assert!(input.unprojected_normal.is_none());
    input.prepare_frame(&pointer, true, true);
    assert!(!input.act.contains(&InputAction::Select));
    pointer.mouse.pressed.clear();
    input.observe_pointer_buttons(false);
    input.prepare_frame(&pointer, true, false);
    pointer.mouse.pressed.insert(MouseButton::Left);
    input.prepare_frame(&pointer, true, true);
    assert!(input.just_act.contains(&InputAction::Select));
    drop(input);

    // The panel normally drains commands when the toolbox next renders.
    uiw.write::<SessionState>().interaction_blocked = false;
    assert!(uiw
        .read::<GuiState>()
        .windows
        .update_transit_lines(&uiw, &sim)
        .is_none());
    assert!(uiw.commands().is_empty());
    assert_eq!(sim.read::<Transit>().lines[line].fare, Money::new_bucks(2));
}

#[test]
fn scale_t1_publication_gate_is_polled_for_new_generation() {
    let coordinator = save_coordinator::SaveCoordinator::default();
    let slot = SlotId::legacy();
    let permit = coordinator.request(0, &slot).unwrap();
    let (locked, ready) = std::sync::mpsc::channel();
    let (unlock, done) = std::sync::mpsc::channel();
    let job = std::thread::spawn({
        let coordinator = coordinator.clone();
        move || {
            coordinator.publish(&permit, || {
                locked.send(()).unwrap();
                done.recv_timeout(Duration::from_secs(5)).unwrap()
            })
        }
    });
    ready.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(!coordinator.try_swap(1));
    assert!(coordinator.request(0, &slot).is_none());
    unlock.send(()).unwrap();
    assert_eq!(job.join().unwrap(), Some(()));
    assert!(coordinator.try_swap(1));
    assert!(coordinator.request(0, &slot).is_none());
    assert!(coordinator.request(1, &slot).is_some());
}

#[test]
fn scale_t1_screen_transition_retains_staged_input() {
    scenarios::ensure_repo_root_cwd();
    crate::init::init();
    let uiw = UiWorld::with_defaults();
    uiw.commands()
        .push(WorldCommand::SpawnRandomCars { n_cars: 0 });
    assert!(set_screen(&uiw, SessionScreen::MainMenu));
    assert_eq!(uiw.commands().as_ref().len(), 1);
}
