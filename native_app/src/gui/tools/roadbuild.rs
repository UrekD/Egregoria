use engine::AudioKind;
use geom::{BoldLine, BoldSpline, Camera, Line, PolyLine, Ray, ShapeEnum, Spline};
use geom::{PolyLine3, Vec2, Vec3};
use itertools::Itertools;
use ordered_float::OrderedFloat;
use simulation::map::{
    LanePatternBuilder, Map, MapProject, PointGenerateError, ProjectFilter, ProjectKind,
    PylonPosition, RoadSegmentKind,
};
use simulation::world_command::{WorldCommand, WorldCommands};
use simulation::Simulation;
use BuildState::{Curved, CurvedConnection, Hover, Start, StartCurved};
use ProjectKind::{Building, Ground, Intersection, Road};

use crate::gui::{PotentialCommands, Tool};
use crate::inputmap::{InputAction, InputMap};
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

    let grid_size = 20.0;
    let unproj = unwrap_ret!(inp.unprojected);
    let mut interpolation_points: Vec<Vec3> = Vec::new();
    let nosnapping = inp.act.contains(&InputAction::NoSnapping);

    // Prepare mousepos depending on snap to grid or snap to angle
    let mousepos = match state.snapping {
        Snapping::None => state.mouse_height(map, unproj),
        Snapping::SnapToGrid => state.grid_mouse_height(map, unproj, grid_size),
        Snapping::SnapToAngle => {
            interpolation_points = state
                .possible_interpolations(map, unproj)
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

    let mut cur_proj = if !matches!(state.build_state, CurvedConnection(..)) {
        map.project(
            mousepos,
            (log_camheight * 5.0).clamp(1.0, 10.0),
            ProjectFilter::INTER | ProjectFilter::ROAD,
        )
    } else {
        MapProject::ground(mousepos)
    };

    let patwidth = state.pattern_builder.width();

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

    if nosnapping {
        cur_proj = MapProject {
            pos: mousepos,
            kind: Ground,
        }
    }

    let is_rail = state.pattern_builder.rail;

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
                && !sp.is_steep(state.pattern_builder.width())
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
                && !sp.is_steep(state.pattern_builder.width())
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
            Some((selected_proj, cur_proj, None, state.pattern_builder.build()))
        }
        Start(selected_proj) => {
            Some((selected_proj, cur_proj, None, state.pattern_builder.build()))
        }
        CurvedConnection(src, dst) => Some((
            src,
            dst,
            Some(cur_proj.pos.xy()),
            state.pattern_builder.build(),
        )),

        Curved(interpoint, selected_proj) => {
            let inter = Some(interpoint);
            Some((
                selected_proj,
                cur_proj,
                inter,
                state.pattern_builder.build(),
            ))
        }
        _ => None,
    };
    potential_command.0.clear();

    let mut points = None;

    if let Some((src, dst, inter, pat)) = build_args {
        if let Some((p, err)) = connection_preview(map, src, dst, inter, is_rail) {
            potential_command.set(WorldCommand::MapMakeConnection {
                from: src,
                to: dst,
                inter,
                pat,
            });
            points = Some(p);
            is_valid &= err.is_none();
        } else {
            is_valid = false;
        }
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
                state.build_state = Curved(mousepos.xy(), v);
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

#[derive(Default)]
pub struct RoadBuildResource {
    pub build_state: BuildState,
    pub pattern_builder: LanePatternBuilder,
    pub snapping: Snapping,
    pub height_offset: f32,
    pub height_reference: HeightReference,
    suppress_chaining: bool,
}

impl RoadBuildResource {
    fn grid_mouse_height(&self, map: &Map, unprojected: Vec3, grid_size: f32) -> Vec3 {
        let snapped = unprojected.xy().snap(grid_size, grid_size);
        let height = map.environment.height(snapped).unwrap_or(unprojected.z);
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

fn connection_preview(
    map: &Map,
    src: MapProject,
    dst: MapProject,
    inter: Option<Vec2>,
    is_rail: bool,
) -> Option<(PolyLine3, Option<PointGenerateError>)> {
    let src = src.road_connection_position(map)?;
    let dst = dst.road_connection_position(map)?;
    let segment = match inter {
        Some(x) => RoadSegmentKind::from_elbow(src.xy(), dst.xy(), x),
        None => RoadSegmentKind::Straight,
    };

    Some(simulation::map::Road::generate_points(
        src,
        dst,
        segment,
        is_rail,
        &map.environment,
    ))
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
        } in simulation::map::Road::pylons_positions(&p, &map.environment)
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

    pub fn possible_interpolations(&self, map: &Map, mousepos: Vec3) -> Option<Vec<Vec3>> {
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
                            let h = map.environment.height(p)?;
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

                            let h = map.environment.height(p)?;

                            Some(p.z(h))
                        })
                        .collect::<Vec<_>>(),
                )
            }

            (Intersection(inter_id), Road(road_id)) | (Road(road_id), Intersection(inter_id))
                if self.pattern_builder.rail =>
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
                            let h = map.environment.height(p)?;

                            Some(p.z(h))
                        })
                        .collect(),
                )
            }

            (Road(id), Ground) | (Ground, Road(id)) if self.pattern_builder.rail => {
                let road = map.roads().get(id)?;

                let pos = if start.kind == Road(id) {
                    start.pos
                } else {
                    end.pos
                };
                let (pos, _, dir) = road.points().project_segment_dir(pos);

                let line = Line::new_dir(pos.xy(), dir.xy());
                let p = line.project(mousepos.xy());

                let h = map.environment.height(p)?;
                Some(vec![p.z(h)])
            }
            (Road(id0), Road(id1)) if self.pattern_builder.rail => {
                let road0 = map.roads().get(id0)?;
                let road1 = map.roads().get(id1)?;

                let (pos0, _, dir0) = road0.points().project_segment_dir(start.pos);
                let (pos1, _, dir1) = road1.points().project_segment_dir(end.pos);

                let line0 = Line::new_dir(pos0.xy(), dir0.xy());
                let line1 = Line::new_dir(pos1.xy(), dir1.xy());

                let p = line0.intersection_point(&line1)?;
                let h = map.environment.height(p)?;

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
    fn grid_snap_uses_terrain_at_the_snapped_position() {
        let mut map = Map::empty();
        map.environment = Environment::new(1, 1);
        map.environment
            .terrain_apply(map.environment.bounds(), |p| p.y * 0.4);
        let xy = vec2(100.0, 109.0);
        let click = xy.z(map.environment.height(xy).unwrap());
        let snapped = vec2(100.0, 100.0);
        let ground = map.environment.height(snapped).unwrap();
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
        map.environment = Environment::new(1, 1);
        map.environment
            .terrain_apply(map.environment.bounds(), |_| 0.0);
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
        let (preview, _) = connection_preview(&map, from, to, elbow, false).unwrap();
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
        map.environment = Environment::new(1, 1);
        map.environment
            .terrain_apply(map.environment.bounds(), |_| 1.0);
        let pattern = LanePatternBuilder::default().build();
        let (_, original) = map
            .make_connection(
                MapProject::ground(vec3(32.0, 64.0, 1.0)),
                MapProject::ground(vec3(288.0, 64.0, 1.0)),
                None,
                &pattern,
            )
            .unwrap();
        map.environment
            .terrain_apply(map.environment.bounds(), |_| 0.0);

        let points = map.get(original).unwrap().points();
        let from = MapProject {
            pos: points.project_2d(vec2(160.0, 64.0)),
            kind: Road(original),
        };
        let to = MapProject::ground(vec3(160.0, 224.0, 5.0));
        let (preview, error) = connection_preview(&map, from, to, None, false).unwrap();
        assert!(error.is_none());
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
        let z = sim.map().environment.height(point).unwrap().max(0.0);
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
