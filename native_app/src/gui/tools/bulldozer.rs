use std::collections::BTreeSet;

use crate::gui::specialbuilding::SpecialBuildingResource;
use crate::gui::windows::settings::Settings;
use crate::gui::Tool;
use crate::inputmap::{InputAction, InputMap};
use crate::rendering::immediate::ImmediateDraw;
use crate::uiworld::UiWorld;
use geom::{Camera, Vec2, Vec3};
use simulation::map::{BuildingKind, Map, ProjectFilter, ProjectKind};
use simulation::world_command::WorldCommands;
use simulation::Simulation;

#[derive(Default)]
pub struct BulldozerState {
    gesture: Gesture,
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
    pub fn reset_gesture(&mut self) {
        self.gesture = Gesture::Idle;
    }

    fn cancel(&mut self, held: bool) {
        if held {
            self.gesture = Gesture::Cancelled;
        } else {
            self.reset_gesture();
        }
    }

    fn update_drag(
        &mut self,
        held: bool,
        pressed: bool,
        current: Vec2,
        map: &Map,
        mut project: impl FnMut(Vec2) -> Option<Vec3>,
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
        let targets = sweep_targets(*previous, current, map, drag_filter(), |point| {
            let position = project(point).filter(|pos| pos.is_finite());
            interrupted |= position.is_none();
            position
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
    map: &Map,
    filter: ProjectFilter,
    mut project: impl FnMut(Vec2) -> Option<Vec3>,
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
        let Some(position) = project(point).filter(|pos| pos.is_finite()) else {
            break;
        };
        let target = map.project(position, 0.0, filter).kind;
        if target != ProjectKind::Ground && seen.insert(target) {
            targets.push(target);
        }
    }
    targets
}

fn over_ui(point: Vec2) -> bool {
    goryak::pointer_over_ui(point.x, point.y) || engine::pointer_over_egui(point)
}

fn project_terrain(camera: &Camera, map: &Map, point: Vec2) -> Option<Vec3> {
    if !point.is_finite()
        || point.x < 0.0
        || point.y < 0.0
        || point.x >= camera.viewport_w
        || point.y >= camera.viewport_h
        || over_ui(point)
    {
        return None;
    }

    let ray = camera.unproj_ray(point)?;
    if !ray.dir.is_finite() {
        return None;
    }
    let (mut position, _) = map.environment.raycast(ray)?;
    position.z = position.z.max(0.0);
    Some(position)
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

pub fn bulldozer(sim: &Simulation, uiworld: &UiWorld) {
    profiling::scope!("gui::bulldozer");
    let input = uiworld.read::<InputMap>();
    let pointer = engine::pointer_frame();
    let selecting = input.act.contains(&InputAction::Select);
    let pressed = input.just_act.contains(&InputAction::Select);
    let held = selecting || pointer.mouse_held;
    let mut state = uiworld.write::<BulldozerState>();
    if *uiworld.read::<Tool>() != Tool::Bulldozer || pointer.blocked || over_ui(input.screen) {
        state.cancel(held);
        return;
    }

    let map = sim.map();
    let mut commands = uiworld.commands();
    let before = commands.as_ref().len();
    let drag_enabled = uiworld.read::<Settings>().bulldozer_drag;
    let projection = if drag_enabled {
        let camera = &uiworld.camera().camera;
        let project = |point| project_terrain(camera, &map, point);
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
        current.map(|pos| map.project(pos, 0.0, drag_filter()))
    } else {
        state.reset_gesture();
        let current = input
            .unprojected
            .map(|pos| map.project(pos, 0.0, ProjectFilter::ALL));
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

    #[test]
    fn p4d_sweep_finds_each_traversed_road_once() {
        let city = city();
        let map = city.sim.map();
        let start = vec2(680.0, 780.0);
        let end = vec2(1200.0, 780.0);
        let targets = sweep_targets(start, end, &map, ProjectFilter::ROAD, sample);
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
            sweep_targets(start, end, &map, ProjectFilter::ROAD, |point| {
                (point.x < 900.0).then_some(point.z(0.0))
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
    fn p4d_drag_queues_once_while_paused_and_keeps_partially_swept_junctions() {
        let mut city = city();
        crate::init::init();
        let ui = UiWorld::with_defaults();
        ui.write::<Settings>().time_warp = 0;
        let map = city.sim.map();
        let tick = city.sim.get_tick();
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
        let mut commands = ui.commands();
        let a = vec2(700.0, 780.0);
        let b = vec2(1180.0, 780.0);
        state.update_drag(true, true, a, &map, sample, &mut commands);
        state.update_drag(true, false, b, &map, sample, &mut commands);
        state.update_drag(true, false, a, &map, sample, &mut commands);
        assert_eq!(commands.as_ref().len(), 4);
        assert!(commands
            .iter()
            .all(|cmd| matches!(cmd, WorldCommand::MapRemoveRoad(_))));
        assert_eq!(city.sim.get_tick(), tick);
        assert_eq!(map.roads().len(), road_count);

        // A second segment covers the other roads at a four-way junction. Applying the
        // batch lets map removal clean up that junction exactly when its last road goes.
        state.update_drag(true, false, vec2(860.0, 780.0), &map, sample, &mut commands);
        state.update_drag(true, false, vec2(860.0, 940.0), &map, sample, &mut commands);
        state.update_drag(true, false, vec2(780.0, 860.0), &map, sample, &mut commands);
        state.update_drag(true, false, vec2(940.0, 860.0), &map, sample, &mut commands);

        let removed_roads: BTreeSet<_> = commands
            .iter()
            .map(|command| match command {
                WorldCommand::MapRemoveRoad(road) => *road,
                command => panic!("unexpected drag command {command:?}"),
            })
            .collect();
        assert_eq!(removed_roads.len(), commands.as_ref().len());
        assert!(incident.iter().all(|road| removed_roads.contains(road)));
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
        state.update_drag(true, true, a, &map, sample, &mut commands);
        state.update_drag(
            true,
            false,
            vec2(900.0, 780.0),
            &map,
            |_| None,
            &mut commands,
        );
        state.update_drag(true, true, b, &map, sample, &mut commands);
        assert_eq!(commands.as_ref().len(), 1);
        state.update_drag(false, false, b, &map, sample, &mut commands);
        state.update_drag(true, true, b, &map, sample, &mut commands);
        assert_eq!(commands.as_ref().len(), 2);
    }
}
