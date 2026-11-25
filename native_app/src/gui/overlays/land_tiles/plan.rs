use std::{collections::BTreeSet, rc::Rc};

use simulation::map::{
    LandTileId, LaneKind, Map, ProjectKind, Road, TileExtent, TileStatus, LAND_TILE_SIZE,
};
use yakui::{
    event::{EventInterest, EventResponse, WidgetEvent},
    input::MouseButton,
    paint::{PaintDom, PaintMesh, PaintRect, Vertex},
    widget::{EventContext, LayoutContext, PaintContext, Widget},
    Color, Constraints, Rect, Vec2,
};

const MAX_PLAN_SIDE: f32 = 440.0;
const MAX_ROAD_SEGMENTS: usize = 8_192;
const MAX_APPROACH_MARKERS: usize = 256;

#[derive(Clone, Copy, Debug)]
pub(super) struct PlanTransform {
    pub extent: TileExtent,
    pub size: Vec2,
}

impl PlanTransform {
    pub fn new(extent: TileExtent, available: Vec2) -> Self {
        let counts = Vec2::new(f32::from(extent.width), f32::from(extent.height));
        let pitch = (available / counts).min_element();
        Self {
            extent,
            size: counts * pitch,
        }
    }

    pub fn tile_at(self, point: Vec2) -> Option<LandTileId> {
        if !point.is_finite() || point.cmplt(Vec2::ZERO).any() || point.cmpge(self.size).any() {
            return None;
        }
        let tile = point / self.pitch();
        Some(LandTileId {
            x: (tile.x as u16).min(self.extent.width - 1),
            y: self.extent.height - 1 - (tile.y as u16).min(self.extent.height - 1),
        })
    }

    fn pitch(self) -> f32 {
        self.size.x / f32::from(self.extent.width)
    }

    fn point(self, world: Vec2) -> Vec2 {
        Vec2::new(world.x * self.pitch(), self.size.y - world.y * self.pitch())
    }

    fn tile_origin(self, tile: LandTileId) -> Vec2 {
        self.point(Vec2::new(f32::from(tile.x), f32::from(tile.y) + 1.0))
    }
}

#[derive(Debug)]
pub(super) struct MapGeometry {
    extent: TileExtent,
    water: Vec<(Vec2, Vec2)>,
    routes: Vec<RouteSegment>,
    approaches: Vec<Vec2>,
}

#[derive(Debug)]
struct RouteSegment {
    from: Vec2,
    to: Vec2,
    color: Color,
    width: f32,
    rail_core: bool,
}

pub(super) const WATER: Color = Color::rgb(51, 137, 205);
pub(super) const ROAD: Color = Color::rgb(215, 223, 233);
pub(super) const RAIL: Color = Color::rgb(171, 151, 241);
pub(super) const PROTECTED: Color = Color::rgb(255, 207, 112);
pub(super) const APPROACH: Color = Color::rgb(93, 231, 225);

impl MapGeometry {
    pub fn build(map: &Map, extent: TileExtent) -> Self {
        let env = map.environment();
        let land = map.land_ownership();
        let world_point = |p: geom::Vec2| Vec2::new(p.x, p.y) / LAND_TILE_SIZE;

        // Rasterize any wet cell into a coarser grid and merge horizontal runs.
        // This keeps narrow rivers visible at the full-world scale.
        let resolution = if extent.width.max(extent.height) <= 32 {
            8
        } else {
            4
        };
        let width = usize::from(extent.width) * resolution;
        let height = usize::from(extent.height) * resolution;
        let mut wet = vec![false; width * height];
        if env.has_water_configuration() {
            for (chunk, _) in env.chunks() {
                for cell in env.water_tiles(chunk) {
                    let point = world_point(cell.tile.center()) * resolution as f32;
                    let (x, y) = (point.x as usize, point.y as usize);
                    if x < width && y < height {
                        wet[y * width + x] = true;
                    }
                }
            }
        }
        let mut water = Vec::new();
        let step = 1.0 / resolution as f32;
        for (y, row) in wet.chunks_exact(width).enumerate() {
            let mut x = 0;
            while let Some(wet) = row.get(x) {
                if !wet {
                    x += 1;
                    continue;
                }
                let len = row[x..].iter().take_while(|&&wet| wet).count();
                water.push((
                    Vec2::new(x as f32, (y + 1) as f32) * step,
                    Vec2::new(len as f32, 1.0) * step,
                ));
                x += len;
            }
        }

        let mut roads = Vec::new();
        let mut rail = Vec::new();
        let mut protected_routes = Vec::new();
        let mut approaches = Vec::new();
        let mut marked = BTreeSet::new();
        for road in map.roads().values() {
            let protected = land.is_protected(ProjectKind::Road(road.id));
            let is_rail = road.lanes_iter().any(|(_, kind)| kind == LaneKind::Rail);
            if protected {
                protected_routes.push((road, is_rail));
            } else if is_rail {
                rail.push((road, false));
            } else {
                roads.push((road, false));
            }
            if !protected && road.has_sidewalks() && approaches.len() < MAX_APPROACH_MARKERS {
                for junction in [road.src, road.dst] {
                    if !land.is_protected(ProjectKind::Intersection(junction))
                        && marked.insert(junction)
                    {
                        if let Some(point) = map.get(junction) {
                            approaches.push(world_point(point.pos.xy()));
                        }
                    }
                }
            }
        }
        // Draw the protected trunk above intersecting routes. Its share includes
        // the second stroke drawn down the center of protected rail segments.
        let mut routes = Vec::new();
        append_routes(&mut routes, &roads, MAX_ROAD_SEGMENTS / 2, 8, ROAD, 1.2);
        append_routes(&mut routes, &rail, MAX_ROAD_SEGMENTS / 4, 8, RAIL, 1.7);
        append_routes(
            &mut routes,
            &protected_routes,
            MAX_ROAD_SEGMENTS / 8,
            32,
            PROTECTED,
            2.3,
        );
        Self {
            extent,
            water,
            routes,
            approaches,
        }
    }

    pub fn approaches_in(&self, tile: LandTileId) -> usize {
        let corner = Vec2::new(f32::from(tile.x), f32::from(tile.y));
        self.approaches
            .iter()
            .filter(|point| point.cmpge(corner).all() && point.cmplt(corner + Vec2::ONE).all())
            .count()
    }
}

fn append_routes(
    routes: &mut Vec<RouteSegment>,
    roads: &[(&Road, bool)],
    budget: usize,
    detail: usize,
    color: Color,
    width: f32,
) {
    let count = roads.len().min(budget);
    if count == 0 {
        return;
    }
    let per_road = (budget / count).min(detail);
    for i in 0..count {
        let (road, rail_core) = roads[i * roads.len() / count];
        let Some((first, rest)) = road.points().as_slice().split_first() else {
            continue;
        };
        let stride = rest.len().div_ceil(per_road).max(1);
        let world_point = |p: geom::Vec3| Vec2::new(p.x, p.y) / LAND_TILE_SIZE;
        let mut from = world_point(*first);
        for to in rest.chunks(stride).filter_map(|chunk| chunk.last()) {
            let to = world_point(*to);
            if from.distance_squared(to) > f32::EPSILON {
                routes.push(RouteSegment {
                    from,
                    to,
                    color,
                    width,
                    rail_core,
                });
            }
            from = to;
        }
    }
}

pub(super) fn status_color(status: TileStatus) -> Color {
    match status {
        TileStatus::Owned => Color::rgb(32, 114, 94),
        TileStatus::Buyable => Color::rgb(129, 167, 79),
        TileStatus::Locked { .. } => Color::rgb(131, 90, 151),
        TileStatus::InsufficientFunds => Color::rgb(165, 101, 51),
        TileStatus::NeedsAdjacent | TileStatus::Unavailable => Color::rgb(48, 61, 75),
    }
}

#[derive(Debug)]
pub(super) struct Plan {
    pub geometry: Rc<MapGeometry>,
    pub statuses: Rc<Vec<TileStatus>>,
    pub selected: Option<LandTileId>,
}

#[derive(Debug, Default)]
pub(super) struct PlanWidget {
    props: Option<Plan>,
    pressed: Option<LandTileId>,
    clicked: Option<LandTileId>,
}

impl Widget for PlanWidget {
    type Props<'a> = Plan;
    type Response = Option<LandTileId>;

    fn new() -> Self {
        Self::default()
    }

    fn update(&mut self, props: Plan) -> Self::Response {
        self.props = Some(props);
        self.clicked.take()
    }

    fn layout(&self, ctx: LayoutContext<'_>, constraints: Constraints) -> Vec2 {
        ctx.layout.enable_clipping(ctx.dom);
        let available = constraints.max.min(Vec2::splat(MAX_PLAN_SIDE));
        PlanTransform::new(self.props.as_ref().unwrap().geometry.extent, available).size
    }

    fn event_interest(&self) -> EventInterest {
        EventInterest::MOUSE_INSIDE | EventInterest::MOUSE_OUTSIDE
    }

    fn event(&mut self, ctx: EventContext<'_>, event: &WidgetEvent) -> EventResponse {
        let WidgetEvent::MouseButtonChanged {
            button: MouseButton::One,
            down,
            inside,
            position,
            ..
        } = *event
        else {
            return EventResponse::Bubble;
        };
        let rect = ctx.layout.get(ctx.dom.current()).unwrap().rect;
        let transform =
            PlanTransform::new(self.props.as_ref().unwrap().geometry.extent, rect.size());
        let tile = inside
            .then(|| transform.tile_at(position - rect.pos()))
            .flatten();
        if down {
            self.pressed = tile;
        } else if self.pressed.take() == tile {
            self.clicked = tile;
        }
        if inside {
            EventResponse::Sink
        } else {
            EventResponse::Bubble
        }
    }

    fn paint(&self, ctx: PaintContext<'_>) {
        let props = self.props.as_ref().unwrap();
        let geometry = &props.geometry;
        let rect = ctx.layout.get(ctx.dom.current()).unwrap().rect;
        let plan = PlanTransform::new(geometry.extent, rect.size());
        let pitch = plan.pitch();
        paint_rect(ctx.paint, rect.pos(), plan.size, Color::rgb(22, 31, 42));
        for (tile, &status) in geometry.extent.tiles().zip(props.statuses.iter()) {
            paint_rect(
                ctx.paint,
                rect.pos() + plan.tile_origin(tile),
                Vec2::splat(pitch),
                status_color(status),
            );
        }
        for &(origin, size) in &geometry.water {
            paint_rect(
                ctx.paint,
                rect.pos() + plan.point(origin),
                size * pitch,
                WATER.with_alpha(0.78),
            );
        }
        let grid = Color::rgba(17, 25, 36, 190);
        for x in 1..geometry.extent.width {
            let pos = rect.pos() + Vec2::new(f32::from(x) * pitch, 0.0);
            paint_rect(ctx.paint, pos, Vec2::new(1.0, plan.size.y), grid);
        }
        for y in 1..geometry.extent.height {
            let pos = rect.pos() + Vec2::new(0.0, f32::from(y) * pitch);
            paint_rect(ctx.paint, pos, Vec2::new(plan.size.x, 1.0), grid);
        }
        for route in &geometry.routes {
            let from = rect.pos() + plan.point(route.from);
            let to = rect.pos() + plan.point(route.to);
            paint_route(ctx.paint, from, to, route.width, route.color);
            if route.rail_core {
                paint_route(ctx.paint, from, to, 1.1, RAIL);
            }
        }
        for &point in &geometry.approaches {
            paint_rect(
                ctx.paint,
                rect.pos() + plan.point(point) - Vec2::splat(1.5),
                Vec2::splat(3.0),
                APPROACH,
            );
        }
        if let Some(tile) = props.selected {
            let origin = plan.tile_origin(tile);
            let edge = pitch - 1.0;
            let width = 2.0;
            for (offset, size) in [
                (Vec2::ZERO, Vec2::new(edge, width)),
                (Vec2::ZERO, Vec2::new(width, edge)),
                (Vec2::new(0.0, edge - width), Vec2::new(edge, width)),
                (Vec2::new(edge - width, 0.0), Vec2::new(width, edge)),
            ] {
                paint_rect(ctx.paint, rect.pos() + origin + offset, size, Color::WHITE);
            }
        }
    }
}

fn paint_route(paint: &mut PaintDom, from: Vec2, to: Vec2, width: f32, color: Color) {
    let normal = (to - from).normalize().perp() * width * 0.5;
    let color = color.to_linear();
    let vertices = [from + normal, from - normal, to - normal, to + normal]
        .map(|pos| Vertex::new(pos, Vec2::ZERO, color));
    paint.add_mesh(PaintMesh::new(vertices, [0, 1, 2, 0, 2, 3]));
}

fn paint_rect(paint: &mut PaintDom, pos: Vec2, size: Vec2, color: Color) {
    let mut rect = PaintRect::new(Rect::from_pos_size(pos, size));
    rect.color = color;
    rect.add(paint);
}
