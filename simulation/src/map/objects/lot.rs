use crate::map::{Building, LotOrigin, Map, ProjectFilter, ProjectKind, RoadID, UpdateType};
use geom::{Intersect, Polygon, Shape, Vec2, Vec3, OBB};
use ordered_float::OrderedFloat;
use prototypes::BuildingGen;
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Lot {
    pub id: LotID,
    pub parent: RoadID,
    pub kind: LotKind,
    pub shape: OBB,
    pub height: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrowableLotError {
    MissingLot,
    MissingRoad,
    UnsupportedDensity,
    MissingPrototype,
    TooSmall,
    Obstructed,
    Wet,
    InfeasibleGrade,
    MixedUseDisabled,
}

impl Lot {
    pub(crate) fn check_growable_footprint(
        &self,
        map: &Map,
        generator: BuildingGen,
    ) -> Result<(), GrowableLotError> {
        if map.get(self.parent).is_none() {
            return Err(GrowableLotError::MissingRoad);
        }
        if !Building::valid_footprint(&self.shape, generator) {
            return Err(GrowableLotError::Obstructed);
        }
        if !Building::dry_footprint(map.environment(), &self.shape, None) {
            return Err(GrowableLotError::Wet);
        }
        if Self::footprint_obstructed(map, self.shape, false) {
            return Err(GrowableLotError::Obstructed);
        }
        Ok(())
    }

    pub(crate) fn footprint_obstructed(map: &Map, shape: OBB, include_lots: bool) -> bool {
        let mut filter = ProjectFilter::INTER | ProjectFilter::BUILDING;
        if include_lots {
            filter = filter | ProjectFilter::LOT;
        }
        map.spatial_map.query(shape, filter).next().is_some()
            || map
                .spatial_map
                .query(Polygon::from(shape.corners.as_slice()), ProjectFilter::ROAD)
                .next()
                .is_some()
    }

    pub(crate) fn try_restore(map: &mut Map, origin: LotOrigin) -> Option<LotID> {
        const FRONTAGE_RADIUS: f32 = 15.0;

        let shape = origin.shape;
        if !Building::dry_footprint(map.environment(), &shape, None)
            || Self::footprint_obstructed(map, shape, true)
        {
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
                let projected = map.roads.get(id)?.points.project_2d(front);
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
        let height = map.environment.height(at.xy())?;
        if (height - at.z).abs() > 1.0 {
            return None;
        }

        let shape = OBB::new(at.xy() + axis * size * 0.5, axis, size, size);

        if Self::footprint_obstructed(map, shape, true) {
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

    pub fn generate_along_road(map: &mut Map, road: RoadID) {
        if !map.roads.contains_key(road) {
            log::error!("trying to generate along invalid road");
            return;
        }
        fn gen_side(map: &mut Map, road: RoadID, side: f32) {
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
                        0 => 20.0,
                        1 => 30.0,
                        _ => 40.0,
                    }
                }
            };

            let points = r.points.clone();
            let mut along = points.points_dirs_manual();
            let mut size = picksize();
            let mut d = size * 0.5;

            let mut lots = vec![];
            while let Some((pos, dir)) = along.next(d) {
                let axis = side * dir.perp_up();
                let l = Lot::try_make(map, road, pos + axis * (w + 1.0), axis.xy(), size);
                if let Some(id) = l {
                    lots.push(id);
                    map.subscribers.dispatch(UpdateType::Road, &map.lots[id]);

                    d += size * 0.5 + 2.0;
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
            gen_side(map, road, 1.0);
        }
        if pair.incoming.is_some() {
            gen_side(map, road, -1.0);
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
