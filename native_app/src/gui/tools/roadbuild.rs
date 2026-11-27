use engine::AudioKind;
use geom::{
    BoldLine, BoldSpline, Camera, Line, LinearColor, PolyLine, Radians, Ray, ShapeEnum, Spline,
};
use geom::{PolyLine3, Vec2, Vec3};
use itertools::Itertools;
use ordered_float::OrderedFloat;
use simulation::map::{
    ConnectionError, LanePattern, LanePatternBuilder, Map, MapProject, ProjectFilter, ProjectKind,
    PylonPosition, RoadPreview, ROAD_Z_OFFSET, ZONE_CELL_SIZE,
};
use simulation::world_command::{Rejection, WorldCommand, WorldCommands};
use simulation::Simulation;
use BuildState::{Curved, CurvedConnection, Hover, Start, StartCurved};
use ProjectKind::{Building, Ground, Intersection, Road};

use crate::gui::{ErrorTooltip, PotentialCommands, Tool};
use crate::inputmap::{DeckPicker, InputAction, InputMap};
use crate::rendering::immediate::{ImmediateDraw, ImmediateSound};
use crate::uiworld::UiWorld;

#[derive(Copy, Clone, Debug, Default)]
pub enum BuildState {
    #[default]
    /// Default hovered state
    Hover,
    /// State in "Straight" mode after clicking on something
    Start(MapProject),
    /// State in "Curved" mode after clicking on something
    StartCurved(MapProject),
    /// State in "Curved" mode after second click on a road/inter: We have first/last point and
    /// next click determines the interpolation point
    CurvedConnection(MapProject, MapProject),
    /// State in "Curved" mode after second click on the ground: the Vec2 is the interpolation point
    Curved(Vec2, MapProject),
}

/// Road building tool
/// Allows to build roads and intersections
pub fn roadbuild(sim: &Simulation, uiworld: &UiWorld) {
    profiling::scope!("gui::roadbuild");
    let state = &mut *uiworld.write::<RoadBuildResource>();
    let immdraw = &mut *uiworld.write::<ImmediateDraw>();
    let immsound = &mut *uiworld.write::<ImmediateSound>();
    let potential_command = &mut *uiworld.write::<PotentialCommands>();
    let inp = uiworld.read::<InputMap>();
    let tool = *uiworld.read::<Tool>();
    let map = &*sim.map();
    let commands: &mut WorldCommands = &mut uiworld.commands();
    let cam = &*uiworld.read::<Camera>();

    if !tool.is_roadbuild() {
        state.build_state = Hover;
        state.height_offset = 0.0;
        return;
    }

    // Switching between straight and curved mode keeps the selected start point.
    state.build_state = match (tool, state.build_state) {
        (Tool::RoadbuildCurved, Start(start)) => StartCurved(start),
        (Tool::RoadbuildStraight, StartCurved(start) | Curved(_, start))
        | (Tool::RoadbuildStraight, CurvedConnection(start, _)) => Start(start),
        (_, build_state) => build_state,
    };

    let builder = if tool == Tool::LocalOneWay {
        LanePatternBuilder::local_one_way(state.local_curb_parking)
    } else {
        state.pattern_builder
    };
    let grid_size = 20.0;
    let nosnapping = inp.act.contains(&InputAction::NoSnapping);
    let precision = !nosnapping
        && inp.act.contains(&InputAction::PrecisionRoad)
        && !matches!(state.build_state, Hover);
    let attach = !nosnapping && !matches!(state.build_state, CurvedConnection(..));
    let surfaces = inp.surface_hits(map.environment());
    let deck = inp.ray.filter(|_| attach).and_then(|ray| {
        DeckPicker::new(map).pick(ray, &surfaces, ProjectFilter::ROAD | ProjectFilter::INTER)
    });
    let surface = surfaces
        .water
        .map(|hit| hit.point)
        .or_else(|| surfaces.ground.map(|(pos, _)| pos.xy().z(pos.z.max(0.0))))
        .or(inp.unprojected);
    let unproj = unwrap_ret!(surface.or_else(|| deck.map(|hit| hit.pos)));
    let mut interpolation_points: Vec<Vec3> = Vec::new();

    // Prepare mousepos depending on snap to grid or snap to angle
    let mousepos = match if precision {
        Snapping::None
    } else {
        state.snapping
    } {
        Snapping::None => state.mouse_height(map, unproj),
        Snapping::SnapToGrid => state.grid_mouse_height(map, unproj, grid_size),
        Snapping::SnapToAngle => {
            interpolation_points = state
                .possible_interpolations_for(map, unproj, builder.rail)
                .unwrap_or_default();
            state.mouse_height(
                map,
                interpolation_points
                    .iter()
                    .filter_map(|&point| {
                        let distance = point.distance(unproj);
                        if distance < grid_size {
                            Some((point, distance))
                        } else {
                            None
                        }
                    })
                    .min_by_key(|(_, distance)| OrderedFloat(*distance))
                    .unwrap_or((unproj, 0.0))
                    .0,
            )
        }
    };

    let log_camheight = cam.eye().z.log10();

    // If a road was placed recently (as it is async with networking) prepare the next road.
    if !state.suppress_chaining {
        for command in uiworld.received_commands().iter() {
            if let WorldCommand::MapMakeConnection { to, .. } = command {
                if let proj @ MapProject {
                    kind: Intersection(_),
                    ..
                } = map.project(to.pos, 0.0, ProjectFilter::ALL)
                {
                    if matches!(tool, Tool::RoadbuildCurved) {
                        state.build_state = StartCurved(proj);
                    } else {
                        state.build_state = Start(proj);
                    }
                }
            }
        }
    }

    if inp.just_act.contains(&InputAction::UpElevation) {
        state.height_offset += 5.0;
        state.height_offset = state.height_offset.min(100.0);
    }

    if inp.just_act.contains(&InputAction::DownElevation) {
        state.height_offset -= 5.0;
        state.height_offset = state.height_offset.max(0.0);
    }

    let mut cur_proj = if attach {
        deck.unwrap_or_else(|| {
            project_ground_attachment(map, mousepos, (log_camheight * 5.0).clamp(1.0, 10.0))
        })
    } else {
        MapProject::ground(mousepos)
    };

    let patwidth = builder.width();

    if let Road(r_id) = cur_proj.kind {
        let r = &map.roads()[r_id];
        if r.points
            .first()
            .is_close(cur_proj.pos, r.interface_from(r.src) + patwidth * 0.5)
        {
            cur_proj = MapProject {
                kind: Intersection(r.src),
                pos: r.points.first(),
            };
        } else if r
            .points
            .last()
            .is_close(cur_proj.pos, r.interface_from(r.dst) + patwidth * 0.5)
        {
            cur_proj = MapProject {
                kind: Intersection(r.dst),
                pos: r.points.last(),
            };
        }
    }

    let mut measurement = None;
    if precision {
        let (start, anchor) = match state.build_state {
            Start(start) | StartCurved(start) | CurvedConnection(start, _) => (start, start.pos),
            Curved(control, start) => (start, control.z(start.pos.z)),
            Hover => unreachable!("precision requires a start point"),
        };
        let snap = PrecisionSnap::at(map, start, anchor, unproj);
        // Existing road and junction attachments keep their exact position.
        if cur_proj.is_ground() {
            let env = map.environment();
            let height = env
                .water_surface_elevation(snap.point)
                .or_else(|| env.height(snap.point))
                .unwrap_or(unproj.z);
            cur_proj.pos = state.mouse_height(map, snap.point.z(height));
        }
        measurement = Some((anchor, snap));
    }

    let is_rail = builder.rail;

    let mut is_valid = match (state.build_state, cur_proj.kind) {
        (Hover, Building(_)) => false,
        (StartCurved(sel_proj), Ground) => {
            compatible(map, cur_proj, sel_proj)
                && check_angle(map, sel_proj, cur_proj.pos.xy(), is_rail)
        }
        (StartCurved(sel_proj), Intersection(_) | Road(_)) => compatible(map, sel_proj, cur_proj),
        (Start(selected_proj), _) => {
            let sp = BoldLine::new(
                PolyLine::new(vec![selected_proj.pos.xy(), cur_proj.pos.xy()]),
                patwidth * 0.5,
            );

            compatible(map, cur_proj, selected_proj)
                && check_angle(map, selected_proj, cur_proj.pos.xy(), is_rail)
                && check_angle(map, cur_proj, selected_proj.pos.xy(), is_rail)
                && !check_intersect(
                    map,
                    &ShapeEnum::BoldLine(sp),
                    (selected_proj.pos.z + cur_proj.pos.z) / 2.0,
                    cur_proj.kind,
                    selected_proj.kind,
                )
        }
        (CurvedConnection(src, dst), _) => {
            let sp = Spline {
                from: src.pos.xy(),
                to: dst.pos.xy(),
                from_derivative: (cur_proj.pos.xy() - src.pos.xy())
                    * std::f32::consts::FRAC_1_SQRT_2,
                to_derivative: (dst.pos.xy() - cur_proj.pos.xy()) * std::f32::consts::FRAC_1_SQRT_2,
            };

            compatible(map, dst, src)
                && check_angle(map, src, cur_proj.pos.xy(), is_rail)
                && check_angle(map, dst, cur_proj.pos.xy(), is_rail)
                && !sp.is_steep(builder.width())
                && !check_intersect(
                    map,
                    &ShapeEnum::BoldSpline(BoldSpline::new(sp, patwidth * 0.5)),
                    (src.pos.z + dst.pos.z) / 2.0,
                    src.kind,
                    dst.kind,
                )
        }
        (Curved(interpoint, selected_proj), _) => {
            let sp = Spline {
                from: selected_proj.pos.xy(),
                to: cur_proj.pos.xy(),
                from_derivative: (interpoint - selected_proj.pos.xy())
                    * std::f32::consts::FRAC_1_SQRT_2,
                to_derivative: (cur_proj.pos.xy() - interpoint) * std::f32::consts::FRAC_1_SQRT_2,
            };

            compatible(map, cur_proj, selected_proj)
                && check_angle(map, selected_proj, interpoint, is_rail)
                && check_angle(map, cur_proj, interpoint, is_rail)
                && !sp.is_steep(builder.width())
                && !check_intersect(
                    map,
                    &ShapeEnum::BoldSpline(BoldSpline::new(sp, patwidth * 0.5)),
                    (selected_proj.pos.z + cur_proj.pos.z) / 2.0,
                    selected_proj.kind,
                    cur_proj.kind,
                )
        }
        _ => true,
    };

    let build_args = match state.build_state {
        StartCurved(selected_proj) if !cur_proj.is_ground() => {
            Some((selected_proj, cur_proj, None, builder.build()))
        }
        Start(selected_proj) => Some((selected_proj, cur_proj, None, builder.build())),
        CurvedConnection(src, dst) => Some((src, dst, Some(cur_proj.pos.xy()), builder.build())),

        Curved(interpoint, selected_proj) => {
            let inter = Some(interpoint);
            Some((selected_proj, cur_proj, inter, builder.build()))
        }
        _ => None,
    };
    potential_command.0.clear();

    let mut points = None;

    if let Some((src, dst, inter, ref pat)) = build_args {
        match state.preview(map, src, dst, inter, pat) {
            Ok(preview) => {
                points = Some(preview.deck.clone());
                let color = if is_valid {
                    simulation::colors().gui_primary
                } else {
                    simulation::colors().gui_danger
                };
                for outline in &preview.junctions {
                    immdraw.polyline(outline.clone(), 1.0, true).color(color);
                }
                if is_valid {
                    if preview.crosses_water {
                        *uiworld.write::<ErrorTooltip>() = ErrorTooltip::new("Bridge");
                    }
                    potential_command.set(WorldCommand::MapMakeConnection {
                        from: src,
                        to: dst,
                        inter,
                        pat: pat.clone(),
                    });
                }
            }
            Err(error) => {
                let message = match error {
                    ConnectionError::LandDenied(reason) => {
                        crate::gui::notifications::command_message(
                            "MapMakeConnection",
                            Rejection::LandDenied(*reason),
                        )
                    }
                    _ => error.to_string().into(),
                };
                *uiworld.write::<ErrorTooltip>() = ErrorTooltip::new(message);
                is_valid = false;
                let (a, b) = match (
                    src.road_connection_position(map),
                    dst.road_connection_position(map),
                ) {
                    (Some(a), Some(b)) => (a, b),
                    _ => (src.pos, dst.pos),
                };
                if a.is_finite() && b.is_finite() && a != b {
                    immdraw
                        .line(a.up(0.4), b.up(0.4), patwidth)
                        .color(simulation::colors().gui_danger);
                }
            }
        }
    } else if cur_proj.kind == Ground
        && matches!(state.build_state, Hover)
        && map.environment().is_water(cur_proj.pos.xy())
    {
        is_valid = false;
        *uiworld.write::<ErrorTooltip>() =
            ErrorTooltip::new("Start on dry land or attach to an elevated deck");
    }

    if let Some((anchor, _)) = &measurement {
        let tooltip = &mut *uiworld.write::<ErrorTooltip>();
        tooltip.detail = Some(precision_readout(
            map,
            *anchor,
            cur_proj,
            build_args.as_ref(),
            points.as_ref(),
        ));
        tooltip.isworld = true;
    }

    state.update_drawing(
        map,
        immdraw,
        cur_proj,
        patwidth,
        is_valid,
        points,
        interpolation_points,
    );

    if let Some((anchor, snap)) = measurement {
        snap.draw(immdraw, anchor, cur_proj.pos, patwidth);
    }

    if is_valid && inp.just_act.contains(&InputAction::Select) {
        log::info!(
            "left clicked with state {:?} and {:?}",
            state.build_state,
            cur_proj.kind
        );

        match (state.build_state, cur_proj.kind) {
            (Hover, Ground | Road(_) | Intersection(_)) => {
                // Hover selection
                state.suppress_chaining = false;
                if tool == Tool::RoadbuildCurved {
                    state.build_state = StartCurved(cur_proj);
                } else {
                    state.build_state = Start(cur_proj);
                }
            }
            (StartCurved(v), Ground) => {
                // Set interpolation point
                state.build_state = Curved(cur_proj.pos.xy(), v);
            }
            (StartCurved(p), Road(_) | Intersection(_)) => {
                // Set interpolation point
                state.build_state = CurvedConnection(p, cur_proj);
            }

            (Start(_), _) => {
                // Straight connection to something
                immsound.play("road_lay", AudioKind::Ui);
                if let Some(wc) = potential_command.0.drain(..).next() {
                    commands.push(wc);
                }
                state.build_state = Hover;
            }
            (CurvedConnection(_, _), _) => {
                immsound.play("road_lay", AudioKind::Ui);
                if let Some(wc) = potential_command.0.drain(..).next() {
                    commands.push(wc);
                }
                state.build_state = Hover;
            }
            (Curved(_, _), _) => {
                // Interpolated connection to something
                immsound.play("road_lay", AudioKind::Ui);
                if let Some(wc) = potential_command.0.drain(..).next() {
                    commands.push(wc);
                }
                state.build_state = Hover;
            }
            _ => {}
        }
    }
}

const PRECISION_ANGLE: f32 = std::f32::consts::PI / 12.0;
const ALIGNMENT_ANGLE: f32 = std::f32::consts::PI / 36.0;
const ALIGNMENT_DISTANCE: f32 = 24.0;

pub(crate) struct PrecisionSnap {
    pub(crate) point: Vec2,
    alignment: Option<Vec2>,
}

impl PrecisionSnap {
    pub(crate) fn at(map: &Map, start: MapProject, anchor: Vec3, pointer: Vec3) -> Self {
        let delta = pointer.xy() - anchor.xy();
        let angle = delta.angle_cossin().0;
        let preferred = |a: Vec2, b: Vec2| {
            let error = |dir: Vec2| {
                let angle = dir.angle(delta);
                (angle
                    - (angle / std::f32::consts::FRAC_PI_2).round() * std::f32::consts::FRAC_PI_2)
                    .abs()
            };
            error(a).total_cmp(&error(b))
        };
        let start_axis = match start.kind {
            Intersection(id) => map.get(id).and_then(|junction| {
                junction
                    .roads
                    .iter()
                    .filter_map(|&road| map.get(road))
                    .map(|road| road.dir_from(id))
                    .min_by(|&a, &b| preferred(a, b))
            }),
            Road(id) => map
                .get(id)
                .and_then(|road| road_axis(road, start.pos.xy()).1),
            _ => None,
        };
        let quantize = |base: f32, step: f32| base + ((angle - base) / step).round() * step;
        let nearby = map
            .spatial_map()
            .query_around(pointer.xy(), ALIGNMENT_DISTANCE, ProjectFilter::ROAD)
            .filter_map(|kind| {
                let Road(id) = kind else { return None };
                let road = map.get(id)?;
                let (point, direction) = road_axis(road, pointer.xy());
                let distance = (point.xy().distance(pointer.xy()) - road.width * 0.5).max(0.0);
                let axis = direction?;
                let aligned = quantize(axis.angle_cossin().0, std::f32::consts::FRAC_PI_2);
                ((point.z - pointer.z).abs() < 4.0
                    && distance <= ALIGNMENT_DISTANCE
                    && (aligned - angle).abs() <= ALIGNMENT_ANGLE)
                    .then_some((OrderedFloat(distance), id, axis, aligned))
            })
            .min_by_key(|&(distance, id, _, _)| (distance, id));
        let (direction, alignment) = nearby.map_or_else(
            || {
                let reference = start_axis.unwrap_or(Vec2::X).angle_cossin().0;
                (quantize(reference, PRECISION_ANGLE), start_axis)
            },
            |(_, _, axis, aligned)| (aligned, Some(axis)),
        );
        let length = (delta.mag() / ZONE_CELL_SIZE).round() * ZONE_CELL_SIZE;
        let point = anchor.xy() + Radians(direction).vec2() * length;
        Self { point, alignment }
    }

    pub(crate) fn draw(&self, draw: &mut ImmediateDraw, from: Vec3, to: Vec3, width: f32) {
        let from = from.up(0.85);
        let to = to.up(0.85);
        let guide = LinearColor::new(0.87, 0.97, 1.0, 1.0);
        let segments = (from.distance(to) / 8.0).ceil().clamp(1.0, 36.0) as usize;
        if from.xy() != to.xy() {
            for i in 0..segments {
                let a = i as f32 / segments as f32;
                let b = (i as f32 + 0.55) / segments as f32;
                draw.line(from.lerp(to, a), from.lerp(to, b), 0.65)
                    .color(guide);
            }
        }
        let axis = self.alignment.unwrap_or(Vec2::X);
        for axis in [axis, axis.perpendicular()] {
            let offset = (axis * (width * 0.5 + 4.0)).z0();
            draw.line(to - offset, to + offset, 0.55).color(guide);
        }
        draw.stroke_circle(to, 2.5, 0.45).color(guide);
    }
}

fn road_axis(road: &simulation::map::Road, xy: Vec2) -> (Vec3, Option<Vec2>) {
    let points = road.points();
    let (point, segment) = points.project_segment_2d(xy);
    let direction = segment
        .checked_sub(1)
        .and_then(|start| points.segment_vec(start))
        .and_then(|segment| segment.xy().try_normalize());
    (point, direction)
}

fn precision_readout(
    map: &Map,
    anchor: Vec3,
    cursor: MapProject,
    connection: Option<&(MapProject, MapProject, Option<Vec2>, LanePattern)>,
    deck: Option<&PolyLine3>,
) -> String {
    let (from, to) = connection.map_or((anchor, cursor.pos), |(from, to, _, _)| {
        (
            from.road_connection_position(map).unwrap_or(from.pos),
            to.road_connection_position(map).unwrap_or(to.pos),
        )
    });
    let span = deck.map_or_else(
        || from.xy().distance(to.xy()),
        |deck| {
            deck.segments()
                .map(|segment| segment.src.xy().distance(segment.dst.xy()))
                .sum()
        },
    );
    let angle = (cursor.pos.xy() - anchor.xy())
        .angle_cossin()
        .0
        .to_degrees()
        .rem_euclid(360.0);
    let grade = if span > 0.0 {
        (to.z - from.z) / span * 100.0
    } else {
        0.0
    };
    let attachment = match cursor.kind {
        Intersection(_) => "  ·  Junction",
        Road(_) => "  ·  Road split",
        _ => "",
    };
    format!("{span:.1} m   ·   {angle:.0}°   ·   {grade:+.1}%{attachment}")
}

#[derive(Default)]
pub struct RoadBuildResource {
    pub build_state: BuildState,
    pub pattern_builder: LanePatternBuilder,
    pub local_curb_parking: bool,
    pub snapping: Snapping,
    pub height_offset: f32,
    pub height_reference: HeightReference,
    suppress_chaining: bool,
    preview: Option<CachedConnectionPreview>,
}

struct CachedConnectionPreview {
    revision: (u64, u64, u64, u64, usize),
    from: MapProject,
    to: MapProject,
    elbow: Option<Vec2>,
    pattern: LanePattern,
    result: Result<RoadPreview, ConnectionError>,
}

impl RoadBuildResource {
    fn preview(
        &mut self,
        map: &Map,
        from: MapProject,
        to: MapProject,
        elbow: Option<Vec2>,
        pattern: &LanePattern,
    ) -> &Result<RoadPreview, ConnectionError> {
        let revision = map.road_preview_revision();
        let same_project = |a: MapProject, b: MapProject| a.kind == b.kind && a.pos == b.pos;
        if !self.preview.as_ref().is_some_and(|preview| {
            preview.revision == revision
                && same_project(preview.from, from)
                && same_project(preview.to, to)
                && preview.elbow == elbow
                && &preview.pattern == pattern
        }) {
            self.preview = Some(CachedConnectionPreview {
                revision,
                from,
                to,
                elbow,
                pattern: pattern.clone(),
                result: map.preview_connection(from, to, elbow, pattern),
            });
        }
        &self.preview.as_ref().unwrap().result
    }

    fn grid_mouse_height(&self, map: &Map, unprojected: Vec3, grid_size: f32) -> Vec3 {
        let snapped = unprojected.xy().snap(grid_size, grid_size);
        let env = map.environment();
        let height = env
            .water_surface_elevation(snapped)
            .or_else(|| env.height(snapped))
            .unwrap_or(unprojected.z);
        self.mouse_height(map, snapped.z(height))
    }

    fn mouse_height(&self, map: &Map, h: Vec3) -> Vec3 {
        match (self.height_reference, self.build_state) {
            (
                HeightReference::Start,
                Start(start) | StartCurved(start) | CurvedConnection(start, _) | Curved(_, start),
            ) => {
                let start_height = start
                    .road_connection_position(map)
                    .map_or(start.pos.z, |p| p.z);
                h.xy().z(start_height + self.height_offset)
            }
            (HeightReference::Ground | HeightReference::Start, _) => h.up(self.height_offset),
            (HeightReference::MaxIncline | HeightReference::MaxDecline, _) => h, // work in progress
        }
    }

    pub(crate) fn cancel_placement(&mut self) -> bool {
        if matches!(self.build_state, Hover) {
            return false;
        }
        self.build_state = Hover;
        // A delayed command acknowledgement can arrive after Close.
        self.suppress_chaining = true;
        true
    }
}

#[derive(Default, Clone, Copy)]
pub enum Snapping {
    None,
    SnapToGrid,
    #[default]
    SnapToAngle,
}

#[derive(Default, Clone, Copy)]
pub enum HeightReference {
    #[default]
    Ground,
    Start,
    MaxIncline,
    MaxDecline,
}

fn project_ground_attachment(map: &Map, pos: Vec3, tolerance: f32) -> MapProject {
    let env = map.environment();
    if env.is_water(pos.xy()) {
        return MapProject::ground(pos);
    }
    let on_ground = |point: Vec3| {
        !env.is_water(point.xy())
            && env
                .height(point.xy())
                .is_some_and(|z| (z - point.z).abs() <= 1.0)
    };
    let mut road_hit = None;
    for kind in map.spatial_map().query_around(
        pos.xy(),
        tolerance,
        ProjectFilter::ROAD | ProjectFilter::INTER,
    ) {
        match kind {
            Intersection(id) => {
                let intersection = &map.intersections()[id];
                if on_ground(intersection.pos) {
                    return MapProject {
                        pos: intersection.pos,
                        kind,
                    };
                }
            }
            Road(id) if road_hit.is_none() => {
                let road = &map.roads()[id];
                if !road.elevated_span() {
                    let point = road.points().project(pos);
                    if on_ground(point - Vec3::z(ROAD_Z_OFFSET)) {
                        road_hit = Some(MapProject { pos: point, kind });
                    }
                }
            }
            _ => {}
        }
    }
    road_hit.unwrap_or_else(|| MapProject::ground(pos))
}

fn check_angle(map: &Map, from: MapProject, to: Vec2, is_rail: bool) -> bool {
    let max_turn_angle = if is_rail {
        0.0
    } else {
        25.0 * std::f32::consts::PI / 180.0
    };

    match from.kind {
        Intersection(i) => {
            let Some(inter) = map.intersections().get(i) else {
                return false;
            };
            let dir = (to - inter.pos.xy()).normalize();

            inter
                .roads
                .iter()
                .map(|road_id| map.roads()[*road_id].dir_from(i))
                .all(|v| v.angle(dir).abs() >= max_turn_angle)
        }
        Road(r) => {
            let Some(r) = map.roads().get(r) else {
                return false;
            };
            let (proj, _, rdir1) = r.points().project_segment_dir(from.pos);
            let rdir2 = -rdir1;
            let dir = (to - proj.xy()).normalize();

            rdir1.xy().angle(dir).abs() >= max_turn_angle
                && rdir2.xy().angle(dir).abs() >= max_turn_angle
        }
        _ => true,
    }
}

fn compatible(map: &Map, x: MapProject, y: MapProject) -> bool {
    if x.pos.distance(y.pos) < 10.0 {
        return false;
    }
    match (x.kind, y.kind) {
        (Ground, Ground)
        | (Ground, Road(_))
        | (Ground, Intersection(_))
        | (Road(_), Ground)
        | (Intersection(_), Ground) => true,
        (Road(id), Road(id2)) => id != id2,
        (Intersection(id), Intersection(id2)) => id != id2,
        (Intersection(id_inter), Road(id_road)) | (Road(id_road), Intersection(id_inter)) => {
            let r = &map.roads()[id_road];
            r.src != id_inter && r.dst != id_inter
        }
        _ => false,
    }
}

/// Check if the given shape intersects with any existing road or intersection
fn check_intersect(
    map: &Map,
    obj: &ShapeEnum,
    z: f32,
    start: ProjectKind,
    end: ProjectKind,
) -> bool {
    map.spatial_map()
        .query(obj, ProjectFilter::ROAD | ProjectFilter::INTER)
        .any(move |x| {
            if let Road(rid) = x {
                let r = &map.roads()[rid];
                if (r.points.first().z - z).abs() > 1.0 || (r.points.last().z - z).abs() > 1.0 {
                    return false;
                }
                if let Intersection(id) = start {
                    if r.src == id || r.dst == id {
                        return false;
                    }
                }
                if let Intersection(id) = end {
                    if r.src == id || r.dst == id {
                        return false;
                    }
                }
            }
            x != start && x != end
        })
}

impl RoadBuildResource {
    pub fn update_drawing(
        &self,
        map: &Map,
        immdraw: &mut ImmediateDraw,
        proj: MapProject,
        patwidth: f32,
        is_valid: bool,
        points: Option<PolyLine3>,
        interpolation_points: Vec<Vec3>,
    ) {
        let mut proj_pos = proj.pos;
        proj_pos.z += 0.4;
        let col = if is_valid {
            simulation::colors().gui_primary
        } else {
            simulation::colors().gui_danger
        };

        interpolation_points.iter().for_each(|p| {
            immdraw.circle(*p, 2.0);
        });

        let p = match self.build_state {
            Hover => {
                immdraw.circle(proj_pos, patwidth * 0.5).color(col);
                return;
            }
            StartCurved(x) if proj.kind.is_ground() => {
                let dir = unwrap_or!((proj_pos - x.pos).try_normalize(), {
                    immdraw.circle(proj_pos, patwidth * 0.5).color(col);
                    return;
                });
                let mut poly = Vec::with_capacity(33);
                for i in 0..=32 {
                    let ang = std::f32::consts::PI * i as f32 * (2.0 / 32.0);
                    let mut v = Vec3::from_angle(ang, dir.z);
                    let center = if v.dot(dir) < 0.0 {
                        x.pos.up(0.4)
                    } else {
                        proj_pos
                    };

                    v = v * patwidth * 0.5;
                    v.z = 0.0;
                    v += center;

                    poly.push(v);
                }
                immdraw.polyline(poly, 3.0, true).color(col);

                return;
            }
            _ => unwrap_ret!(points),
        };

        for PylonPosition {
            terrain_height,
            pos,
            ..
        } in simulation::map::Road::pylons_positions(&p, map.environment())
        {
            immdraw
                .circle(pos.xy().z(terrain_height + 0.1), patwidth * 0.5)
                .color(col);
        }

        immdraw.circle(p.first().up(0.1), patwidth * 0.5).color(col);
        immdraw.circle(p.last().up(0.1), patwidth * 0.5).color(col);
        immdraw
            .polyline(
                p.into_vec()
                    .into_iter()
                    .map(|v| v.up(0.1))
                    .collect::<Vec<_>>(),
                patwidth,
                false,
            )
            .color(col);
    }

    #[cfg(test)]
    pub fn possible_interpolations(&self, map: &Map, mousepos: Vec3) -> Option<Vec<Vec3>> {
        self.possible_interpolations_for(map, mousepos, self.pattern_builder.rail)
    }

    fn possible_interpolations_for(
        &self,
        map: &Map,
        mousepos: Vec3,
        is_rail: bool,
    ) -> Option<Vec<Vec3>> {
        let (start, end) = match self.build_state {
            Hover | Curved(_, _) => {
                return None;
            }
            CurvedConnection(src, dst) => (src, dst),
            Start(sel_proj) | StartCurved(sel_proj) => (sel_proj, MapProject::ground(mousepos)),
        };

        match (start.kind, end.kind) {
            (Intersection(id0), Intersection(id1)) => {
                let inter0 = map.intersections().get(id0)?;
                let inter1 = map.intersections().get(id1)?;

                Some(
                    inter0
                        .roads
                        .iter()
                        .cartesian_product(inter1.roads.iter())
                        .filter_map(|(&r0, &r1)| {
                            let road0 = map.roads().get(r0)?;
                            let road1 = map.roads().get(r1)?;

                            let ray0 = Ray::new(inter0.pos.xy(), -road0.dir_from(id0));
                            let ray1 = Ray::new(inter1.pos.xy(), -road1.dir_from(id1));

                            let p = ray0.intersection_point(&ray1)?;
                            let h = map.environment().height(p)?;
                            Some(p.z(h))
                        })
                        .collect(),
                )
            }

            (Intersection(id), Ground) | (Ground, Intersection(id)) => {
                let inter = map.intersections().get(id)?;

                Some(
                    inter
                        .roads
                        .iter()
                        .filter_map(|&road_id| {
                            let road = map.roads().get(road_id)?;

                            let p = Line::new_dir(inter.pos.xy(), road.dir_from(id))
                                .project(mousepos.xy());

                            let h = map.environment().height(p)?;

                            Some(p.z(h))
                        })
                        .collect::<Vec<_>>(),
                )
            }

            (Intersection(inter_id), Road(road_id)) | (Road(road_id), Intersection(inter_id))
                if is_rail =>
            {
                let inter = map.intersections().get(inter_id)?;
                let road = map.roads().get(road_id)?;

                let pos = if start.kind == Road(road_id) {
                    start.pos
                } else {
                    end.pos
                };
                let (pos, _, dir) = road.points().project_segment_dir(pos);

                Some(
                    inter
                        .roads
                        .iter()
                        .map(|&road_id| &map.roads()[road_id])
                        .filter_map(|road| {
                            let line0 = Line::new_dir(inter.pos.xy(), -road.dir_from(inter_id));
                            let line1 = Line::new_dir(pos.xy(), dir.xy());

                            let p = line0.intersection_point(&line1)?;
                            let h = map.environment().height(p)?;

                            Some(p.z(h))
                        })
                        .collect(),
                )
            }

            (Road(id), Ground) | (Ground, Road(id)) if is_rail => {
                let road = map.roads().get(id)?;

                let pos = if start.kind == Road(id) {
                    start.pos
                } else {
                    end.pos
                };
                let (pos, _, dir) = road.points().project_segment_dir(pos);

                let line = Line::new_dir(pos.xy(), dir.xy());
                let p = line.project(mousepos.xy());

                let h = map.environment().height(p)?;
                Some(vec![p.z(h)])
            }
            (Road(id0), Road(id1)) if is_rail => {
                let road0 = map.roads().get(id0)?;
                let road1 = map.roads().get(id1)?;

                let (pos0, _, dir0) = road0.points().project_segment_dir(start.pos);
                let (pos1, _, dir1) = road1.points().project_segment_dir(end.pos);

                let line0 = Line::new_dir(pos0.xy(), dir0.xy());
                let line1 = Line::new_dir(pos1.xy(), dir1.xy());

                let p = line0.intersection_point(&line1)?;
                let h = map.environment().height(p)?;

                Some(vec![p.z(h)])
            }

            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::{vec2, vec3};
    use simulation::map::Environment;

    #[test]
    fn roadtools_precision_applies_the_previewed_straight_endpoint_and_curve_control() {
        scenarios::ensure_repo_root_cwd();
        simulation::init::init();
        let sim = Simulation::new_authored_flat(1, 0).unwrap();
        let origin = vec3(120.0, 110.0, 0.0);
        let pointer = vec3(191.0, 129.0, 0.0);
        let snapped = origin.xy() + Radians::from_deg(15.0).vec2() * 72.0;
        for tool in [Tool::RoadbuildStraight, Tool::RoadbuildCurved] {
            let ui = ui(tool);
            let start = MapProject::ground(origin);
            ui.write::<RoadBuildResource>().build_state = if tool == Tool::RoadbuildStraight {
                Start(start)
            } else {
                StartCurved(start)
            };
            {
                let mut input = ui.write::<InputMap>();
                input.unprojected = Some(pointer);
                input.act.insert(InputAction::PrecisionRoad);
                input.just_act.insert(InputAction::Select);
            }
            roadbuild(&sim, &ui);

            assert!(ui
                .read::<ErrorTooltip>()
                .detail
                .as_deref()
                .unwrap()
                .contains("72.0 m"));
            match tool {
                Tool::RoadbuildStraight => {
                    let queued = ui.commands();
                    let [WorldCommand::MapMakeConnection { to, .. }] = queued.as_ref() else {
                        panic!("expected one road command");
                    };
                    assert!(to.pos.xy().is_close(snapped, 0.001));
                }
                _ => assert!(matches!(
                    ui.read::<RoadBuildResource>().build_state,
                    Curved(control, _) if control.is_close(snapped, 0.001)
                )),
            }
        }
    }

    #[test]
    fn roadtools_precision_aligns_to_a_nearby_road() {
        scenarios::ensure_repo_root_cwd();
        simulation::init::init();
        let mut map = Map::empty();
        assert!(map.install_environment(Environment::new(1, 1)));
        let direction = Radians::from_deg(24.0).vec2();
        let a = vec2(40.0, 60.0);
        let b = a + direction * 320.0;
        map.make_connection(
            MapProject::ground(a.z0()),
            MapProject::ground(b.z0()),
            None,
            &LanePatternBuilder::new().build(),
        )
        .unwrap();

        for axis in [direction, direction.perpendicular()] {
            let pointer = (a + direction * 160.0).z0();
            let start =
                MapProject::ground(pointer - (axis * 90.0 + axis.perpendicular() * 4.0).z0());
            let snap = PrecisionSnap::at(&map, start, start.pos, pointer);
            let delta = snap.point - start.pos.xy();
            assert!(delta.is_close(axis * 88.0, 0.001), "{delta:?}");
        }
    }

    #[test]
    fn roadtools_precision_uses_the_projected_segment_tangent() {
        scenarios::ensure_repo_root_cwd();
        simulation::init::init();
        let mut map = Map::empty();
        assert!(map.install_environment(Environment::new(1, 1)));
        for elbow in [None, Some(vec2(120.0, 150.0))] {
            let (_, id) = map
                .make_connection(
                    MapProject::ground(vec3(45.0, 80.0, 0.0)),
                    MapProject::ground(vec3(250.0, 80.0, 0.0)),
                    elbow,
                    &LanePatternBuilder::new().build(),
                )
                .unwrap();
            let mut road = map.get(id).unwrap().clone();
            if elbow.is_none() {
                road.points = PolyLine3::new(vec![road.points().first(), road.points().last()]);
            }
            let segments = road.points().segments().collect::<Vec<_>>();
            for segment in [segments[0], *segments.last().unwrap()] {
                let midpoint = segment.src.lerp(segment.dst, 0.5);
                let (projected, tangent) = road_axis(&road, midpoint.xy());
                assert!(projected.is_close(midpoint, 0.001));
                assert!(tangent
                    .unwrap()
                    .is_close(segment.vec().xy().normalize(), 0.001));
            }
            map.remove_road(id);
        }
    }

    #[test]
    fn roadtools_precision_skips_a_closer_unaligned_road() {
        scenarios::ensure_repo_root_cwd();
        simulation::init::init();
        let mut map = Map::empty();
        assert!(map.install_environment(Environment::new(1, 1)));
        let axis = Radians::from_deg(24.0).vec2();
        let pointer = vec2(225.0, 225.0);
        let pattern = LanePatternBuilder::new().build();
        for (dir, midpoint) in [
            (Vec2::X, pointer),
            (axis, pointer + axis.perpendicular() * 20.0),
        ] {
            map.make_connection(
                MapProject::ground((midpoint - dir * 100.0).z0()),
                MapProject::ground((midpoint + dir * 100.0).z0()),
                None,
                &pattern,
            )
            .unwrap();
        }
        let start = MapProject::ground((pointer - axis * 80.0).z0());
        let snap = PrecisionSnap::at(&map, start, start.pos, pointer.z0());
        assert!(snap.point.is_close(pointer, 0.001), "{:?}", snap.point);
    }

    #[test]
    fn grid_snap_uses_terrain_at_the_snapped_position() {
        let mut map = Map::empty();
        let mut terrain = Environment::new(1, 1);
        terrain.terrain_apply(terrain.bounds(), |p| p.y * 0.4);
        assert!(map.install_environment(terrain));
        let xy = vec2(100.0, 109.0);
        let click = xy.z(map.environment().height(xy).unwrap());
        let snapped = vec2(100.0, 100.0);
        let ground = map.environment().height(snapped).unwrap();
        assert!(click.z - ground > 2.5);

        let mut state = RoadBuildResource::default();
        assert_eq!(
            state.grid_mouse_height(&map, click, 20.0),
            snapped.z(ground)
        );

        state.height_offset = 5.0;
        assert_eq!(
            state.grid_mouse_height(&map, click, 20.0),
            snapped.z(ground + 5.0)
        );

        state.height_reference = HeightReference::Start;
        state.build_state = Start(MapProject::ground(vec3(40.0, 60.0, 50.0)));
        assert_eq!(state.grid_mouse_height(&map, click, 20.0), snapped.z(55.0));
    }

    #[test]
    fn w2a_fix_grid_resamples_water_for_road_and_rail() {
        let mut terrain = Environment::new(1, 1);
        terrain.terrain_apply(terrain.bounds(), |_| 4.0);
        terrain
            .fill_lake(
                &geom::Polygon::centered_rect(vec2(240.0, 240.0), 32.0, 32.0),
                30,
                5.0,
            )
            .unwrap();
        let mut map = Map::empty();
        assert!(map.install_environment(terrain));
        for rail in [false, true] {
            let mut state = RoadBuildResource::default();
            state.pattern_builder.rail = rail;
            let inside = vec3(242.0, 240.0, 3.0);
            assert_eq!(
                state.grid_mouse_height(&map, inside, 20.0),
                vec3(240.0, 240.0, 3.0)
            );
            let edge = vec3(225.0, 240.0, 3.0);
            let dry = vec2(220.0, 240.0);
            assert!(!map.environment().is_water(dry));
            assert_eq!(
                state.grid_mouse_height(&map, edge, 20.0),
                dry.z(map.environment().height(dry).unwrap())
            );
        }
    }

    #[test]
    fn w2a_fix_attaches_to_front_deck_without_a_ground_hit() {
        scenarios::ensure_repo_root_cwd();
        simulation::init::init();
        let mut sim = Simulation::new_authored_flat(1, 0).unwrap();
        let pattern = LanePatternBuilder::default().build();
        let road_at = |sim: &mut Simulation, z| {
            sim.write::<Map>()
                .make_connection(
                    MapProject::ground(vec3(40.0, 256.0, z)),
                    MapProject::ground(vec3(500.0, 256.0, z)),
                    None,
                    &pattern,
                )
                .unwrap()
                .1
        };
        road_at(&mut sim, 16.0);
        let upper = road_at(&mut sim, 24.0);
        let ui = ui(Tool::RoadbuildStraight);
        {
            let mut input = ui.write::<InputMap>();
            input.set_world_ray(
                Some(geom::Ray3::new(
                    vec3(280.0, 256.0, 35.0),
                    vec3(1.0, 0.0, -0.1),
                )),
                &sim.map(),
                true,
            );
            assert!(input.unprojected.is_none());
            input.just_act.insert(InputAction::Select);
        }
        roadbuild(&sim, &ui);
        assert!(matches!(
            ui.read::<RoadBuildResource>().build_state,
            Start(MapProject { kind: Road(id), .. }) if id == upper
        ));
    }

    #[test]
    fn w2a_fix_oblique_free_pointer_snaps_from_the_water_hit() {
        scenarios::ensure_repo_root_cwd();
        simulation::init::init();
        let sim = Simulation::new_authored_flat(1, 0).unwrap();
        let mut terrain = Environment::new(1, 1);
        terrain.terrain_apply(terrain.bounds(), |_| 4.0);
        terrain
            .fill_lake(
                &geom::Polygon::centered_rect(vec2(240.0, 240.0), 32.0, 32.0),
                30,
                5.0,
            )
            .unwrap();
        let mut map = Map::empty();
        assert!(map.install_environment(terrain));
        *sim.write::<Map>() = map;
        for rail in [false, true] {
            let ui = ui(Tool::RoadbuildStraight);
            {
                let mut state = ui.write::<RoadBuildResource>();
                state.pattern_builder.rail = rail;
                state.snapping = Snapping::SnapToGrid;
            }
            {
                let mut input = ui.write::<InputMap>();
                input.set_world_ray(
                    Some(geom::Ray3::new(
                        vec3(192.0, 240.0, 20.0),
                        vec3(2.0, 0.0, -1.0),
                    )),
                    &sim.map(),
                    false,
                );
                assert!(input.surface_hits(sim.map().environment()).water.is_some());
                assert!(input.unprojected.unwrap().x >= 230.0);
                input.just_act.insert(InputAction::Select);
            }
            roadbuild(&sim, &ui);
            assert!(matches!(
                ui.read::<RoadBuildResource>().build_state,
                Start(MapProject { kind: Ground, pos })
                    if pos.xy() == vec2(220.0, 240.0)
            ));
        }
    }

    #[test]
    fn curved_start_reference_uses_the_selected_start_height() {
        let start = MapProject::ground(vec3(40.0, 80.0, 20.0));
        let state = RoadBuildResource {
            build_state: Curved(vec2(80.0, 140.0), start),
            height_reference: HeightReference::Start,
            height_offset: 5.0,
            ..Default::default()
        };

        assert_eq!(
            state.mouse_height(&Map::empty(), vec3(140.0, 160.0, 2.0)),
            vec3(140.0, 160.0, 25.0)
        );
    }

    #[test]
    fn curved_road_preview_matches_the_applied_endpoints() {
        let mut map = Map::empty();
        let mut terrain = Environment::new(1, 1);
        terrain.terrain_apply(terrain.bounds(), |_| 0.0);
        assert!(map.install_environment(terrain));
        let pattern = LanePatternBuilder::default().build();
        let ground = MapProject::ground;
        map.make_connection(
            ground(vec3(40.0, 80.0, 20.0)),
            ground(vec3(140.0, 80.0, 24.0)),
            None,
            &pattern,
        )
        .unwrap();
        let (intersection, _) = map
            .make_connection(
                ground(vec3(270.0, 200.0, 24.0)),
                ground(vec3(170.0, 200.0, 24.0)),
                None,
                &pattern,
            )
            .unwrap();
        let from = map.project(vec3(90.0, 80.0, 21.0), 0.0, ProjectFilter::ROAD);
        assert!(matches!(from.kind, Road(_)));
        let to = MapProject {
            pos: vec3(170.0, 200.0, 25.0),
            kind: Intersection(intersection),
        };
        let elbow = Some(vec2(110.0, 155.0));
        let preview = map
            .preview_connection(from, to, elbow, &pattern)
            .unwrap()
            .deck;
        let (_, road) = map.make_connection(from, to, elbow, &pattern).unwrap();
        let applied = map.get(road).unwrap().points();

        assert_eq!(
            (preview.first(), preview.last()),
            (applied.first(), applied.last())
        );
    }

    #[test]
    fn road_preview_matches_placement_at_a_low_terrain_following_split() {
        let mut map = Map::empty();
        let mut terrain = Environment::new(1, 1);
        terrain.terrain_apply(terrain.bounds(), |_| 1.0);
        assert!(map.install_environment(terrain));
        let pattern = LanePatternBuilder::default().build();
        let (_, original) = map
            .make_connection(
                MapProject::ground(vec3(32.0, 64.0, 1.0)),
                MapProject::ground(vec3(288.0, 64.0, 1.0)),
                None,
                &pattern,
            )
            .unwrap();
        scenarios::ensure_repo_root_cwd();
        simulation::init::init();
        let mut sim = Simulation::new_authored_flat(1, 0).unwrap();
        *sim.write::<Map>() = map;
        sim.apply_commands([&simulation::world_command::WorldCommand::Terraform {
            kind: simulation::map::TerraformKind::Level,
            center: vec2(160.0, 128.0),
            radius: 256.0,
            amount: 1.0,
            level: 0.0,
            slope: None,
        }]);
        let mut map = sim.write::<Map>();

        let points = map.get(original).unwrap().points();
        let from = MapProject {
            pos: points.project_2d(vec2(160.0, 64.0)),
            kind: Road(original),
        };
        let to = MapProject::ground(vec3(160.0, 224.0, 5.0));
        let preview = map
            .preview_connection(from, to, None, &pattern)
            .unwrap()
            .deck;
        let (_, road) = map.make_connection(from, to, None, &pattern).unwrap();

        let applied = map.get(road).unwrap().points();
        assert_eq!(preview.as_slice(), applied.as_slice());
    }

    fn ui(tool: Tool) -> UiWorld {
        crate::init::init();
        let mut ui = UiWorld::with_defaults();
        ui.insert(Camera::new(vec3(0.0, 0.0, 300.0), 1280.0, 720.0));
        *ui.write::<Tool>() = tool;
        ui
    }

    /// Run one frame of the tool with the pointer over the terrain at `point`.
    fn frame(sim: &Simulation, ui: &UiWorld, point: Vec2, click: bool) {
        let z = sim.map().environment().height(point).unwrap().max(0.0);
        {
            let mut input = ui.write::<InputMap>();
            input.unprojected = Some(point.z(z));
            input.just_act.clear();
            input.act.clear();
            if click {
                input.act.insert(InputAction::Select);
                input.just_act.insert(InputAction::Select);
            }
        }
        roadbuild(sim, ui);
    }

    fn click_through(sim: &Simulation, ui: &UiWorld, points: &[Vec2]) {
        for &point in points {
            frame(sim, ui, point, false);
            frame(sim, ui, point, true);
        }
    }

    #[test]
    fn traffic1_curved_mode_places_a_curved_road_on_the_ground() {
        let mut city = scenarios::ScenarioCtx::new(simulation::SimulationOptions {
            terrain_size: 4,
            save_replay: false,
            ..simulation::SimulationOptions::LEGACY
        });
        let ui = ui(Tool::RoadbuildCurved);
        let (start, control, end) = (vec2(700.0, 700.0), vec2(800.0, 700.0), vec2(800.0, 800.0));
        click_through(&city.sim, &ui, &[start, control, end]);

        let commands = std::mem::take(&mut *ui.commands());
        let [WorldCommand::MapMakeConnection { inter, .. }] = commands.as_ref() else {
            panic!(
                "curved placement queued {:?} (state {:?})",
                commands.as_ref(),
                ui.read::<RoadBuildResource>().build_state
            );
        };
        assert_eq!(*inter, Some(control));
        let before: Vec<_> = city.sim.map().roads().keys().collect();
        city.sim.apply_commands(commands.as_ref());
        let map = city.sim.map();
        let mut added = map.roads().values().filter(|r| !before.contains(&r.id));
        let road = added.next().expect("curved road was not built");
        assert!(added.next().is_none());
        // The deck bends towards the control point instead of joining the ends directly.
        let middle = road.points().point_along(road.points().length() * 0.5);
        assert!(middle.xy().distance(vec2(750.0, 750.0)) > 10.0);
    }

    #[test]
    fn traffic1_switching_to_curved_mode_keeps_the_chained_start_point() {
        let mut city = scenarios::ScenarioCtx::new(simulation::SimulationOptions {
            terrain_size: 4,
            save_replay: false,
            ..simulation::SimulationOptions::LEGACY
        });
        let ui = ui(Tool::RoadbuildStraight);
        let (a, b) = (vec2(600.0, 700.0), vec2(700.0, 700.0));
        click_through(&city.sim, &ui, &[a, b]);
        let straight = std::mem::take(&mut *ui.commands());
        city.sim.apply_commands(straight.as_ref());
        // The applied command chains the tool to the new road's end.
        *ui.write::<crate::uiworld::ReceivedCommands>() =
            crate::uiworld::ReceivedCommands::new(straight);
        frame(&city.sim, &ui, b, false);
        *ui.write::<crate::uiworld::ReceivedCommands>() = Default::default();
        assert!(matches!(
            ui.read::<RoadBuildResource>().build_state,
            Start(MapProject {
                kind: Intersection(_),
                ..
            })
        ));

        *ui.write::<Tool>() = Tool::RoadbuildCurved;
        let (control, end) = (vec2(800.0, 700.0), vec2(800.0, 800.0));
        click_through(&city.sim, &ui, &[control, end]);
        let commands = std::mem::take(&mut *ui.commands());
        assert!(
            matches!(
                commands.as_ref(),
                [WorldCommand::MapMakeConnection { inter: Some(p), .. }] if *p == control
            ),
            "{:?}",
            commands.as_ref()
        );

        // And back: a half-placed curve continues as a straight road from its start.
        let start = MapProject::ground(vec3(600.0, 900.0, 0.0));
        ui.write::<RoadBuildResource>().build_state = Curved(vec2(650.0, 950.0), start);
        *ui.write::<Tool>() = Tool::RoadbuildStraight;
        frame(&city.sim, &ui, vec2(700.0, 900.0), false);
        assert!(matches!(
            ui.read::<RoadBuildResource>().build_state,
            Start(selected) if selected.pos == start.pos
        ));
    }
}
