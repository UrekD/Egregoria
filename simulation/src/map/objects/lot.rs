use crate::map::terrain::CELL_SIZE;
use crate::map::{
    Building, Environment, LotOrigin, Map, ProjectFilter, ProjectKind, Road, RoadID, UpdateType,
};
use geom::{Intersect, Polygon, Shape, Vec2, Vec3, OBB};
use ordered_float::OrderedFloat;
use serde::{Deserialize, Serialize};
use slotmapd::new_key_type;
use std::collections::BTreeSet;

new_key_type! {
    pub struct LotID;
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
pub enum LotKind {
    Unassigned,
    Residential,
}

/// How new lots are sized along roads. Built lots keep the shape they were built on.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LotLayout {
    /// A deterministic mix of 20, 30 and 40 m lots.
    #[default]
    Varied,
    /// Every lot is `UNIFORM_LOT_SIZE` square, so lots line up along both sides of a road.
    Uniform,
}

/// The middle of the varied mix and the smallest lot every growable fits on (900 m²).
pub const UNIFORM_LOT_SIZE: f32 = 30.0;
pub(crate) const MAX_LOT_SIZE: f32 = 40.0;
const FRONTAGE_HEIGHT_TOLERANCE_M: f32 = 1.0;
const MIN_LOT_SIZE: f32 = 20.0;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Lot {
    pub id: LotID,
    pub parent: RoadID,
    pub kind: LotKind,
    pub shape: OBB,
    pub height: f32,
}

impl Lot {
    pub(crate) fn footprint_obstructed(map: &Map, shape: OBB, include_lots: bool) -> bool {
        let mut filter = ProjectFilter::INTER | ProjectFilter::BUILDING;
        if include_lots {
            filter = filter | ProjectFilter::LOT;
        }
        !Building::dry_footprint(map.environment(), &shape, None)
            || map.spatial_map.query(shape, filter).next().is_some()
            || map
                .spatial_map
                .query(Polygon::from(shape.corners.as_slice()), ProjectFilter::ROAD)
                .next()
                .is_some()
    }

    pub(crate) fn try_restore(map: &mut Map, origin: LotOrigin) -> Option<LotID> {
        const FRONTAGE_RADIUS: f32 = 15.0;

        let shape = origin.shape;
        if Self::footprint_obstructed(map, shape, true) {
            return None;
        }

        let front = (shape.corners[0] + shape.corners[1]) * 0.5;
        let (road, projected, _) = map
            .spatial_map
            .query_around(front, FRONTAGE_RADIUS, ProjectFilter::ROAD)
            .filter_map(|object| {
                let ProjectKind::Road(id) = object else {
                    return None;
                };
                let road = map.roads.get(id)?;
                if !Self::allows_frontage(map, road)
                    || !Self::has_dry_frontage(map.environment(), road, &shape)
                {
                    return None;
                }
                let projected = road.points.project_2d(front);
                let dist = projected.xy().distance2(front);
                (dist <= FRONTAGE_RADIUS.powi(2)).then_some((id, projected, OrderedFloat(dist)))
            })
            .min_by_key(|&(id, _, distance)| (distance, id))?;

        let depth = shape.corners[3] - shape.corners[0];
        let id = Self::try_make(
            map,
            road,
            front.z(projected.z),
            depth.normalize(),
            depth.mag(),
        )?;
        map.subscribers.dispatch(UpdateType::Road, &map.lots[id]);
        Some(id)
    }

    pub fn try_make(
        map: &mut Map,
        parent: RoadID,
        at: Vec3,
        axis: Vec2,
        size: f32,
    ) -> Option<LotID> {
        if !at.is_finite()
            || !axis.is_finite()
            || axis == Vec2::ZERO
            || !size.is_finite()
            || size <= 0.0
        {
            return None;
        }
        let shape = OBB::new(at.xy() + axis * size * 0.5, axis, size, size);
        Self::make_if_clear(map, parent, at, shape)
    }

    fn make_if_clear(map: &mut Map, parent: RoadID, at: Vec3, shape: OBB) -> Option<LotID> {
        let height = map.environment.height(at.xy())?;
        if (height - at.z).abs() > FRONTAGE_HEIGHT_TOLERANCE_M {
            return None;
        }
        let road = map.roads.get(parent)?;
        if !Self::allows_frontage(map, road)
            || Self::footprint_obstructed(map, shape, true)
            || !Self::has_dry_frontage(map.environment(), road, &shape)
        {
            return None;
        }

        let id = map.lots.insert_with_key(move |id| Lot {
            id,
            parent,
            kind: LotKind::Unassigned,
            shape,
            height,
        });
        map.spatial_map.insert(&map.lots[id]);
        Some(id)
    }

    pub(crate) fn allows_frontage(map: &Map, road: &Road) -> bool {
        !map.no_frontage.contains(&road.id) && road.has_sidewalks()
    }

    /// Check the full carriageway over the lot's frontage against the local terrain and
    /// water. Walk both ways from the projection so a dry midpoint cannot hide a wet
    /// shoulder or an elevated section at one end of the lot.
    pub(crate) fn has_dry_frontage(env: &Environment, road: &Road, shape: &OBB) -> bool {
        if !env.strict_water_siting() {
            return true;
        }
        if !Building::dry_footprint(env, shape, None) {
            return false;
        }
        let [a, b, _, _] = shape.corners;
        let front = (a + b) * 0.5;
        let points = road.points();
        let (projected, end) = points.project_segment_2d(front);
        if end == 0 || projected.xy().distance(front) > road.width * 0.5 + 2.0 {
            return false;
        }
        let start = points[end - 1];
        let tangent = (points[end] - start).xy();
        let sidewalks = road.sidewalks(road.src);
        let sidewalk = if (front - projected.xy()).dot(tangent.perpendicular()) > 0.0 {
            sidewalks.outgoing
        } else {
            sidewalks.incoming
        };
        if sidewalk.is_none() {
            return false;
        }

        // A horizontal segment's bilinear ground samples are spaced no farther
        // apart than half a terrain cell, including both ends of the frontage.
        let usable = |from: Vec3, to: Vec3| {
            let Some((axis, length)) = (to - from).xy().dir_dist() else {
                return (to.z - from.z).abs() <= f32::EPSILON;
            };
            let deck = OBB::new((from.xy() + to.xy()) * 0.5, axis, length, road.width);
            let steps = (length / (CELL_SIZE * 0.5)).ceil() as usize;
            Building::dry_footprint(env, &deck, None)
                && (0..=steps).all(|step| {
                    let pos = from + (to - from) * (step as f32 / steps as f32);
                    env.height(pos.xy())
                        .is_some_and(|ground| (ground - pos.z).abs() <= FRONTAGE_HEIGHT_TOLERANCE_M)
                })
        };
        let check_side = |side: &mut dyn Iterator<Item = &Vec3>| {
            let mut remaining = a.distance(b) * 0.5;
            let mut from = projected;
            for &to in side {
                let length = from.xy().distance(to.xy());
                if length >= remaining {
                    return usable(from, from + (to - from) * (remaining / length));
                }
                if !usable(from, to) {
                    return false;
                }
                remaining -= length;
                from = to;
            }
            false
        };
        let (before, after) = points.as_slice().split_at(end);
        check_side(&mut before.iter().rev()) && check_side(&mut after.iter())
    }

    pub fn generate_along_road(map: &mut Map, road: RoadID) {
        Self::generate_along_road_where(map, road, None);
    }

    pub(crate) fn generate_along_road_where(
        map: &mut Map,
        road: RoadID,
        eligible: Option<&dyn Fn(OBB, f32) -> bool>,
    ) {
        let Some(r) = map.roads.get(road) else {
            log::error!("trying to generate along invalid road");
            return;
        };
        if !Self::allows_frontage(map, r) {
            return;
        }

        fn gen_side(
            map: &mut Map,
            road: RoadID,
            side: f32,
            eligible: Option<&dyn Fn(OBB, f32) -> bool>,
        ) {
            let r = unwrap_ret!(map.roads.get(road));

            let w = r.width * 0.5;
            let mut ri = 0.0;
            let r1 = r.points.first().x + r.points.last().x;
            let r2 = r.points.last().y + r.points.first().y;
            let r3 = side * r.length();
            let layout = map.lot_layout;
            let mut picksize = || match layout {
                LotLayout::Uniform => UNIFORM_LOT_SIZE,
                LotLayout::Varied => {
                    ri += 1.0;
                    match (common::rand::rand4(r1, r2, r3, ri) * 3.0) as usize {
                        0 => MIN_LOT_SIZE,
                        1 => UNIFORM_LOT_SIZE,
                        _ => MAX_LOT_SIZE,
                    }
                }
            };

            let points = r.points.clone();
            let mut along = points.points_dirs_manual();
            let mut size = picksize();
            let mut d = size * 0.5;

            while let Some((pos, dir)) = along.next(d) {
                let axis = side * dir.perp_up();
                let at = pos + axis * (w + 1.0);
                // Lots already on the street can leave space for a narrower lot
                // than this position's first seeded size after drying.
                let smaller = [UNIFORM_LOT_SIZE, MIN_LOT_SIZE]
                    .into_iter()
                    .filter(|&other| {
                        eligible.is_some() && layout == LotLayout::Varied && other < size
                    });
                let lot = std::iter::once(size).chain(smaller).find_map(|size| {
                    let shape = OBB::new(at.xy() + axis.xy() * size * 0.5, axis.xy(), size, size);
                    if !eligible.is_none_or(|filter| filter(shape, w)) {
                        return None;
                    }
                    Lot::make_if_clear(map, road, at, shape).map(|id| (id, size))
                });
                if let Some((id, placed_size)) = lot {
                    map.subscribers.dispatch(UpdateType::Road, &map.lots[id]);

                    d += placed_size * 0.5 + 2.0;
                    size = picksize();
                    d += size * 0.5;
                } else {
                    d += 2.0;
                }
            }
        }

        let r = unwrap_ret!(map.roads.get(road));
        let pair = r.sidewalks(r.src);
        if pair.outgoing.is_some() {
            gen_side(map, road, 1.0, eligible);
        }
        if pair.incoming.is_some() {
            gen_side(map, road, -1.0, eligible);
        }
    }

    pub fn remove_intersecting_lots(map: &mut Map, road: RoadID) {
        let r = unwrap_retlog!(map.roads.get(road), "{:?} does not exist", road);
        let road_shape = r.boldline();
        let mut to_remove: BTreeSet<_> = map
            .spatial_map
            .query(road_shape.bbox(), ProjectFilter::LOT)
            .filter_map(|object| {
                let id = object.to_lot()?;
                let shape = &map.lots[id].shape;
                road_shape
                    .intersects(&Polygon::from(shape.corners.as_slice()))
                    .then_some(object)
            })
            .collect();

        // Connecting a road can widen its junctions before the lane update. Use
        // their new bounds both to clear old lots and to place the new ones.
        for id in [r.src, r.dst] {
            let junction = unwrap_cont!(map.intersections.get_mut(id));
            junction.update_interface_radius(&mut map.roads);
            map.spatial_map.update(junction);
            to_remove.extend(
                map.spatial_map
                    .query(junction.bcircle(), ProjectFilter::LOT),
            );
        }

        for lot in to_remove {
            if let ProjectKind::Lot(lot) = lot {
                map.lots.remove(lot);
                map.spatial_map.remove(lot);
            }
        }
    }
}

#[cfg(test)]
mod water_tests {
    use super::*;
    use crate::map::{Environment, LanePatternBuilder, MapProject};
    use geom::vec2;

    #[test]
    fn w2b_water_lot_rejects_wet_footprint_and_wet_frontage() {
        let mut map = Map::empty();
        assert!(map.install_environment(Environment::new(1, 1)));
        let (_, road) = map
            .make_connection(
                MapProject::ground(vec2(48.0, 109.0).z0()),
                MapProject::ground(vec2(448.0, 109.0).z0()),
                None,
                &LanePatternBuilder::new().build(),
            )
            .unwrap();
        let curb_y = 109.0 + map.roads[road].width * 0.5 + 1.0;
        let lots = map.lots.keys().collect::<Vec<_>>();
        for id in lots {
            map.lots.remove(id);
            map.spatial_map.remove(id);
        }
        let env = map.environment_mut_for_test();
        for center in [vec2(152.0, 104.0), vec2(248.0, 136.0)] {
            env.fill_lake(&Polygon::centered_rect(center, 8.0, 8.0), 0, 1.0)
                .unwrap();
        }

        let at = |x| vec2(x, curb_y).z(0.3);
        assert!(Lot::try_make(&mut map, road, at(248.0), Vec2::Y, 20.0).is_none());
        let frontage_only = OBB::new(at(164.0).xy() + Vec2::Y * 10.0, Vec2::Y, 20.0, 20.0);
        assert!(!map
            .environment()
            .intersects_water(&Polygon(frontage_only.corners.to_vec())));
        assert!(Lot::try_make(&mut map, road, at(164.0), Vec2::Y, 20.0).is_none());
        assert!(Lot::try_make(&mut map, road, at(336.0), Vec2::Y, 20.0).is_some());
    }
}
