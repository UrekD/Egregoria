use std::collections::BTreeSet;

use common::FastSet;

use crate::gui::specialbuilding::SpecialBuildingResource;
use crate::gui::windows::settings::Settings;
use crate::gui::{overlays::land_tiles::show_edit_error, ErrorTooltip, Tool};
use crate::inputmap::{Bindings, DeckPicker, InputAction, InputMap, SurfaceHits, UnitInput};
use crate::rendering::immediate::ImmediateDraw;
use crate::uiworld::UiWorld;
use engine::MouseButton;
use geom::{Camera, LinearColor, Ray3, Vec2};
use prototypes::Money;
use simulation::map::{
    BuildingID, BuildingKind, Intersection, IntersectionID, Map, MapProject, ProjectFilter,
    ProjectKind, Road, RoadID, Turn, ROAD_Z_OFFSET,
};
use simulation::world_command::{WorldCommand, WorldCommands};
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
    let hits = SurfaceHits::cast(map.environment(), ray);
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
    Some(map.project(
        ground,
        0.0,
        filter - ProjectFilter::ROAD - ProjectFilter::INTER,
    ))
}

fn removal_candidate(map: &Map, target: ProjectKind) -> Option<WorldCommand> {
    Some(match target {
        ProjectKind::Intersection(id) => WorldCommand::MapRemoveIntersection(id),
        ProjectKind::Road(id) => WorldCommand::MapRemoveRoad(id),
        ProjectKind::Building(id)
            if map.get(id).is_some_and(|building| {
                !matches!(building.kind, BuildingKind::ExternalTrading)
            }) =>
        {
            WorldCommand::MapRemoveBuilding(id)
        }
        _ => return None,
    })
}

fn queue_target(map: &Map, target: ProjectKind, commands: &mut WorldCommands) {
    if let Some(command) = removal_candidate(map, target) {
        commands.push(command);
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
    let mut candidates = WorldCommands::default();
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
                &mut candidates,
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
                queue_target(&map, current.kind, &mut candidates);
            }
        }
        current
    };

    let mut preview = DemolitionPreview::default();
    let mut quote = |command: &WorldCommand| match command.preview(sim) {
        Ok(action) => {
            preview.add(&map, command, action.cost);
            true
        }
        Err(reason) => {
            show_edit_error(uiworld, command.kind(), reason);
            false
        }
    };
    let mut commands = uiworld.commands();
    let before = commands.as_ref().len();
    if candidates.as_ref().is_empty() {
        if let Some(command) = projection.and_then(|hit| removal_candidate(&map, hit.kind)) {
            quote(&command);
        }
    } else {
        commands.extend(candidates.iter().filter(|command| quote(command)).cloned());
    }
    if commands.as_ref().len() != before {
        uiworld.write::<SpecialBuildingResource>().last_obb = None;
    }

    let mut draw = uiworld.write::<ImmediateDraw>();
    if preview.is_empty() {
        if let Some(project) = projection {
            draw.circle(project.pos.up(0.5), 2.0)
                .color(simulation::colors().gui_disabled);
        }
    } else {
        let summary = preview.draw(sim, &map, &mut draw);
        let tooltip = &mut *uiworld.write::<ErrorTooltip>();
        tooltip.detail = Some(summary);
        tooltip.isworld = true;
    }
}

#[derive(Default)]
struct DemolitionPreview {
    roads: BTreeSet<RoadID>,
    junctions: BTreeSet<IntersectionID>,
    buildings: BTreeSet<BuildingID>,
    cost: Money,
    road_edits: Vec<ProjectKind>,
}

impl DemolitionPreview {
    fn add(&mut self, map: &Map, command: &WorldCommand, cost: Money) {
        let added = match *command {
            WorldCommand::MapRemoveRoad(id) => self.roads.insert(id),
            WorldCommand::MapRemoveIntersection(id) => {
                self.roads
                    .extend(map.intersections()[id].roads.iter().copied());
                self.junctions.insert(id)
            }
            WorldCommand::MapRemoveBuilding(id) => self.buildings.insert(id),
            _ => false,
        };
        if added {
            match *command {
                WorldCommand::MapRemoveRoad(road) => self.road_edits.push(ProjectKind::Road(road)),
                WorldCommand::MapRemoveIntersection(junction) => {
                    self.road_edits.push(ProjectKind::Intersection(junction));
                }
                _ => {}
            }
            self.cost = Money::new_inner(self.cost.inner().saturating_add(cost.inner()));
        }
    }

    fn is_empty(&self) -> bool {
        self.roads.is_empty() && self.junctions.is_empty() && self.buildings.is_empty()
    }

    fn draw(mut self, sim: &Simulation, map: &Map, draw: &mut ImmediateDraw) -> String {
        let red = LinearColor::from(simulation::colors().gui_danger);
        let mut disconnected = BTreeSet::new();
        for &id in &self.roads {
            let road = &map.roads()[id];
            draw.polyline(
                road.interfaced_points()
                    .iter()
                    .map(|point| point.up(0.34))
                    .collect::<Vec<_>>(),
                road.width,
                false,
            )
            .color(red.a(0.55));
            for endpoint in [road.src, road.dst] {
                if map.intersections()[endpoint]
                    .roads
                    .iter()
                    .all(|id| self.roads.contains(id))
                {
                    self.junctions.insert(endpoint);
                }
            }
            disconnected.extend(road.connected_buildings.iter().copied());
        }
        for &id in &self.junctions {
            draw_junction(map, &map.intersections()[id], draw, red);
        }
        let mut lots = 0;
        for id in self.roads.iter().flat_map(|&road| map.road_lots(road)) {
            let lot = &map.lots()[id];
            draw.polyline(
                lot.shape.corners.map(|p| p.z(lot.height + 0.42)).to_vec(),
                0.65,
                true,
            )
            .color(red);
            lots += 1;
        }
        let mut zones = 0;
        for &id in &self.buildings {
            let building = &map.buildings()[id];
            draw_building(map, id, draw, red);
            if let Some(zone) = &building.zone {
                let outline = zone
                    .poly
                    .iter()
                    .map(|p| {
                        let z = map.environment().height(*p).unwrap_or(building.height);
                        p.z(z + 0.5)
                    })
                    .collect::<Vec<_>>();
                draw.polyline(outline, 0.8, true).color(red);
                zones += 1;
            }
        }
        disconnected.retain(|id| !self.buildings.contains(id));
        for &id in &disconnected {
            draw_building(map, id, draw, LinearColor::new(1.0, 0.60, 0.25, 1.0));
        }

        let stops =
            simulation::map_dynamic::transit::stops_removed_by_demolition(sim, &self.road_edits);
        if let Ok(stops) = &stops {
            for &(_, pos) in stops {
                draw.stroke_circle(pos.up(0.7), 3.0, 1.0).color(red);
            }
        }

        let mut labels = Vec::new();
        for (count, singular, plural) in [
            (self.roads.len(), "road", "roads"),
            (self.junctions.len(), "junction", "junctions"),
            (self.buildings.len(), "building", "buildings"),
            (lots, "lot", "lots"),
            (zones, "building zone", "building zones"),
            (stops.as_ref().map_or(0, Vec::len), "bus stop", "bus stops"),
        ] {
            if count > 0 {
                labels.push(format!(
                    "{count} {}",
                    if count == 1 { singular } else { plural }
                ));
            }
        }
        let price = if self.cost < Money::ZERO {
            let refund = Money::new_inner(self.cost.inner().saturating_neg());
            format!("Refund {refund}")
        } else {
            format!("Cost {}", self.cost)
        };
        let mut summary = format!("Remove {}  ·  {price}", labels.join(", "));
        if !disconnected.is_empty() {
            let n = disconnected.len();
            let noun = if n == 1 { "building" } else { "buildings" };
            summary.push_str(&format!("\nDisconnect {n} {noun} from roads"));
        }
        if stops.is_err() {
            summary.push_str("\nBus stop effects unavailable");
        }
        summary
    }
}

fn draw_building(map: &Map, id: BuildingID, draw: &mut ImmediateDraw, color: LinearColor) {
    let building = &map.buildings()[id];
    let upper = building
        .mesh
        .faces
        .iter()
        .flat_map(|(face, _)| face)
        .map(|point| point.z)
        .fold(building.height + 3.0, f32::max);
    let corners = building.obb.corners;
    draw.polyline(
        corners.map(|point| point.z(upper + 0.45)).to_vec(),
        0.8,
        true,
    )
    .color(color);
    for point in corners {
        draw.line(point.z(building.height + 0.3), point.z(upper + 0.45), 0.55)
            .color(color);
    }
}

fn draw_junction(map: &Map, junction: &Intersection, draw: &mut ImmediateDraw, color: LinearColor) {
    let id = junction.id;
    let ring = junction.roundabout_geometry();
    let mouth = |road: &Road, side: f32| {
        let points = road.interfaced_points();
        let dir = if road.src == id {
            points.first_dir().map(|dir| dir.xy())
        } else {
            points.last_dir().map(|dir| -dir.xy())
        }
        .and_then(Vec2::try_normalize)?;
        let width = road.width * 0.5;
        Some((
            road.interface_point(id) + (dir.perpendicular() * width * side).z0(),
            dir,
        ))
    };
    let mut outline = Vec::new();
    for (&a, &b) in junction
        .roads
        .iter()
        .zip(junction.roads.iter().cycle().skip(1))
    {
        let (a, b) = (&map.roads()[a], &map.roads()[b]);
        if let Some(corner) = ring.and_then(|ring| ring.road_corner(id, a, b, true)) {
            outline.extend(corner.map(|point| point.up(0.55)));
        } else if let (Some((start, dir_a)), Some((end, dir_b))) = (mouth(a, -1.0), mouth(b, 1.0)) {
            outline.extend(
                Turn::spline(start.xy(), end.xy(), -dir_a, dir_b)
                    .into_smart_points(0.6, 0.0, 1.0)
                    .map(|point| point.z(junction.pos.z + ROAD_Z_OFFSET + 0.55)),
            );
        }
    }
    if outline.len() >= 3 {
        draw.polyline(outline, 1.0, true).color(color);
    } else {
        draw.stroke_circle(junction.pos.up(0.55), 5.0, 0.8)
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
    fn water_w4_bulldozer_does_not_restore_an_occluded_deck_from_ground_xy() {
        use geom::{vec3, Vec3};
        use simulation::map::LanePatternBuilder;

        let city = city();
        let center = vec2(1450.0, 1450.0);
        let mut map = city.sim.write::<Map>();
        let endpoint = |x| MapProject::ground(vec3(x, center.y, 25.0));
        let (_, road) = map
            .make_connection(
                endpoint(1340.0),
                endpoint(1560.0),
                None,
                &LanePatternBuilder::default().build(),
            )
            .unwrap();
        let ground = map.environment().true_height(center).unwrap();
        let from_above = Ray3::new(center.z(60.0), -Vec3::Z);
        let from_under = Ray3::new(center.z(ground + 2.0), -Vec3::Z);
        let mut picker = DeckPicker::new(&map);
        assert_eq!(
            project_target(&map, &mut picker, from_above, ProjectFilter::ROAD)
                .unwrap()
                .kind,
            ProjectKind::Road(road)
        );
        let terrain_over_deck = SurfaceHits {
            ground: Some((center.z(40.0), Vec3::Z)),
            water: None,
        };
        assert!(picker
            .pick(from_above, &terrain_over_deck, ProjectFilter::ROAD)
            .is_none());
        assert_eq!(
            project_target(&map, &mut picker, from_under, ProjectFilter::ROAD)
                .unwrap()
                .kind,
            ProjectKind::Ground
        );
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
    fn roadtools_bulldozer_hover_quotes_the_junction_cascade_without_removing_it() {
        let city = city();
        crate::init::init();
        let ui = UiWorld::with_defaults();
        *ui.write::<Settings>() = Settings::default();
        *ui.write::<Tool>() = Tool::Bulldozer;
        let point = vec2(860.0, 860.0);
        {
            let mut input = ui.write::<InputMap>();
            input.screen = point;
            input.unprojected = sample(point);
        }
        let map = city.sim.map();
        let count = map.roads().len();
        let ProjectKind::Intersection(id) =
            map.project(point.z(0.0), 0.0, ProjectFilter::INTER).kind
        else {
            panic!("fixture needs a junction");
        };
        let arms = &map.get(id).unwrap().roads;
        let roads = arms.len();
        let indexed_lots = arms.iter().flat_map(|&road| map.road_lots(road)).count();
        let affected_lots = map.lots().values().filter(|lot| arms.contains(&lot.parent)).count();
        assert_eq!(indexed_lots, affected_lots);
        assert!(map.lots().len() > affected_lots);
        drop(map);

        bulldozer(&city.sim, &ui);
        assert!(ui.commands().as_ref().is_empty());
        assert_eq!(city.sim.map().roads().len(), count);
        let tooltip = ui.read::<crate::gui::ErrorTooltip>();
        let message = tooltip
            .detail
            .as_deref()
            .expect("expected a demolition quote");
        assert!(message.contains(&format!("{roads} roads")), "{message}");
        if affected_lots > 0 {
            assert!(message.contains(&format!("{affected_lots} lot")), "{message}");
        }
        assert!(message.contains("Cost 0$"), "{message}");
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
    fn roadtools_bulldozer_previews_a_stop_that_loses_its_berth() {
        use simulation::map::LaneKind;
        use simulation::map_dynamic::transit::Transit;

        scenarios::ensure_repo_root_cwd();
        simulation::init::init();
        let mut sim = Simulation::new_authored_flat(1, 0).unwrap();
        let add_road = |sim: &mut Simulation, y| {
            let mut map = sim.write::<Map>();
            let (_, road) = map
                .make_connection(
                    MapProject::ground(geom::vec3(50.0, y, 0.0)),
                    MapProject::ground(geom::vec3(250.0, y, 0.0)),
                    None,
                    &simulation::map::LanePatternBuilder::new().build(),
                )
                .unwrap();
            let road = map.get(road).unwrap();
            let &(lane, _) = road
                .outgoing_lanes_from(road.src)
                .iter()
                .rfind(|(_, kind)| *kind == LaneKind::Driving)
                .unwrap();
            let points = &map.get(lane).unwrap().points;
            let (pos, dir) = points.point_dir_along(points.length() * 0.5);
            (
                road.id,
                road.points().point_along(road.length() * 0.5),
                pos,
                dir.xy(),
            )
        };
        let (road, cursor, pos, dir) = add_road(&mut sim, 100.0);
        let (_, _, other_pos, other_dir) = add_road(&mut sim, 150.0);
        sim.apply_commands([
            &WorldCommand::TransitAddStop { pos, dir },
            &WorldCommand::TransitAddStop {
                pos: other_pos,
                dir: other_dir,
            },
        ]);
        assert_eq!(sim.read::<Transit>().stops.len(), 2);

        crate::init::init();
        let ui = UiWorld::with_defaults();
        *ui.write::<Settings>() = Settings::default();
        *ui.write::<Tool>() = Tool::Bulldozer;
        {
            let mut input = ui.write::<InputMap>();
            input.screen = cursor.xy();
            input.unprojected = Some(cursor);
        }
        let assert_preview = |sim: &Simulation| {
            bulldozer(sim, &ui);
            let summary = ui
                .read::<crate::gui::ErrorTooltip>()
                .detail
                .clone()
                .unwrap();
            assert!(summary.contains("1 bus stop"), "{summary}");
            assert!(ui.commands().as_ref().is_empty());
            assert_eq!(sim.read::<Transit>().stops.len(), 2);
        };
        assert_preview(&sim);
        let snapshot = sim
            .encode_slot(simulation::SnapshotRevision::new(1))
            .unwrap()
            .into_snapshot_bytes();
        let mut sim = Simulation::load_compressed_snapshot(&snapshot, 64 * 1024 * 1024).unwrap();
        assert_preview(&sim);
        sim.apply_commands([&WorldCommand::MapRemoveRoad(road)]);
        let transit = sim.read::<Transit>();
        assert_eq!(transit.stops.len(), 1);
        assert_eq!(transit.stops.values().next().unwrap().pos, other_pos);
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
