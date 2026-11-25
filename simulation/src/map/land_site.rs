use geom::{vec2, BaseNode, Polygon, Shape, Vec2, Vec3, AABB, OBB};
use prototypes::BuildingGen;

use super::{
    base_node_support, Building, BuildingID, IntersectionID, LandDenial, LandTileId, LotID, Map,
    ProjectFilter, ProjectKind, RoadID, Zone,
};

pub(crate) enum PreparedEffects<'a> {
    Point(Vec2),
    Footprint(&'a [Vec2]),
    BaseNode(BaseNode),
    Road(RoadID),
    Junction(IntersectionID),
    Building(BuildingID),
    Lot(LotID),
}

impl Map {
    /// Land authority for resolved effects. Environmental and price checks remain
    /// in the callers' existing planners.
    pub(crate) fn authorize_land_effects(
        &self,
        effects: PreparedEffects<'_>,
    ) -> Result<(), LandDenial> {
        if !self.land_ownership().is_tiled() {
            return Ok(());
        }
        use PreparedEffects::*;
        match effects {
            Point(point) => {
                self.land_ownership().check_point(point)?;
                if self
                    .spatial_map
                    .query(
                        point,
                        ProjectFilter::ROAD | ProjectFilter::INTER | ProjectFilter::BUILDING,
                    )
                    .any(|object| self.land_ownership().is_protected(object))
                {
                    return Err(LandDenial::ProtectedGateway);
                }
                Ok(())
            }
            Footprint(points) => self.authorize_land_footprint(points, None),
            BaseNode(node) => {
                let bounds = base_node_support(node, self);
                self.authorize_land_footprint(&rect_corners(bounds), None)
            }
            Road(id) => {
                self.mutable_object(id.into())?;
                let road = self.get(id).ok_or(LandDenial::RequiresMutableAccess)?;
                for triangle in super::land_deck_triangles(road) {
                    self.authorize_land_effects(Footprint(&triangle.map(Vec3::xy)))?;
                }
                Ok(())
            }
            Junction(id) => {
                self.mutable_object(id.into())?;
                let inter = self.get(id).ok_or(LandDenial::RequiresMutableAccess)?;
                self.land_ownership().check_point(inter.pos.xy())
            }
            Building(id) => {
                self.mutable_object(id.into())?;
                let b = self.get(id).ok_or(LandDenial::RequiresMutableAccess)?;
                self.authorize_site(
                    &b.obb,
                    b.zone.as_ref(),
                    b.connected_road,
                    b.door_pos.xy(),
                    None,
                )
            }
            Lot(id) => {
                let lot = self.get(id).ok_or(LandDenial::RequiresMutableAccess)?;
                self.authorize_frontage(lot.parent, &lot.shape, None)
            }
        }
    }

    pub(super) fn mutable_object(&self, object: ProjectKind) -> Result<(), LandDenial> {
        if self.land_ownership().is_protected(object) {
            return Err(LandDenial::ProtectedGateway);
        }
        Ok(())
    }

    pub(super) fn authorize_land_footprint(
        &self,
        points: &[Vec2],
        purchased: Option<LandTileId>,
    ) -> Result<(), LandDenial> {
        self.land_ownership().check_polygon(points, purchased)?;
        let bounds = Polygon::from(points).bbox();
        for object in self.spatial_map.construction_candidates(
            bounds,
            ProjectFilter::ROAD | ProjectFilter::INTER | ProjectFilter::BUILDING,
        ) {
            if !self.land_ownership().is_protected(object) {
                continue;
            }
            let overlaps = match object {
                ProjectKind::Road(id) => super::land_deck_triangles(&self.roads[id])
                    .any(|triangle| polygons_overlap(points, &triangle.map(|p| p.xy()))),
                ProjectKind::Intersection(id) => {
                    let outline = self
                        .land_junction_outline(id)
                        .map_err(|_| LandDenial::WorkLimit)?;
                    polygons_overlap(points, outline.as_slice())
                }
                ProjectKind::Building(id) => {
                    let building = &self.buildings[id];
                    polygons_overlap(points, &building.obb.corners)
                        || building
                            .zone
                            .as_ref()
                            .is_some_and(|zone| polygons_overlap(points, zone.poly.as_slice()))
                }
                _ => unreachable!("a protected root is a road, junction, or building"),
            };
            if overlaps {
                return Err(LandDenial::ProtectedGateway);
            }
        }
        Ok(())
    }

    pub(super) fn authorize_site(
        &self,
        shape: &OBB,
        zone: Option<&Zone>,
        access: Option<RoadID>,
        door: Vec2,
        purchased: Option<LandTileId>,
    ) -> Result<(), LandDenial> {
        if !self.land_ownership().is_tiled() {
            return Ok(());
        }
        self.authorize_land_footprint(&shape.corners, purchased)?;
        if let Some(zone) = zone {
            self.authorize_zone_polygon(zone)?;
        }
        if purchased.is_none() {
            self.authorize_land_effects(PreparedEffects::Point(door))?;
        }
        if let Some(road) = access {
            self.mutable_object(road.into())?;
            let road = self.get(road).ok_or(LandDenial::RequiresMutableAccess)?;
            let street = road.points().project_2d(door).xy();
            self.authorize_connector(door, street, 1.5, purchased)?;
        }
        Ok(())
    }

    pub(super) fn authorize_connector(
        &self,
        from: Vec2,
        to: Vec2,
        radius: f32,
        purchased: Option<LandTileId>,
    ) -> Result<(), LandDenial> {
        let segment = to - from;
        let Some(axis) = segment.try_normalize() else {
            return self.authorize_land_effects(PreparedEffects::Point(from));
        };
        let path = OBB::new((from + to) * 0.5, axis, segment.mag(), radius * 2.0);
        self.authorize_land_footprint(&path.corners, purchased)
    }

    pub(in crate::map) fn authorize_frontage(
        &self,
        road: RoadID,
        shape: &OBB,
        purchased: Option<LandTileId>,
    ) -> Result<(), LandDenial> {
        let front = (shape.corners[0] + shape.corners[1]) * 0.5;
        self.authorize_site(shape, None, Some(road), front, purchased)
    }

    fn authorize_zone_polygon(&self, zone: &Zone) -> Result<(), LandDenial> {
        let points = zone.poly.as_slice();
        if points.len() > 256 {
            return Err(LandDenial::WorkLimit);
        }
        if points.len() < 3 || points.iter().any(|point| !point.is_finite()) {
            return Err(LandDenial::OutsideMap);
        }
        use geom::Intersect;
        let edges: Vec<_> = zone.poly.segments().collect();
        for (i, edge) in edges.iter().enumerate() {
            if edge.src == edge.dst
                || edges
                    .iter()
                    .enumerate()
                    .skip(i + 2)
                    .any(|(j, other)| (i != 0 || j + 1 != edges.len()) && edge.intersects(other))
            {
                return Err(LandDenial::OutsideMap);
            }
        }
        self.authorize_land_effects(PreparedEffects::Footprint(points))
    }

    pub(crate) fn authorize_new_building(
        &self,
        shape: &OBB,
        gen: BuildingGen,
        zone: Option<&Zone>,
        road: Option<RoadID>,
    ) -> Result<(), LandDenial> {
        if !self.land_ownership().is_tiled() {
            return Ok(());
        }
        let door = Building::door_position(shape, gen, self.environment());
        self.authorize_site(shape, zone, road, door.xy(), None)?;
        self.authorize_building_cut(shape)?;
        if let Some(zone) = zone {
            self.authorize_displaced_lots(zone.poly.clone())?;
        } else {
            self.authorize_displaced_lots(*shape)?;
        }
        Ok(())
    }

    pub(crate) fn authorize_zone_change(
        &self,
        id: BuildingID,
        zone: &Zone,
    ) -> Result<(), LandDenial> {
        self.authorize_land_effects(PreparedEffects::Building(id))?;
        if self.land_ownership().is_tiled() {
            self.authorize_zone_polygon(zone)?;
            self.authorize_displaced_lots(zone.poly.clone())?;
        }
        Ok(())
    }

    pub(super) fn authorize_displaced_lots(
        &self,
        shape: impl geom::Intersect<geom::ShapeEnum> + geom::Intersect<AABB> + Clone,
    ) -> Result<(), LandDenial> {
        if self.land_ownership().is_tiled() {
            for object in self.spatial_map.query(shape, ProjectFilter::LOT) {
                if let ProjectKind::Lot(id) = object {
                    self.authorize_land_effects(PreparedEffects::Lot(id))?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn authorize_building_cut(&self, shape: &OBB) -> Result<(), LandDenial> {
        if self.land_ownership().is_tiled() {
            let cut = shape.expand(25.0);
            let mut remaining = super::MAX_WATER_BATCH_WORK;
            self.authorize_cut_nodes(cut.bbox(), 1, &mut remaining, |point| cut.contains(point))?;
        }
        Ok(())
    }

    pub(super) fn authorize_cut_nodes(
        &self,
        bounds: AABB,
        units_per_node: usize,
        remaining: &mut usize,
        changes: impl Fn(Vec2) -> bool,
    ) -> Result<(), LandDenial> {
        let pitch = super::terrain::CELL_SIZE;
        let (width, height) = self.environment().size();
        let resolution = super::TERRAIN_CHUNK_RESOLUTION;
        let min = (bounds.ll / pitch).ceil().max(Vec2::ZERO);
        let max = (bounds.ur / pitch).floor().min(vec2(
            (usize::from(width) * resolution - 1) as f32,
            (usize::from(height) * resolution - 1) as f32,
        ));
        if min.x > max.x || min.y > max.y {
            return Ok(());
        }
        let work = (1 + (max.x - min.x) as usize)
            .saturating_mul(1 + (max.y - min.y) as usize)
            .saturating_mul(units_per_node);
        super::spend_water_work(remaining, work).map_err(|_| LandDenial::WorkLimit)?;
        for y in min.y as usize..=max.y as usize {
            for x in min.x as usize..=max.x as usize {
                let point = vec2(x as f32, y as f32) * pitch;
                if !self.environment().is_wet_base_node((x, y)) && changes(point) {
                    self.authorize_land_effects(PreparedEffects::BaseNode((x, y)))?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn authorize_player_train(
        &self,
        wagons: &[prototypes::RollingStockID],
        lane: &super::Lane,
        nose: f32,
    ) -> Result<(), crate::world_command::Rejection> {
        use crate::world_command::Rejection;
        if !self.land_ownership().is_tiled() {
            return Ok(());
        }
        self.mutable_object(lane.parent.into())?;
        let tail = nose - crate::transportation::train::train_length(wagons);
        if tail < 0.0 || nose > lane.points.length() || !tail.is_finite() {
            return Err(Rejection::InvalidTrain);
        }
        let mut previous = lane.points.point_along(tail).xy();
        let mut distance = 0.0;
        for segment in lane.points.segments() {
            distance += segment.src.distance(segment.dst);
            if distance <= tail {
                continue;
            }
            let next = if distance < nose {
                segment.dst
            } else {
                lane.points.point_along(nose)
            };
            self.authorize_connector(previous, next.xy(), 2.5, None)?;
            if distance >= nose {
                break;
            }
            previous = next.xy();
        }
        Ok(())
    }

    pub(crate) fn authorize_transit_site(
        &self,
        anchor: Vec3,
        walk: Vec3,
        berth: Vec3,
        axis: Vec2,
        road: RoadID,
    ) -> Result<(), LandDenial> {
        if !self.land_ownership().is_tiled() {
            return Ok(());
        }
        self.mutable_object(road.into())?;
        self.authorize_land_effects(PreparedEffects::Point(anchor.xy()))?;
        self.authorize_connector(walk.xy(), berth.xy(), 1.5, None)?;
        let footprint = OBB::new(berth.xy(), axis, 30.0, 4.0);
        self.authorize_land_effects(PreparedEffects::Footprint(&footprint.corners))
    }
}

pub(super) fn rect_corners(bounds: AABB) -> [Vec2; 4] {
    [
        bounds.ll,
        vec2(bounds.ur.x, bounds.ll.y),
        bounds.ur,
        vec2(bounds.ll.x, bounds.ur.y),
    ]
}

// A proper boundary crossing or an interior vertex proves positive-area
// overlap. Collinear boundaries also overlap when their interiors face the
// same side. A shared edge with interiors on opposite sides is just contact.
fn polygons_overlap(a: &[Vec2], b: &[Vec2]) -> bool {
    use geom::Vec2d;
    let point = |p: Vec2| Vec2d::new(f64::from(p.x), f64::from(p.y));
    let edges = |points: &[Vec2]| {
        (0..points.len())
            .map(|i| (points[i], points[(i + 1) % points.len()]))
            .collect::<Vec<_>>()
    };
    if a.len() < 3 || b.len() < 3 {
        return false;
    }
    let a_edges = edges(a);
    let b_edges = edges(b);
    let area = |edges: &[(Vec2, Vec2)]| {
        edges
            .iter()
            .map(|&(a, b)| point(a).perp_dot(point(b)))
            .sum::<f64>()
    };
    let a_area = area(&a_edges);
    let b_area = area(&b_edges);
    if a_area == 0.0 || b_area == 0.0 {
        return false;
    }
    let interior_vertex = |vertices: &[Vec2], boundary: &[(Vec2, Vec2)], polygon: &[Vec2]| {
        let polygon = Polygon::from(polygon);
        let bounds = polygon.bbox();
        vertices.iter().copied().any(|vertex| {
            bounds.contains(vertex)
                && polygon.contains(vertex)
                && !boundary.iter().any(|&(start, end)| {
                    let along = point(end) - point(start);
                    let offset = point(vertex) - point(start);
                    along.perp_dot(offset) == 0.0
                        && (0.0..=along.dot(along)).contains(&along.dot(offset))
                })
        })
    };
    if interior_vertex(a, &b_edges, b) || interior_vertex(b, &a_edges, a) {
        return true;
    }
    for &(a1, a2) in &a_edges {
        let a1 = point(a1);
        let da = point(a2) - a1;
        for &(b1, b2) in &b_edges {
            let b1 = point(b1);
            let b2 = point(b2);
            let db = b2 - b1;
            let ab1 = da.perp_dot(b1 - a1);
            let ab2 = da.perp_dot(b2 - a1);
            let ba1 = db.perp_dot(a1 - b1);
            let ba2 = db.perp_dot(a1 + da - b1);
            if ab1 * ab2 < 0.0 && ba1 * ba2 < 0.0 {
                return true;
            }
            if ab1 == 0.0 && ab2 == 0.0 && da.dot(db) * a_area * b_area > 0.0 {
                let c = da.dot(b1 - a1);
                let d = da.dot(b2 - a1);
                if c.max(d).min(da.dot(da)) > c.min(d).max(0.0) {
                    return true;
                }
            }
        }
    }
    false
}
