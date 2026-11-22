use geom::{LinearColor, Vec3};
use goryak::{
    blur_bg, fixed_spacer, mincolumn, minrow, on_secondary_container, padxy, secondary_container,
    textc,
};
use prototypes::Tick;
use simulation::map::{
    LaneKind, Map, PathKind, Pathfinder, Traversable, TraverseDirection, TraverseKind,
};
use simulation::map_dynamic::Transit;
use yakui::{colored_box_container, reflow, Alignment, Color, Dim2, Pivot};

use crate::rendering::immediate::ImmediateDraw;

pub(super) fn invalidation_key(transit: &Transit) -> u64 {
    transit.revision
}

pub(super) fn draw(map: &Map, transit: &Transit, draw: &mut ImmediateDraw) {
    for line in transit.lines.values() {
        for (&from, &to) in line.stops.iter().zip(line.stops.iter().cycle().skip(1)) {
            let (Some(from), Some(to)) = (transit.stops.get(from), transit.stops.get(to)) else {
                continue;
            };
            let mut points =
                leg_points(map, from.berth, to.berth).unwrap_or_else(|| vec![from.berth, to.berth]);
            points.dedup_by(|a, b| a.distance2(*b) < 0.01);
            if points.len() >= 2 {
                for point in &mut points {
                    *point = point.up(0.65);
                }
                draw.polyline(points, 4.0, false)
                    .color(line.color)
                    .persistent();
            }
        }
    }

    for stop in transit.stops.values() {
        draw.circle(stop.walk_pos.up(0.85), 4.0)
            .color(LinearColor::WHITE)
            .persistent();
        draw.circle(stop.walk_pos.up(0.9), 2.4)
            .color(LinearColor::BLACK)
            .persistent();
    }
}

fn leg_points(map: &Map, from: Vec3, to: Vec3) -> Option<Vec<Vec3>> {
    // Each berth lies on the driving lane it serves. A fixed tick keeps the
    // display stable across rebuilds of the same network.
    let start = map.nearest_lane(from, LaneKind::Driving, Some(4.0))?;
    let end = map.nearest_lane(to, LaneKind::Driving, Some(4.0))?;
    if start == end {
        if let Some(path) = PathKind::Vehicle.local_route(map, start, from, to) {
            return Some(path.into_vec());
        }
    }
    let start = Traversable::new(TraverseKind::Lane(start), TraverseDirection::Forward);
    let route = PathKind::Vehicle.path(map, Tick(0), start, end)?;
    let last = route.len().checked_sub(1)?;
    let mut points = Vec::new();
    for (index, traversable) in route.into_iter().enumerate() {
        let path = traversable.points(map)?;
        let trim_start = if index == 0 {
            path.length_at_proj(from)
        } else {
            0.0
        };
        let trim_end = if index == last {
            path.length() - path.length_at_proj(to)
        } else {
            0.0
        };
        points.extend(path.cut(trim_start, trim_end).into_vec());
    }
    Some(points)
}

pub(super) fn legend(empty: bool) {
    reflow(
        Alignment::TOP_LEFT,
        Pivot::TOP_LEFT,
        Dim2::pixels(12.0, 112.0),
        || {
            blur_bg(secondary_container().with_alpha(0.94), 8.0, || {
                padxy(10.0, 6.0, || {
                    mincolumn(4.0, || {
                        let color = on_secondary_container();
                        textc(color, "Transit");
                        if empty {
                            textc(color, "No transit routes or stops to display");
                            return;
                        }
                        textc(color, "Routes use their line colours");
                        minrow(6.0, || {
                            colored_box_container(Color::WHITE, || {
                                fixed_spacer((8.0, 8.0));
                            });
                            textc(color, "Bus stop");
                        });
                    });
                });
            });
        },
    );
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::{overlays::OverlayMode, Tool};
    use crate::uiworld::UiWorld;
    use geom::{vec2, vec3, Color};
    use scenarios::ScenarioCtx;
    use simulation::map_dynamic::{TransitLine, TransitStop};
    use simulation::SimulationOptions;

    #[test]
    fn p3d_overlay_invalidation_key_tracks_paused_stop_and_line_edits() {
        let test = ScenarioCtx::new(SimulationOptions {
            terrain_size: 1,
            save_replay: false,
            ..SimulationOptions::LEGACY
        });
        let tick = test.sim.get_tick();
        let mut uiworld = UiWorld::default();
        uiworld.insert(Tool::Hand);
        uiworld.insert(OverlayMode::Transit);
        uiworld.insert(ImmediateDraw::default());
        let draw_overlay = || {
            super::super::draw(&test.sim, &uiworld);
            uiworld.read::<ImmediateDraw>().persistent_orders.len()
        };
        assert_eq!(draw_overlay(), 0);
        let mut transit = test.sim.write::<Transit>();
        let initial = invalidation_key(&transit);
        let stops = [10.0, 30.0].map(|x| {
            transit.stops.insert_with_key(|id| TransitStop {
                id,
                pos: vec3(x, 20.0, 0.0),
                dir: vec2(1.0, 0.0),
                walk_pos: vec3(x, 25.0, 0.0),
                berth: vec3(x, 20.0, 0.0),
            })
        });
        transit.revision += 1;
        let after_stop = invalidation_key(&transit);
        assert_ne!(initial, after_stop);
        drop(transit);
        assert_eq!(draw_overlay(), 4);
        assert_eq!(draw_overlay(), 4);

        let mut transit = test.sim.write::<Transit>();
        let line = transit.lines.insert_with_key(|id| TransitLine {
            id,
            stops: stops.to_vec(),
            target_vehicles: 0,
            fare: Default::default(),
            color: Color::BLUE,
            suspended: false,
        });
        transit.revision += 1;
        assert_ne!(after_stop, invalidation_key(&transit));
        drop(transit);
        assert_eq!(draw_overlay(), 6);

        let mut transit = test.sim.write::<Transit>();
        transit.lines.remove(line);
        transit.revision += 1;
        drop(transit);
        assert_eq!(draw_overlay(), 4);
        assert_eq!(test.sim.get_tick(), tick);
    }
}
