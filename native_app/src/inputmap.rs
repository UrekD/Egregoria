use common::{FastMap, FastSet};
use engine::{Context, InputContext, Key, MouseButton, Tesselator};
use geom::{Intersect, Ray3, Vec2, Vec3, AABB};
use serde::de::{value::StrDeserializer, IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use simulation::map::{Environment, Map, MapProject, ProjectFilter, ProjectKind, Road, WaterHit};
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashSet};
use std::fmt::{Debug, Display, Formatter};

// Either combinations can work
#[derive(Serialize, Deserialize)]
pub struct InputCombinations(pub Vec<InputCombination>);

#[derive(Clone, Hash, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum UnitInput {
    Key(Key),
    KeyScan(u32),
    Mouse(MouseButton),
    WheelUp,
    WheelDown,
}

#[derive(Clone, Hash, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum InputAction {
    GoLeft,
    GoRight,
    GoForward,
    GoBackward,
    CameraMove,
    CameraRotate,
    Zoom,
    Dezoom,
    Rotate,
    SizeUp,
    SizeDown,
    Close,
    Select,
    SecondarySelect,
    NoSnapping,
    HideInterface,
    UpElevation,
    DownElevation,
    OpenEconomyMenu,
    OpenDebugMenu,
    PausePlay,
    OpenChat,
    Speed1,
    Speed3,
    Speed8,
    Speed20,
    Speed50,
    SpeedMax,
    Cancel,
    Bulldozer,
}

// All unit inputs need to match
#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Eq)]
pub struct InputCombination(Vec<UnitInput>);

#[derive(Default)]
struct InputTree {
    actions: Vec<InputAction>,
    childs: FastMap<UnitInput, Box<InputTree>>,
}

/// Screen distance a pointer may travel and still count as a click rather than a drag.
const CLICK_DRAG_THRESHOLD: f32 = 5.0;

#[derive(Default)]
pub struct InputMap {
    /// Actions that were just pressed this frame
    pub just_act: FastSet<InputAction>,
    /// Actions that are currently pressed
    pub act: FastSet<InputAction>,
    /// Mouse wheel delta
    pub wheel: f32,
    /// Mouse position in world space on the terrain (max height 0)
    pub unprojected: Option<Vec3>,

    pub unprojected_normal: Option<Vec3>,
    /// Ray from camera to mouse
    pub ray: Option<Ray3>,
    /// Mouse position in screen space
    pub screen: Vec2,
    input_tree: InputTree,
    require_pointer_release: bool,
    close_down: bool,
    cancel_held: bool,
    /// Where the Cancel press started, until the pointer moves far enough to make it a drag.
    cancel_origin: Option<Vec2>,
}

/// Separate signed terrain and half-open water intersections for the same ray.
#[derive(Default)]
pub(crate) struct SurfaceHits {
    pub ground: Option<(Vec3, Vec3)>,
    pub water: Option<WaterHit>,
}

impl SurfaceHits {
    pub(crate) fn cast(env: &Environment, ray: Ray3) -> Self {
        if !ray.from.is_finite() || !ray.dir.is_finite() || ray.dir.mag2() == 0.0 {
            return Self::default();
        }
        let ground = env.raycast(ray);
        let limit = ground.map_or(f32::MAX, |(point, _)| ray_parameter(ray, point));
        let water = if env.has_water_configuration() {
            env.raycast_water(ray, limit)
        } else {
            None
        };
        Self { ground, water }
    }

    pub(crate) fn nearest(&self) -> Option<Vec3> {
        self.water
            .map(|hit| hit.point)
            .or(self.ground.map(|(point, _)| point))
    }
}

pub(crate) fn ray_parameter(ray: Ray3, point: Vec3) -> f32 {
    (point - ray.from).dot(ray.dir) / ray.dir.mag2()
}

/// Shared depth-ranked deck and junction picking. Reuse one mesh scratch per sweep.
pub(crate) struct DeckPicker<'a> {
    map: &'a Map,
    padding: f32,
    vertices: Vec<engine::MeshVertex>,
    indices: Vec<u32>,
}

impl<'a> DeckPicker<'a> {
    pub(crate) fn new(map: &'a Map) -> Self {
        // The tessellator caps a miter at ten times the half-width.
        let padding = 5.0
            * map
                .roads()
                .values()
                .map(|road| road.width)
                .fold(0.0, f32::max);
        Self {
            map,
            padding,
            vertices: Vec::new(),
            indices: Vec::new(),
        }
    }

    pub(crate) fn pick(
        &mut self,
        ray: Ray3,
        surfaces: &SurfaceHits,
        filter: ProjectFilter,
    ) -> Option<MapProject> {
        let limit = surfaces
            .nearest()
            .map_or(f32::INFINITY, |p| ray_parameter(ray, p));
        let world = self.map.environment().bounds().expand(self.padding);
        let ray_bounds = ray_xy_bounds(ray, world, limit)?;
        let search = ray_bounds.expand(self.padding);
        let map = self.map;
        let mut mesh = engine::Tesselator::new(&mut self.vertices, &mut self.indices, None, 1.0);
        map.spatial_map()
            .query(search, filter)
            .filter_map(|kind| {
                let depth = match kind {
                    ProjectKind::Road(id) => {
                        let road = map.get(id)?;
                        if !road
                            .interfaced_points()
                            .bbox()
                            .flatten()
                            .expand(5.0 * road.width)
                            .intersects(&ray_bounds)
                        {
                            return None;
                        }
                        road_hit(road, ray, limit, &mut mesh)?
                    }
                    ProjectKind::Intersection(id) => {
                        mesh.vertices.clear();
                        mesh.indices.clear();
                        crate::rendering::intersection_mesh(
                            &mut mesh,
                            &geom::LinearColor::WHITE,
                            map.get(id)?,
                            map.roads(),
                        );
                        mesh.indices
                            .chunks_exact(3)
                            .filter_map(|indices| {
                                let point =
                                    |i: usize| mesh.vertices[indices[i] as usize].position.into();
                                triangle_hit(
                                    ray,
                                    point(0),
                                    point(1),
                                    point(2),
                                )
                                .filter(|depth| (0.0..=limit).contains(depth))
                            })
                            .min_by(f32::total_cmp)?
                    }
                    _ => return None,
                };
                Some((depth, kind))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)))
            .map(|(depth, kind)| MapProject {
                kind,
                pos: ray.from + ray.dir * depth,
            })
    }
}

fn road_hit(
    road: &Road,
    ray: Ray3,
    limit: f32,
    mesh: &mut Tesselator<'_>,
) -> Option<f32> {
    let points = road.interfaced_points();
    let first = points.first_dir()?.xy();
    let last = points.last_dir()?.xy();
    // Use the renderer's tessellator so the triangles follow the deck's mitered joins and grade.
    mesh.vertices.clear();
    mesh.indices.clear();
    mesh.draw_polyline_with_dir(points.as_slice(), first, last, road.width);

    mesh.indices
        .chunks_exact(3)
        .filter_map(|indices| {
            let point = |i: usize| Vec3::from(mesh.vertices[indices[i] as usize].position);
            triangle_hit(ray, point(0), point(1), point(2))
                .filter(|depth| (0.0..=limit).contains(depth))
        })
        .min_by(f32::total_cmp)
}

fn triangle_hit(ray: Ray3, a: Vec3, b: Vec3, c: Vec3) -> Option<f32> {
    let ab = b - a;
    let ac = c - a;
    let cross = ray.dir.cross(ac);
    let det = ab.dot(cross);
    if det.abs() <= f32::EPSILON {
        return None;
    }
    let origin = ray.from - a;
    let u = origin.dot(cross) / det;
    let side = origin.cross(ab);
    let v = ray.dir.dot(side) / det;
    (u >= 0.0 && v >= 0.0 && u + v <= 1.0).then(|| ac.dot(side) / det)
}

fn ray_xy_bounds(ray: Ray3, world: AABB, limit: f32) -> Option<AABB> {
    if !ray.from.is_finite() || !ray.dir.is_finite() || ray.dir.mag2() == 0.0 {
        return None;
    }
    let mut near = 0.0_f32;
    let mut far = limit;
    for (origin, direction, low, high) in [
        (ray.from.x, ray.dir.x, world.ll.x, world.ur.x),
        (ray.from.y, ray.dir.y, world.ll.y, world.ur.y),
    ] {
        if direction == 0.0 {
            if !(low..=high).contains(&origin) {
                return None;
            }
            continue;
        }
        let a = (low - origin) / direction;
        let b = (high - origin) / direction;
        near = near.max(a.min(b));
        far = far.min(a.max(b));
    }
    if near > far {
        return None;
    }
    let start = ray.from.xy() + ray.dir.xy() * near;
    let end = if far.is_finite() {
        ray.from.xy() + ray.dir.xy() * far
    } else {
        start
    };
    Some(AABB::new_ll_ur(start.min(end), start.max(end)))
}

#[derive(Serialize)]
pub struct Bindings(pub BTreeMap<InputAction, InputCombinations>);

impl<'de> Deserialize<'de> for Bindings {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if !deserializer.is_human_readable() {
            return BTreeMap::deserialize(deserializer).map(Self);
        }

        struct BindingsVisitor;

        impl<'de> Visitor<'de> for BindingsVisitor {
            type Value = Bindings;

            fn expecting(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                f.write_str("a map of input actions to key bindings")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut entries: A) -> Result<Bindings, A::Error> {
                let mut bindings = BTreeMap::new();
                while let Some(name) = entries.next_key::<String>()? {
                    match InputAction::deserialize(StrDeserializer::<A::Error>::new(&name)) {
                        Ok(action) => {
                            bindings.insert(action, entries.next_value()?);
                        }
                        Err(_) => {
                            entries.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(Bindings(bindings))
            }
        }

        deserializer.deserialize_map(BindingsVisitor)
    }
}

use InputAction::*;
use Key as K;
use MouseButton::*;
use UnitInput::*;

#[cfg(target_os = "macos")]
const TOP_ROW_DIGITS: [u32; 6] = [0x12, 0x13, 0x14, 0x15, 0x17, 0x16];
#[cfg(not(target_os = "macos"))]
const TOP_ROW_DIGITS: [u32; 6] = [2, 3, 4, 5, 6, 7];
const SPEED_DEFAULTS: [(InputAction, u32, &str); 6] = [
    (Speed1, TOP_ROW_DIGITS[0], "1"),
    (Speed3, TOP_ROW_DIGITS[1], "2"),
    (Speed8, TOP_ROW_DIGITS[2], "3"),
    (Speed20, TOP_ROW_DIGITS[3], "4"),
    (Speed50, TOP_ROW_DIGITS[4], "5"),
    (SpeedMax, TOP_ROW_DIGITS[5], "6"),
];

// https://stackoverflow.com/a/38068969/5000800 for key scans
#[rustfmt::skip]
const DEFAULT_BINDINGS: &[(InputAction, &[&[UnitInput]])] = &[
    (GoForward,       &[&[KeyScan(17)], &[Key(K::ArrowUp)]]),
    (GoBackward,      &[&[KeyScan(31)], &[Key(K::ArrowDown)]]),
    (GoLeft,          &[&[KeyScan(30)], &[Key(K::ArrowLeft)]]),
    (GoRight,         &[&[KeyScan(32)], &[Key(K::ArrowRight)]]),
    (CameraRotate,    &[&[Mouse(Right)]]),
    (CameraMove,      &[&[Key(K::Shift), Mouse(Right)], &[Mouse(Middle)]]),
    (Zoom,            &[&[Key(K::c("+"))], &[WheelUp]]),
    (Dezoom,          &[&[Key(K::c("-"))], &[WheelDown]]),
    (Rotate,          &[&[Key(K::Control), WheelUp], &[Key(K::Control), WheelDown]]),
    (SizeUp,          &[&[Key(K::Control), WheelUp]]),
    (SizeDown,        &[&[Key(K::Control), WheelDown]]),
    (Close,           &[&[Key(K::Escape)]]),
    (Select,          &[&[Mouse(Left)]]),
    (SecondarySelect, &[&[Key(K::Control), Mouse(Left)]]),
    (NoSnapping,      &[&[Key(K::Control)]]),
    (HideInterface,   &[&[Key(K::c("H"))]]),
    (UpElevation,     &[&[Key(K::Control), WheelUp]]),
    (DownElevation,   &[&[Key(K::Control), WheelDown]]),
    (OpenEconomyMenu, &[&[Key(K::c("E"))]]),
    (OpenDebugMenu,   &[&[Key(K::F3)]]),
    (PausePlay,       &[&[Key(K::Space)]]),
    (OpenChat,        &[&[Key(K::c("T"))]]),
    (Cancel,          &[&[Mouse(Right)]]),
    (Bulldozer,       &[&[Key(K::c("B"))]]),
];

impl Default for Bindings {
    fn default() -> Self {
        let mut m = BTreeMap::default();

        for (k, v) in DEFAULT_BINDINGS {
            if m.insert(
                k.clone(),
                InputCombinations(v.iter().map(|&x| InputCombination(x.to_vec())).collect()),
            )
            .is_some()
            {
                log::error!("inserting same action twice!");
            }
        }

        m.extend(SPEED_DEFAULTS.iter().map(|(action, scan, _)| {
            (
                action.clone(),
                InputCombinations(vec![InputCombination(vec![KeyScan(*scan)])]),
            )
        }));
        Bindings(m)
    }
}

impl Bindings {
    pub(crate) fn fill_missing_defaults(&mut self) {
        let defaults = Self::default();
        self.0.retain(|act, _| defaults.0.contains_key(act));
        for (action, mut combinations) in defaults.0 {
            if self.0.contains_key(&action) {
                continue;
            }
            // Defaults added after bindings.json existed never take an input the player uses.
            if added_later(&action) {
                combinations
                    .0
                    .retain(|combination| !self.uses_input_of(&action, combination));
            }
            self.0.insert(action, combinations);
        }
    }

    fn uses_input_of(&self, action: &InputAction, default: &InputCombination) -> bool {
        let primary: Vec<_> = default
            .0
            .iter()
            .filter(|unit| !matches!(unit, Key(k) if k.is_modifier()))
            .map(same_key)
            .collect();
        self.0
            .iter()
            .filter(|(other, _)| !shares_inputs(action, other))
            .flat_map(|(_, combinations)| &combinations.0)
            .flat_map(|combination| &combination.0)
            .any(|unit| primary.contains(&same_key(unit)))
    }
}

fn added_later(action: &InputAction) -> bool {
    matches!(action, Cancel | Bulldozer)
        || SPEED_DEFAULTS.iter().any(|(speed, _, _)| speed == action)
}

/// Right click cancels, while right drag moves the camera.
fn shares_inputs(action: &InputAction, other: &InputAction) -> bool {
    *action == Cancel && matches!(other, CameraRotate | CameraMove)
}

/// Top-row digits match whether they were bound by character or by scan code.
fn same_key(unit: &UnitInput) -> UnitInput {
    match unit {
        Key(Key::Char(c)) => SPEED_DEFAULTS
            .iter()
            .find(|(_, _, digit)| c == digit)
            .map_or_else(|| unit.clone(), |(_, scan, _)| KeyScan(*scan)),
        _ => unit.clone(),
    }
}

impl InputMap {
    pub(crate) fn surface_hits(&self, env: &Environment) -> SurfaceHits {
        self.ray
            .map_or_else(SurfaceHits::default, |ray| SurfaceHits::cast(env, ray))
    }

    pub fn build_input_tree(&mut self, bindings: &mut Bindings) {
        for v in &mut bindings.0.values_mut() {
            for x in &mut v.0 {
                x.0.sort()
            }
        }
        self.input_tree = InputTree::new(bindings);
    }

    pub(crate) fn reset_world_input(&mut self) {
        self.just_act.clear();
        self.act.clear();
        self.wheel = 0.0;
        self.screen = Vec2::ZERO;
        self.ray = None;
        self.unprojected = None;
        self.unprojected_normal = None;
        self.cancel_held = false;
        self.cancel_origin = None;
        self.require_fresh_pointer_press();
    }

    /// While a keybind is being captured, the press being bound must not also act as a
    /// shortcut, neither now nor when it is released after capture ends.
    pub(crate) fn suppress_shortcuts(&mut self) {
        self.just_act.clear();
        self.wheel = 0.0;
        self.cancel_origin = None;
    }

    pub(crate) fn require_fresh_pointer_press(&mut self) {
        self.require_pointer_release = true;
    }

    pub(crate) fn observe_pointer_release(&mut self, ctx: &mut Context, scripted: bool) {
        if !self.require_pointer_release {
            return;
        }
        if scripted {
            self.observe_pointer_buttons(!ctx.input.mouse.pressed.is_empty());
            return;
        }

        // Egui receives window events before Yakui can consume them.
        let platform = &ctx.egui.platform;
        let raw = platform.egui_input();
        let held = raw.focused
            && platform.egui_ctx().input(|input| {
                use egui::PointerButton::*;
                [Primary, Secondary, Middle, Extra1, Extra2]
                    .into_iter()
                    .any(|button| {
                        raw.events
                            .iter()
                            .rev()
                            .find_map(|event| match event {
                                egui::Event::PointerButton {
                                    button: event_button,
                                    pressed,
                                    ..
                                } if *event_button == button => Some(*pressed),
                                _ => None,
                            })
                            .unwrap_or_else(|| input.pointer.button_down(button))
                    })
            });
        if !held {
            ctx.input.mouse.pressed.clear();
        }
        self.observe_pointer_buttons(held);
    }

    pub(crate) fn observe_pointer_buttons(&mut self, held: bool) {
        self.require_pointer_release &= held;
    }

    fn actions(
        &self,
        input: &InputContext,
        kb: bool,
        mouse: bool,
    ) -> impl Iterator<Item = InputAction> + '_ {
        let empty1 = FastSet::default();
        let empty2 = FastSet::default();
        let empty3 = FastSet::default();
        self.input_tree.query(
            if kb { &input.keyboard.pressed } else { &empty1 },
            if kb {
                &input.keyboard.pressed_scancode
            } else {
                &empty2
            },
            if mouse { &input.mouse.pressed } else { &empty3 },
            if mouse { input.mouse.wheel_delta } else { 0.0 },
        )
    }

    /// Track the binding across world-input resets and menu transitions.
    pub(crate) fn close_pressed(&mut self, input: &InputContext, kb: bool, mouse: bool) -> bool {
        let inputs_held = !input.keyboard.pressed.is_empty()
            || !input.keyboard.pressed_scancode.is_empty()
            || !input.mouse.pressed.is_empty()
            || input.mouse.wheel_delta != 0.0;
        let down = inputs_held
            && self
                .actions(input, true, true)
                .any(|action| action == Close);
        let was_down = std::mem::replace(&mut self.close_down, down);
        down && !was_down
            && ((kb && mouse) || self.actions(input, kb, mouse).any(|action| action == Close))
    }

    pub fn prepare_frame(&mut self, input: &InputContext, kb: bool, mouse: bool) {
        let mouse = mouse && !self.require_pointer_release;
        self.just_act.clear();
        let mut acts: FastSet<_> = self
            .actions(input, kb, mouse)
            .filter(|action| *action != Close)
            .collect();
        let cancel_held = acts.remove(&Cancel);
        std::mem::swap(&mut self.act, &mut acts);
        for v in &self.act {
            if !acts.contains(v) {
                self.just_act.insert(v.clone());
            }
        }
        if self.cancel_clicked(cancel_held, input.mouse.screen, mouse) {
            self.just_act.insert(Cancel);
        }
        self.screen = input.mouse.screen;
        self.wheel = if self.require_pointer_release {
            0.0
        } else {
            input.mouse.wheel_delta
        };
    }
}

impl InputMap {
    /// Cancel acts on release, and only if the pointer stayed put and over the world.
    /// `mouse` only knows about egui; Yakui lets releases through, so its panels are
    /// checked here.
    fn cancel_clicked(&mut self, held: bool, screen: Vec2, mouse: bool) -> bool {
        let was_held = std::mem::replace(&mut self.cancel_held, held);
        if held && !was_held {
            self.cancel_origin = Some(screen);
        }
        if self
            .cancel_origin
            .is_some_and(|origin| origin.distance(screen) > CLICK_DRAG_THRESHOLD)
        {
            self.cancel_origin = None;
        }
        if held {
            return false;
        }
        self.cancel_origin.take().is_some()
            && was_held
            && mouse
            && !goryak::pointer_over_ui(screen.x, screen.y)
    }
}

impl InputCombination {
    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// A valid combination is a list of modifiers and a single key/mouse button
    pub fn is_valid(&self) -> bool {
        let mut has_primary = false;
        let mut has_more_than_one_primary = false;

        for x in &self.0 {
            match x {
                // ignore modifiers
                Key(k) if k.is_modifier() => {}
                Key(_) | Mouse(_) | KeyScan(_) | WheelDown | WheelUp => {
                    if has_primary {
                        has_more_than_one_primary = true;
                    }
                    has_primary = true;
                }
            }
        }

        has_primary && !has_more_than_one_primary
    }

    /// Returns true if the combination is only modifiers
    pub fn is_modifiers_only(&self) -> bool {
        self.0.iter().all(|x| match x {
            Key(k) => k.is_modifier(),
            _ => false,
        })
    }

    /// Pushes a unit if not already present
    pub fn push_unique(&mut self, unit: UnitInput) {
        if self.0.contains(&unit) {
            return;
        }
        self.0.push(unit);
    }

    pub fn sort(&mut self) {
        // put modifiers in front
        self.0.sort();
    }
}

impl Display for InputCombinations {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        for (i, x) in self.0.iter().enumerate() {
            Display::fmt(x, f)?;
            if i < self.0.len() - 1 {
                f.write_str(", ")?;
            }
        }
        Ok(())
    }
}

impl Display for InputCombination {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        for (i, x) in self.0.iter().enumerate() {
            Display::fmt(x, f)?;
            if i < self.0.len() - 1 {
                f.write_str(" + ")?;
            }
        }
        Ok(())
    }
}

impl Display for UnitInput {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            UnitInput::Key(code) => match code {
                Key::Char(s) => Display::fmt(s, f),
                _ => Debug::fmt(code, f),
            },
            Mouse(mb) => Debug::fmt(mb, f),
            WheelUp => {
                write!(f, "Scroll Up")
            }
            WheelDown => {
                write!(f, "Scroll Down")
            }
            KeyScan(scan) => {
                if let Some((_, _, digit)) = SPEED_DEFAULTS.iter().find(|(_, code, _)| code == scan)
                {
                    f.write_str(digit)
                } else {
                    write!(f, "ScanCode({scan})")
                }
            }
        }
    }
}

impl Debug for UnitInput {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Display::fmt(self, f)
    }
}

impl Debug for InputCombinations {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Display::fmt(self, f)
    }
}

impl Debug for InputCombination {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Display::fmt(self, f)
    }
}

impl Debug for InputAction {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Display::fmt(self, f)
    }
}

impl InputTree {
    pub fn new(bindings: &Bindings) -> Self {
        let mut root = Self {
            actions: Vec::new(),
            childs: Default::default(),
        };

        for (act, combs) in &bindings.0 {
            for comb in &combs.0 {
                let mut cur: &mut InputTree = &mut root;
                for inp in &comb.0 {
                    let ent = cur.childs.entry(inp.clone());
                    match ent {
                        Entry::Occupied(_) => {}
                        Entry::Vacant(v) => {
                            v.insert(Box::new(InputTree {
                                actions: Vec::new(),
                                childs: Default::default(),
                            }));
                        }
                    }
                    cur = &mut **cur.childs.get_mut(inp).unwrap();
                }
                cur.actions.push(act.clone());
            }
        }

        root
    }

    pub fn query(
        &self,
        kb: &FastSet<Key>,
        kb_scans: &FastSet<u32>,
        mouse: &FastSet<MouseButton>,
        wheel: f32,
    ) -> impl Iterator<Item = InputAction> + '_ {
        let mut units: HashSet<UnitInput> = HashSet::with_capacity(kb.len() + mouse.len() + 1);

        units.extend(kb.iter().map(|x| UnitInput::Key(x.clone())));
        units.extend(mouse.iter().map(|x| Mouse(*x)));
        units.extend(kb_scans.iter().map(|x| KeyScan(*x)));
        if wheel > 0.0 {
            units.insert(WheelUp);
        }
        if wheel < 0.0 {
            units.insert(WheelDown);
        }

        let mut matches = vec![];
        let mut queue = vec![(vec![], self)];

        while !queue.is_empty() {
            for (input_stack, q) in std::mem::take(&mut queue) {
                for key in units.iter().cloned() {
                    if let Some(inp) = q.childs.get(&key) {
                        let mut newstack = input_stack.clone();
                        newstack.push(key);
                        if !inp.actions.is_empty() {
                            matches.push((newstack.clone(), &inp.actions));
                        }
                        queue.push((newstack, &**inp));
                    }
                }
            }
        }

        matches
            .into_iter()
            .rev()
            .filter_map(move |(inp, act)| {
                if !inp.iter().all(|x| units.contains(x)) {
                    return None;
                }
                for v in inp {
                    units.remove(&v);
                }
                Some(act)
            })
            .flatten()
            .cloned()
    }
}

impl Display for InputAction {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                GoLeft => "Go Left",
                GoRight => "Go Right",
                GoForward => "Go Forward",
                GoBackward => "Go Backward",
                CameraMove => "Camera Move",
                CameraRotate => "Camera Rotate",
                Zoom => "Zoom",
                Dezoom => "Dezoom",
                Rotate => "Rotate",
                Close => "Close",
                Select => "Select",
                SecondarySelect => "Secondary Select",
                HideInterface => "Hide interface",
                NoSnapping => "No Snapping",
                UpElevation => "Up Elevation",
                DownElevation => "Down Elevation",
                OpenEconomyMenu => "Economy Menu",
                PausePlay => "Pause/Play",
                Speed1 => "Simulation Speed 1×",
                Speed3 => "Simulation Speed 3×",
                Speed8 => "Simulation Speed 8×",
                Speed20 => "Simulation Speed 20×",
                Speed50 => "Simulation Speed 50×",
                SpeedMax => "Simulation Speed MAX",
                Cancel => "Cancel tool / Deselect (click)",
                Bulldozer => "Bulldozer",
                OpenChat => "Interact with Chat",
                SizeUp => "Size Up",
                SizeDown => "Size Down",
                OpenDebugMenu => "Debug Menu",
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qf1_missing_speed_defaults_respect_saved_bindings() {
        let mut bindings = saved_bindings();
        bindings.fill_missing_defaults();
        assert!(bindings.0[&Speed1].0.is_empty());
        assert!(bindings.0[&Speed3].0.is_empty());
        assert!(bindings.0[&Speed8].0.is_empty());
        assert!(bindings.0[&Speed50].0.is_empty());

        let mut input_map = InputMap::default();
        input_map.build_input_tree(&mut bindings);
        let mut input = InputContext::default();
        input.keyboard.pressed.insert(K::c("1"));
        input.keyboard.pressed_scancode.insert(TOP_ROW_DIGITS[0]);
        input_map.prepare_frame(&input, true, true);
        assert!(input_map.just_act.contains(&GoRight));
        assert!(!input_map.just_act.contains(&Speed1));

        input.keyboard.pressed.clear();
        input.keyboard.pressed_scancode.clear();
        input.keyboard.pressed_scancode.insert(TOP_ROW_DIGITS[3]);
        input_map.prepare_frame(&input, true, true);
        assert!(input_map.just_act.contains(&Speed20));
        input_map.prepare_frame(&input, false, true);
        assert!(!input_map.act.contains(&Speed20));
    }

    fn right_button(input_map: &mut InputMap, held: bool, screen: Vec2) {
        let mut input = InputContext::default();
        if held {
            input.mouse.pressed.insert(Right);
        }
        input.mouse.screen = screen;
        input_map.prepare_frame(&input, true, true);
    }

    #[test]
    fn ux2_right_click_cancels_on_release_but_right_drag_does_not() {
        let mut bindings = Bindings::default();
        let mut input_map = InputMap::default();
        input_map.build_input_tree(&mut bindings);

        right_button(&mut input_map, true, Vec2::new(100.0, 100.0));
        assert!(input_map.act.contains(&CameraRotate));
        assert!(!input_map.just_act.contains(&Cancel), "cancelled on press");
        right_button(&mut input_map, true, Vec2::new(102.0, 101.0));
        right_button(&mut input_map, false, Vec2::new(102.0, 101.0));
        assert!(input_map.just_act.contains(&Cancel), "click did not cancel");
        right_button(&mut input_map, false, Vec2::new(102.0, 101.0));
        assert!(!input_map.just_act.contains(&Cancel), "cancel repeated");

        // Dragging past the threshold rotates the camera, even if it returns to the origin.
        right_button(&mut input_map, true, Vec2::new(100.0, 100.0));
        right_button(
            &mut input_map,
            true,
            Vec2::new(100.0 + CLICK_DRAG_THRESHOLD + 1.0, 100.0),
        );
        right_button(&mut input_map, true, Vec2::new(100.0, 100.0));
        right_button(&mut input_map, false, Vec2::new(100.0, 100.0));
        assert!(!input_map.just_act.contains(&Cancel), "drag cancelled");

        // Shift + right drag moves the camera instead.
        let mut input = InputContext::default();
        input.keyboard.pressed.insert(K::Shift);
        input.mouse.pressed.insert(Right);
        input_map.prepare_frame(&input, true, true);
        assert!(input_map.act.contains(&CameraMove));
        input.mouse.pressed.clear();
        input_map.prepare_frame(&input, true, true);
        assert!(
            !input_map.just_act.contains(&Cancel),
            "shift click cancelled"
        );
    }

    #[test]
    fn ux2_right_click_released_over_the_ui_does_not_cancel() {
        let mut bindings = Bindings::default();
        let mut input_map = InputMap::default();
        input_map.build_input_tree(&mut bindings);
        right_button(&mut input_map, true, Vec2::ZERO);
        let mut input = InputContext::default();
        input.mouse.pressed.insert(Right);
        input_map.prepare_frame(&input, true, false);
        input.mouse.pressed.clear();
        input_map.prepare_frame(&input, true, false);
        right_button(&mut input_map, false, Vec2::ZERO);
        assert!(!input_map.just_act.contains(&Cancel));

        right_button(&mut input_map, true, Vec2::ZERO);
        input_map.reset_world_input();
        input_map.observe_pointer_buttons(false);
        right_button(&mut input_map, false, Vec2::ZERO);
        assert!(
            !input_map.just_act.contains(&Cancel),
            "cancel survived reset"
        );
    }

    #[test]
    fn uxfix_keybind_capture_swallows_b_and_right_click() {
        let mut bindings = Bindings::default();
        let mut input_map = InputMap::default();
        input_map.build_input_tree(&mut bindings);
        let mut b = InputContext::default();
        b.keyboard.pressed.insert(K::c("B"));

        // The press being bound is no shortcut, also once capture ends with it held.
        input_map.prepare_frame(&b, true, true);
        input_map.suppress_shortcuts();
        assert!(!input_map.just_act.contains(&Bulldozer));
        input_map.prepare_frame(&b, true, true);
        assert!(!input_map.just_act.contains(&Bulldozer));
        input_map.prepare_frame(&InputContext::default(), true, true);

        // A right click bound on press does not cancel on its release.
        right_button(&mut input_map, true, Vec2::ZERO);
        input_map.suppress_shortcuts();
        right_button(&mut input_map, true, Vec2::ZERO);
        right_button(&mut input_map, false, Vec2::ZERO);
        assert!(!input_map.just_act.contains(&Cancel));

        // Outside capture both act again.
        input_map.prepare_frame(&b, true, true);
        assert!(input_map.just_act.contains(&Bulldozer));
        input_map.prepare_frame(&InputContext::default(), true, true);
        right_button(&mut input_map, true, Vec2::ZERO);
        right_button(&mut input_map, false, Vec2::ZERO);
        assert!(input_map.just_act.contains(&Cancel));
    }

    #[test]
    fn finalnative_keybind_capture_swallows_every_shortcut() {
        let mut bindings = Bindings::default();
        let mut input_map = InputMap::default();
        input_map.build_input_tree(&mut bindings);
        for key in [K::c("H"), K::c("E"), K::c("T"), K::Escape] {
            let mut input = InputContext::default();
            input.keyboard.pressed.insert(key.clone());
            input.mouse.wheel_delta = 1.0;
            input_map.prepare_frame(&input, true, true);
            assert!(!input_map.just_act.is_empty(), "{key:?} is no shortcut");
            input_map.suppress_shortcuts();
            assert!(
                input_map.just_act.is_empty(),
                "{key:?} acted during capture"
            );
            assert_eq!(input_map.wheel, 0.0, "{key:?} zoomed during capture");
            // Held after capture ends, it still does not act.
            input_map.prepare_frame(&input, true, true);
            assert!(input_map.just_act.is_empty(), "{key:?} acted after capture");
            input_map.prepare_frame(&InputContext::default(), true, true);
        }
    }

    #[test]
    fn uxfix_right_click_released_over_a_yakui_panel_does_not_cancel() {
        let mut ui = yakui::Yakui::new();
        let viewport = yakui::Vec2::new(400.0, 160.0);
        ui.set_surface_size(viewport);
        ui.set_unscaled_viewport(yakui::Rect::from_pos_size(yakui::Vec2::ZERO, viewport));
        ui.start();
        yakui::offset(yakui::Vec2::new(200.0, 0.0), || {
            yakui::opaque(|| {
                yakui::constrained(yakui::Constraints::tight(yakui::Vec2::splat(100.0)), || {});
            });
        });
        goryak::render_tooltip();
        ui.finish();
        ui.paint();
        assert!(goryak::pointer_over_ui(201.0, 50.0));
        assert!(!goryak::pointer_over_ui(198.0, 50.0));

        let mut bindings = Bindings::default();
        let mut input_map = InputMap::default();
        input_map.build_input_tree(&mut bindings);
        // Yakui lets the release through to the window, so the panel has to be checked.
        right_button(&mut input_map, true, Vec2::new(197.0, 50.0));
        right_button(&mut input_map, false, Vec2::new(201.0, 50.0));
        assert!(
            !input_map.just_act.contains(&Cancel),
            "released over a panel"
        );

        right_button(&mut input_map, true, Vec2::new(195.0, 50.0));
        right_button(&mut input_map, false, Vec2::new(198.0, 50.0));
        assert!(
            input_map.just_act.contains(&Cancel),
            "released over the world"
        );
    }

    #[test]
    fn ux2_new_defaults_are_max_speed_on_6_bulldozer_on_b_and_cancel_on_right_click() {
        let defaults = Bindings::default();
        assert_eq!(
            defaults.0[&SpeedMax].0,
            [InputCombination(vec![KeyScan(TOP_ROW_DIGITS[5])])]
        );
        assert_eq!(defaults.0[&SpeedMax].to_string(), "6");
        assert_eq!(
            defaults.0[&Bulldozer].0,
            [InputCombination(vec![Key(K::c("B"))])]
        );
        assert_eq!(
            defaults.0[&Cancel].0,
            [InputCombination(vec![Mouse(Right)])]
        );
        // B is free among the defaults.
        let b = Key(K::c("B"));
        assert!(defaults
            .0
            .iter()
            .filter(|(action, _)| **action != Bulldozer)
            .all(|(_, combinations)| combinations.0.iter().all(|c| !c.0.contains(&b))));
    }

    #[test]
    fn ux2_new_defaults_never_take_a_saved_binding() {
        let mut bindings = saved_bindings();
        bindings.fill_missing_defaults();
        assert_eq!(
            bindings.0[&Bulldozer].0,
            Bindings::default().0[&Bulldozer].0
        );
        assert_eq!(bindings.0[&SpeedMax].0, Bindings::default().0[&SpeedMax].0);
        assert_eq!(bindings.0[&Cancel].0, Bindings::default().0[&Cancel].0);

        let mut bindings = Bindings(BTreeMap::from([
            (
                OpenChat,
                InputCombinations(vec![InputCombination(vec![
                    Key(K::Control),
                    Key(K::c("B")),
                ])]),
            ),
            (
                GoLeft,
                InputCombinations(vec![InputCombination(vec![Key(K::c("6"))])]),
            ),
            (
                CameraRotate,
                InputCombinations(vec![InputCombination(vec![Mouse(Right)])]),
            ),
            (
                CameraMove,
                InputCombinations(vec![InputCombination(vec![Key(K::Shift), Mouse(Right)])]),
            ),
        ]));
        bindings.fill_missing_defaults();
        assert!(bindings.0[&Bulldozer].0.is_empty(), "B was hijacked");
        assert!(bindings.0[&SpeedMax].0.is_empty(), "6 was hijacked");
        // Right click and right drag share the button by design.
        assert_eq!(bindings.0[&Cancel].0, Bindings::default().0[&Cancel].0);

        let mut bindings = Bindings(BTreeMap::from([(
            Select,
            InputCombinations(vec![InputCombination(vec![Mouse(Right)])]),
        )]));
        bindings.fill_missing_defaults();
        assert!(bindings.0[&Cancel].0.is_empty(), "right click was hijacked");

        // A saved binding for the new action is kept as is.
        let mut bindings = Bindings(BTreeMap::from([(
            Bulldozer,
            InputCombinations(vec![InputCombination(vec![Key(K::c("X"))])]),
        )]));
        bindings.fill_missing_defaults();
        assert_eq!(
            bindings.0[&Bulldozer].0,
            [InputCombination(vec![Key(K::c("X"))])]
        );
    }

    fn saved_bindings() -> Bindings {
        Bindings(BTreeMap::from([
            (
                GoRight,
                InputCombinations(vec![InputCombination(vec![Key(K::c("1"))])]),
            ),
            (
                Select,
                InputCombinations(vec![InputCombination(vec![KeyScan(TOP_ROW_DIGITS[1])])]),
            ),
            (
                GoLeft,
                InputCombinations(vec![InputCombination(vec![
                    Key(K::Control),
                    Key(K::c("3")),
                ])]),
            ),
            (Speed50, InputCombinations(vec![])),
        ]))
    }
}

#[cfg(test)]
mod water_w4_tests {
    use super::*;
    use geom::{vec2, vec3};
    use simulation::map::Environment;

    #[test]
    fn water_w4_picks_the_surface_before_the_bed_and_respects_tile_edges() {
        let mut env = Environment::new(1, 1);
        let basin = [
            vec2(64.0, 64.0),
            vec2(96.0, 64.0),
            vec2(96.0, 96.0),
            vec2(64.0, 96.0),
        ];
        env.fill_lake(&geom::Polygon::from(basin.to_vec()), -10, 4.0)
            .unwrap();

        let cast = |x| {
            SurfaceHits::cast(
                &env,
                Ray3 {
                    from: vec3(x, 80.0, 20.0),
                    dir: -Vec3::Z,
                },
            )
        };
        let inside = cast(80.0);
        assert!(inside.ground.unwrap().0.z < -1.0);
        assert_eq!(inside.nearest().unwrap().z, -1.0);
        assert!(inside.water.is_some());
        let edge = cast(96.0);
        assert!(edge.water.is_none());
        assert!(edge.nearest().is_some());
    }
}
