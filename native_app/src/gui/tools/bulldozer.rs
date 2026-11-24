use std::collections::BTreeSet;

use common::FastSet;

use crate::gui::specialbuilding::SpecialBuildingResource;
use crate::gui::windows::settings::Settings;
use crate::gui::Tool;
use crate::inputmap::{
    water_environment, Bindings, DeckPicker, InputAction, InputMap, SurfaceHits, UnitInput,
};
use crate::rendering::immediate::ImmediateDraw;
use crate::uiworld::UiWorld;
use engine::MouseButton;
use geom::{Camera, Ray3, Vec2};
use simulation::map::{BuildingKind, Map, MapProject, ProjectFilter, ProjectKind};
use simulation::world_command::WorldCommands;
use simulation::Simulation;

#[derive(Default)]
pub struct BulldozerState {
    gesture: Gesture,
    initiating_button: Option<MouseButton>,
    previous_buttons: FastSet<MouseButton>,
    press_order: Vec<MouseButton>,
}

#[derive(Default)]
enum Gesture {
    #[default]
    Idle,
    Cancelled,
    Sweeping {
        previous: Vec2,
        removed: BTreeSet<ProjectKind>,
    },
}

impl BulldozerState {
    pub(crate) fn record_press_order(&mut self, events: &[egui::Event]) {
        self.press_order.clear();
        for event in events {
            let egui::Event::PointerButton {
                button, pressed, ..
            } = event
            else {
                continue;
            };
            let button = match button {
                egui::PointerButton::Primary => MouseButton::Left,
                egui::PointerButton::Secondary => MouseButton::Right,
                egui::PointerButton::Middle => MouseButton::Middle,
                egui::PointerButton::Extra1 => MouseButton::Other(4),
                egui::PointerButton::Extra2 => MouseButton::Other(5),
            };
            if *pressed {
                if !self.press_order.contains(&button) {
                    self.press_order.push(button);
                }
            } else {
                self.press_order.retain(|&pending| pending != button);
            }
        }
    }

    pub fn reset_gesture(&mut self) {
        self.gesture = Gesture::Idle;
        self.initiating_button = None;
        self.previous_buttons.clear();
        self.press_order.clear();
    }

    fn cancel(&mut self, held: bool) {
        if held {
            self.gesture = Gesture::Cancelled;
        } else {
            self.reset_gesture();
        }
    }

    fn drag_pressed(
        &mut self,
        pointer: &engine::PointerFrame,
        binding_order: impl Fn(MouseButton) -> Option<usize>,
        selecting: bool,
        just_selected: bool,
    ) -> bool {
        if self.initiating_button.is_some_and(|button| {
            pointer.released_buttons.contains(&button) || !pointer.held_buttons.contains(&button)
        }) {
            self.gesture = Gesture::Idle;
            self.initiating_button = None;
        }

        // InputMap only reports transitions of the aggregate Select action. A
        // newly held binding can start a stroke even when Select stayed active.
        let fresh = |button| {
            !self.previous_buttons.contains(&button) || pointer.released_buttons.contains(&button)
        };
        let button = pointer
            .held_buttons
            .iter()
            .filter_map(|&button| {
                let binding = binding_order(button)?;
                let press = self
                    .press_order
                    .iter()
                    .position(|&pressed| pressed == button);
                // egui reports the common buttons in event order. Binding order
                // breaks ties for physical buttons it does not report.
                Some((
                    (!fresh(button), press.unwrap_or(usize::MAX), binding, button),
                    button,
                ))
            })
            .min_by_key(|&(priority, _)| priority)
            .map(|(_, button)| button);
        let pressed =
            matches!(self.gesture, Gesture::Idle) && (just_selected || button.is_some_and(fresh));
        self.previous_buttons.clone_from(&pointer.held_buttons);
        if pressed {
            self.initiating_button = button;
        }
        selecting && pressed
    }

    fn update_drag(
        &mut self,
        held: bool,
        pressed: bool,
        current: Vec2,
        map: &Map,
        mut project: impl FnMut(Vec2) -> Option<MapProject>,
        commands: &mut WorldCommands,
    ) {
        if !held {
            self.reset_gesture();
            return;
        }

        if pressed && matches!(self.gesture, Gesture::Idle) {
            self.gesture = Gesture::Sweeping {
                previous: current,
                removed: BTreeSet::new(),
            };
        }

        let Gesture::Sweeping { previous, removed } = &mut self.gesture else {
            return;
        };
        let mut interrupted = false;
        let targets = sweep_targets(*previous, current, |point| {
            let hit = project(point).filter(|hit| hit.pos.is_finite());
            interrupted |= hit.is_none();
            hit
        });

        for target in targets {
            // Road removal already removes both endpoints if they have no roads left.
            // Queuing an intersection too would cascade into duplicate removals.
            if removed.insert(target) {
                queue_target(map, target, commands);
            }
        }
        if interrupted {
            self.cancel(true);
        } else {
            *previous = current;
        }
    }
}

fn drag_filter() -> ProjectFilter {
    ProjectFilter::ROAD | ProjectFilter::BUILDING
}

/// Visit targets in stroke order, stopping at a UI hit or a missing terrain projection.
fn sweep_targets(
    previous: Vec2,
    current: Vec2,
    mut project: impl FnMut(Vec2) -> Option<MapProject>,
) -> Vec<ProjectKind> {
    const MAX_STEP_PX: f32 = 8.0;
    let distance = (current - previous).mag();
    if !distance.is_finite() {
        return Vec::new();
    }
    let steps = (distance / MAX_STEP_PX).ceil() as usize;
    let mut seen = BTreeSet::new();
    let mut targets = Vec::new();
    for step in 0..=steps {
        let point = previous.lerp(current, step as f32 / steps.max(1) as f32);
        let Some(hit) = project(point).filter(|hit| hit.pos.is_finite()) else {
            break;
        };
        let target = hit.kind;
        if target != ProjectKind::Ground && seen.insert(target) {
            targets.push(target);
        }
    }
    targets
}

fn over_ui(point: Vec2) -> bool {
    goryak::pointer_over_ui(point.x, point.y) || engine::pointer_over_egui(point)
}

fn screen_ray(camera: &Camera, point: Vec2) -> Option<Ray3> {
    if !point.is_finite()
        || point.x < 0.0
        || point.y < 0.0
        || point.x >= camera.viewport_w
        || point.y >= camera.viewport_h
        || over_ui(point)
    {
        return None;
    }

    camera.unproj_ray(point)
}

fn project_target(
    map: &Map,
    picker: &mut DeckPicker<'_>,
    ray: Ray3,
    filter: ProjectFilter,
) -> Option<MapProject> {
    let hits = SurfaceHits::cast(water_environment(map), ray);
    if let Some(deck) = picker.pick(ray, &hits, filter) {
        return Some(deck);
    }
    if let Some(water) = hits.water {
        return Some(MapProject {
            pos: water.point,
            kind: ProjectKind::Ground,
        });
    }
    let (ground, _) = hits.ground?;
    Some(map.project(ground.xy().z(ground.z.max(0.0)), 0.0, filter))
}

fn queue_target(map: &Map, target: ProjectKind, commands: &mut WorldCommands) {
    match target {
        ProjectKind::Intersection(id) => commands.map_remove_intersection(id),
        ProjectKind::Road(id) => commands.map_remove_road(id),
        ProjectKind::Building(id) => {
            if map
                .get(id)
                .is_some_and(|building| !matches!(building.kind, BuildingKind::ExternalTrading))
            {
                commands.map_remove_building(id);
            }
        }
        ProjectKind::Ground | ProjectKind::Lot(_) => {}
    }
}

fn is_select_button(bindings: &Bindings, button: MouseButton) -> bool {
    select_binding_order(bindings, button).is_some()
}

fn select_binding_order(bindings: &Bindings, button: MouseButton) -> Option<usize> {
    bindings.0.get(&InputAction::Select).and_then(|select| {
        select.0.iter().position(|binding| {
            let mut combination = binding.clone();
            combination.push_unique(UnitInput::Mouse(button));
            combination == *binding
        })
    })
}

pub fn bulldozer(sim: &Simulation, uiworld: &UiWorld) {
    profiling::scope!("gui::bulldozer");
    let input = uiworld.read::<InputMap>();
    let pointer = engine::pointer_frame();
    let selecting = input.act.contains(&InputAction::Select);
    let bindings = uiworld.read::<Bindings>();
    let select_button = |&button: &MouseButton| is_select_button(&bindings, button);
    let held = selecting || pointer.held_buttons.iter().any(select_button);
    let mut state = uiworld.write::<BulldozerState>();
    let drag_enabled = uiworld.read::<Settings>().bulldozer_drag;
    let just_selected = input.just_act.contains(&InputAction::Select);
    let pressed = if drag_enabled {
        state.drag_pressed(
            &pointer,
            |button| select_binding_order(&bindings, button),
            selecting,
            just_selected,
        )
    } else {
        just_selected
            || selecting
                && pointer
                    .held_buttons
                    .intersection(&pointer.released_buttons)
                    .any(select_button)
    };
    if *uiworld.read::<Tool>() != Tool::Bulldozer || pointer.blocked || over_ui(input.screen) {
        state.cancel(held);
        return;
    }

    let map = sim.map();
    let mut commands = uiworld.commands();
    let before = commands.as_ref().len();
    let mut picker = DeckPicker::new(&map);
    let projection = if drag_enabled {
        let camera = &uiworld.camera().camera;
        let mut project =
            |point| project_target(&map, &mut picker, screen_ray(camera, point)?, drag_filter());
        let current = project(input.screen);
        if pointer.interrupted || !selecting || current.is_none() {
            state.cancel(held);
        } else {
            state.update_drag(
                selecting,
                pressed,
                input.screen,
                &map,
                project,
                &mut commands,
            );
        }
        current
    } else {
        state.reset_gesture();
        let current = match input.ray {
            Some(ray) => project_target(&map, &mut picker, ray, ProjectFilter::ALL),
            None => input
                .unprojected
                .map(|pos| map.project(pos, 0.0, ProjectFilter::ALL)),
        };
        if pressed && !pointer.interrupted {
            if let Some(current) = current {
                queue_target(&map, current.kind, &mut commands);
            }
        }
        current
    };

    if commands.as_ref().len() != before {
        uiworld.write::<SpecialBuildingResource>().last_obb = None;
    }

    if let Some(project) = projection {
        let color = match project.kind {
            ProjectKind::Intersection(_) | ProjectKind::Road(_) | ProjectKind::Building(_) => {
                simulation::colors().gui_danger
            }
            _ => simulation::colors().gui_disabled,
        };
        uiworld
            .write::<ImmediateDraw>()
            .circle(project.pos.up(0.5), 2.0)
            .color(color);
    }
}

#[cfg(test)]
impl BulldozerState {
    pub(crate) fn start_test_gesture(&mut self) {
        self.gesture = Gesture::Sweeping {
            previous: Vec2::ZERO,
            removed: BTreeSet::new(),
        };
    }

    pub(crate) fn gesture_is_clear(&self) -> bool {
        matches!(self.gesture, Gesture::Idle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::{vec2, Vec2};
    use scenarios::{builders::grid_city, ScenarioCtx};
    use simulation::world_command::WorldCommand;
    use simulation::SimulationOptions;

    fn city() -> ScenarioCtx {
        let mut city = ScenarioCtx::new(SimulationOptions {
            terrain_size: 4,
            save_replay: false,
            ..SimulationOptions::LEGACY
        });
        grid_city(&mut city, vec2(700.0, 700.0), 4, 4, 160.0);
        city
    }

    // Orthographic fixture: one screen pixel per metre, with no camera or GPU dependency.
    fn sample(point: Vec2) -> Option<geom::Vec3> {
        Some(point.z(0.0))
    }

    fn sample_target(map: &Map) -> impl FnMut(Vec2) -> Option<MapProject> + '_ {
        |point| sample(point).map(|pos| map.project(pos, 0.0, drag_filter()))
    }

    #[test]
    fn p4d_sweep_finds_each_traversed_road_once() {
        let city = city();
        let map = city.sim.map();
        let start = vec2(680.0, 780.0);
        let end = vec2(1200.0, 780.0);
        let project = |point| sample(point).map(|pos| map.project(pos, 0.0, ProjectFilter::ROAD));
        let targets = sweep_targets(start, end, project);
        assert_eq!(targets.len(), 4);
        assert!(targets
            .iter()
            .all(|target| matches!(target, ProjectKind::Road(_))));
        assert_eq!(
            targets
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            4
        );
        assert_eq!(
            sweep_targets(start, end, |point| {
                (point.x < 900.0).then(|| project(point)).flatten()
            }),
            targets[..2]
        );
    }

    #[test]
    fn p4d_default_single_click_can_remove_an_intersection() {
        let city = city();
        crate::init::init();
        let ui = UiWorld::with_defaults();
        *ui.write::<Settings>() = Settings::default();
        *ui.write::<Tool>() = Tool::Bulldozer;
        let point = vec2(860.0, 860.0);
        let map = city.sim.map();
        let intersection = map.project(point.z(0.0), 0.0, ProjectFilter::INTER).kind;
        assert!(matches!(intersection, ProjectKind::Intersection(_)));
        {
            let mut input = ui.write::<InputMap>();
            input.screen = point;
            input.unprojected = sample(point);
            input.act.insert(InputAction::Select);
            input.just_act.insert(InputAction::Select);
        }
        bulldozer(&city.sim, &ui);
        ui.write::<InputMap>().just_act.clear();
        bulldozer(&city.sim, &ui);
        assert!(matches!(
            (intersection, ui.commands().as_ref()),
            (ProjectKind::Intersection(expected), [WorldCommand::MapRemoveIntersection(actual)])
                if expected == *actual
        ));
    }

    #[test]
    fn p4d_partial_sweep_keeps_junction_until_all_incident_roads_are_removed() {
        let mut city = city();
        let map = city.sim.map();
        let road_count = map.roads().len();
        let junction = map.project(
            sample(vec2(860.0, 860.0)).unwrap(),
            0.0,
            ProjectFilter::INTER,
        );
        let ProjectKind::Intersection(junction) = junction.kind else {
            panic!("expected a four-way intersection");
        };
        let incident = map.get(junction).unwrap().roads.clone();
        let mut state = BulldozerState::default();
        let mut commands = WorldCommands::default();
        let a = vec2(700.0, 780.0);
        let b = vec2(1180.0, 780.0);
        state.update_drag(true, true, a, &map, sample_target(&map), &mut commands);
        state.update_drag(true, false, b, &map, sample_target(&map), &mut commands);
        state.update_drag(true, false, a, &map, sample_target(&map), &mut commands);
        assert_eq!(commands.as_ref().len(), 4);
        assert!(commands
            .iter()
            .all(|cmd| matches!(cmd, WorldCommand::MapRemoveRoad(_))));
        drop(map);
        city.sim
            .apply_commands(std::mem::take(&mut commands).as_ref());
        let map = city.sim.map();
        assert_eq!(map.roads().len(), road_count - 4);
        let remaining = &map
            .get(junction)
            .expect("partial sweep removed the junction")
            .roads;
        assert_eq!(remaining.len(), incident.len() - 1);
        assert!(remaining.iter().all(|road| incident.contains(road)));

        // A second segment covers the other roads at a four-way junction. Applying the
        // batch lets map removal clean up that junction exactly when its last road goes.
        state.update_drag(
            true,
            false,
            vec2(860.0, 780.0),
            &map,
            sample_target(&map),
            &mut commands,
        );
        state.update_drag(
            true,
            false,
            vec2(860.0, 940.0),
            &map,
            sample_target(&map),
            &mut commands,
        );
        state.update_drag(
            true,
            false,
            vec2(780.0, 860.0),
            &map,
            sample_target(&map),
            &mut commands,
        );
        state.update_drag(
            true,
            false,
            vec2(940.0, 860.0),
            &map,
            sample_target(&map),
            &mut commands,
        );

        let removed_roads: BTreeSet<_> = commands
            .iter()
            .map(|command| match command {
                WorldCommand::MapRemoveRoad(road) => *road,
                command => panic!("unexpected drag command {command:?}"),
            })
            .collect();
        assert_eq!(removed_roads.len(), commands.as_ref().len());
        assert!(remaining.iter().all(|road| removed_roads.contains(road)));
        drop(map);
        city.sim.apply_commands(commands.as_ref());
        assert!(city.sim.map().get(junction).is_none());
        assert!(city.sim.map().roads().len() < road_count);
    }

    #[test]
    fn p4d_cancelled_drag_waits_for_release_and_a_fresh_press() {
        let city = city();
        let map = city.sim.map();
        let mut state = BulldozerState::default();
        let mut commands = WorldCommands::default();
        let a = vec2(700.0, 780.0);
        let b = vec2(1180.0, 780.0);
        state.update_drag(true, true, a, &map, sample_target(&map), &mut commands);
        state.update_drag(
            true,
            false,
            vec2(900.0, 780.0),
            &map,
            |_| None,
            &mut commands,
        );
        state.update_drag(true, true, b, &map, sample_target(&map), &mut commands);
        assert_eq!(commands.as_ref().len(), 1);
        state.update_drag(false, false, b, &map, sample_target(&map), &mut commands);
        state.update_drag(true, true, b, &map, sample_target(&map), &mut commands);
        assert_eq!(commands.as_ref().len(), 2);
    }

    #[test]
    fn p4d_paused_tool_queues_commands_and_requires_a_fresh_press_after_interruption() {
        let city = city();
        crate::init::init();
        let mut ui = UiWorld::with_defaults();
        {
            let mut settings = ui.write::<Settings>();
            settings.time_warp = 0;
            settings.bulldozer_drag = true;
        }
        *ui.write::<Tool>() = Tool::Bulldozer;
        let mut camera = Camera::new(vec2(960.0, 900.0).z(0.0), 1280.0, 720.0);
        camera.update();
        ui.insert(crate::rendering::OrbitCamera::from_camera(camera));
        let map = city.sim.map();
        let on_road = |point: Vec2| {
            let height = map.environment().height(point).unwrap();
            let (screen, _) = camera.project(point.z(height));
            let ray = screen_ray(&camera, screen).unwrap();
            let project =
                project_target(&map, &mut DeckPicker::new(&map), ray, drag_filter()).unwrap();
            let ProjectKind::Road(road) = project.kind else {
                panic!("expected a road at {point:?}");
            };
            (screen, road)
        };
        let (a, road_a) = on_road(vec2(700.0, 780.0));
        let (b, road_b) = on_road(vec2(1180.0, 780.0));
        drop(map);
        let update = |screen, selecting, pressed, frame: engine::PointerFrame| {
            frame.publish();
            {
                let mut input = ui.write::<InputMap>();
                input.screen = screen;
                input.act.clear();
                input.just_act.clear();
                if selecting {
                    input.act.insert(InputAction::Select);
                }
                if pressed {
                    input.just_act.insert(InputAction::Select);
                }
            }
            bulldozer(&city.sim, &ui);
        };
        let open = engine::PointerFrame::default;

        for (blocked, interrupted) in [(true, false), (false, true)] {
            update(a, true, true, open());
            assert!(
                matches!(ui.commands().as_ref(), [WorldCommand::MapRemoveRoad(id)] if *id == road_a)
            );
            assert!(city.sim.map().get(road_a).is_some());

            let mut frame = open();
            frame.blocked = blocked;
            frame.interrupted = interrupted;
            update(b, true, false, frame);
            update(b, true, false, open());
            assert_eq!(ui.commands().as_ref().len(), 1);

            update(b, false, false, open());
            update(b, true, true, open());
            assert!(
                matches!(ui.commands().as_ref(), [WorldCommand::MapRemoveRoad(a), WorldCommand::MapRemoveRoad(b)] if (*a, *b) == (road_a, road_b))
            );
            update(b, false, false, open());
            *ui.commands() = WorldCommands::default();
        }

        let mut select = crate::inputmap::InputCombination::default();
        select.push_unique(UnitInput::Mouse(MouseButton::Right));
        ui.write::<Bindings>().0.insert(
            InputAction::Select,
            crate::inputmap::InputCombinations(vec![select]),
        );
        let mut frame = open();
        frame.held_buttons.insert(MouseButton::Right);
        update(a, true, true, frame.clone());
        let mut secondary_released = frame.clone();
        secondary_released
            .released_buttons
            .insert(MouseButton::Left);
        update(b, true, false, secondary_released);
        assert!(ui.commands().iter().any(|command| {
            matches!(command, WorldCommand::MapRemoveRoad(id) if *id == road_b)
        }));

        update(b, false, false, open());
        *ui.commands() = WorldCommands::default();
        update(a, true, true, frame.clone());
        frame.released_buttons.insert(MouseButton::Right);
        update(b, true, false, frame);
        assert!(
            matches!(ui.commands().as_ref(), [WorldCommand::MapRemoveRoad(a), WorldCommand::MapRemoveRoad(b)] if (*a, *b) == (road_a, road_b))
        );

        update(b, false, false, open());
        *ui.commands() = WorldCommands::default();
        let mut left = crate::inputmap::InputCombination::default();
        left.push_unique(UnitInput::Mouse(MouseButton::Left));
        ui.write::<Bindings>()
            .0
            .get_mut(&InputAction::Select)
            .unwrap()
            .0
            .push(left);
        let mut pointer = engine::InputContext::default();
        pointer.observe_mouse_button(MouseButton::Left, true);
        update(a, true, true, pointer.pointer_frame(false));
        pointer.end_frame();
        pointer.observe_mouse_button(MouseButton::Left, false);
        pointer.observe_mouse_button(MouseButton::Right, true);
        update(b, true, false, pointer.pointer_frame(false));
        assert!(
            matches!(ui.commands().as_ref(), [WorldCommand::MapRemoveRoad(a), WorldCommand::MapRemoveRoad(b)] if (*a, *b) == (road_a, road_b))
        );

        // A Select button held from an earlier redraw cannot inherit the stroke.
        update(b, false, false, open());
        *ui.commands() = WorldCommands::default();
        let mut pointer = engine::InputContext::default();
        pointer.observe_mouse_button(MouseButton::Left, true);
        update(a, true, true, pointer.pointer_frame(false));
        pointer.end_frame();
        pointer.observe_mouse_button(MouseButton::Right, true);
        update(a, true, false, pointer.pointer_frame(false));
        pointer.end_frame();
        pointer.observe_mouse_button(MouseButton::Left, false);
        update(b, true, false, pointer.pointer_frame(false));
        assert!(
            matches!(ui.commands().as_ref(), [WorldCommand::MapRemoveRoad(id)] if *id == road_a)
        );

        // Right starts first even though the binding list also contains Left.
        update(b, false, false, open());
        *ui.commands() = WorldCommands::default();
        let press = |button| egui::Event::PointerButton {
            pos: egui::Pos2::ZERO,
            button,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        };
        ui.write::<BulldozerState>().record_press_order(&[
            press(egui::PointerButton::Secondary),
            press(egui::PointerButton::Primary),
        ]);
        let mut pointer = engine::InputContext::default();
        pointer.observe_mouse_button(MouseButton::Right, true);
        pointer.observe_mouse_button(MouseButton::Left, true);
        update(a, true, true, pointer.pointer_frame(false));
        ui.write::<BulldozerState>().record_press_order(&[]);
        pointer.end_frame();
        pointer.observe_mouse_button(MouseButton::Right, false);
        update(b, true, false, pointer.pointer_frame(false));
        assert!(
            matches!(ui.commands().as_ref(), [WorldCommand::MapRemoveRoad(id)] if *id == road_a)
        );

        update(b, false, false, open());
        *ui.commands() = WorldCommands::default();
        let mut pointer = engine::InputContext::default();
        ui.write::<BulldozerState>().record_press_order(&[
            press(egui::PointerButton::Primary),
            press(egui::PointerButton::Secondary),
        ]);
        pointer.observe_mouse_button(MouseButton::Left, true);
        pointer.observe_mouse_button(MouseButton::Right, true);
        update(a, true, true, pointer.pointer_frame(false));
        ui.write::<BulldozerState>().record_press_order(&[]);
        pointer.end_frame();
        pointer.observe_mouse_button(MouseButton::Left, false);
        update(b, true, false, pointer.pointer_frame(false));
        assert!(
            matches!(ui.commands().as_ref(), [WorldCommand::MapRemoveRoad(id)] if *id == road_a)
        );

        // A press over UI is cancelled, but a release and fresh press can
        // occur between two redraws before the tool sees Select inactive.
        update(b, false, false, open());
        *ui.commands() = WorldCommands::default();
        let mut pointer = engine::InputContext::default();
        pointer.observe_mouse_button(MouseButton::Left, true);
        update(a, false, false, pointer.pointer_frame(true));
        assert!(ui.commands().as_ref().is_empty());
        pointer.end_frame();
        pointer.observe_mouse_button(MouseButton::Left, false);
        pointer.observe_mouse_button(MouseButton::Left, true);
        update(b, true, true, pointer.pointer_frame(false));
        assert!(
            matches!(ui.commands().as_ref(), [WorldCommand::MapRemoveRoad(id)] if *id == road_b)
        );
        open().publish();
    }
}
