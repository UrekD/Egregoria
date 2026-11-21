use std::collections::BTreeMap;

use geom::Vec2;
use serde::{Deserialize, Serialize};

use crate::map::{
    BuildingID, Buildings, ElectricityCache, Environment, Intersections, Lanes, LotOrigin, Lots,
    Map, ParkingSpots, Roads, SpatialMap,
};

#[derive(Serialize, Deserialize)]
struct MapWithLotOrigins {
    map: SerializedMap,
    lot_origins: BTreeMap<BuildingID, LotOrigin>,
}

#[derive(Default, Serialize, Deserialize)]
struct SerializedMap {
    pub roads: Roads,
    pub intersections: Intersections,
    pub buildings: Buildings,
    pub lanes: Lanes,
    pub parking: ParkingSpots,
    pub lots: Lots,
    pub environment: Environment,
    pub external_train_stations: Vec<BuildingID>,
}

impl From<&Map> for SerializedMap {
    fn from(m: &Map) -> Self {
        Self {
            roads: m.roads.clone(),
            intersections: m.intersections.clone(),
            buildings: m.buildings.clone(),
            lanes: m.lanes.clone(),
            parking: m.parking.clone(),
            lots: m.lots.clone(),
            environment: m.environment.clone(),
            external_train_stations: m.external_train_stations.clone(),
        }
    }
}

impl Serialize for Map {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        MapWithLotOrigins {
            map: self.into(),
            lot_origins: self.lot_origins.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Map {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let MapWithLotOrigins { map, lot_origins } = MapWithLotOrigins::deserialize(deserializer)?;
        map.validate_references()
            .map_err(serde::de::Error::custom)?;
        map.validate_spatial_geometry()
            .map_err(serde::de::Error::custom)?;
        Ok(map.into_map(lot_origins))
    }
}

impl SerializedMap {
    fn validate_references(&self) -> Result<(), String> {
        for (id, road) in self.roads.iter() {
            for junction in [road.src, road.dst] {
                if self
                    .intersections
                    .get(junction)
                    .is_none_or(|i| !i.roads.contains(&id))
                {
                    return Err(format!(
                        "road {id:?} has an invalid intersection {junction:?}"
                    ));
                }
            }
            for (lane, _) in road.lanes_iter() {
                if !self.lanes.contains_key(lane) {
                    return Err(format!("road {id:?} has an invalid lane {lane:?}"));
                }
            }
            for &building in &road.connected_buildings {
                if self
                    .buildings
                    .get(building)
                    .is_none_or(|b| b.connected_road != Some(id))
                {
                    return Err(format!("road {id:?} has an invalid building {building:?}"));
                }
            }
        }
        for (id, intersection) in self.intersections.iter() {
            for &road in &intersection.roads {
                if self
                    .roads
                    .get(road)
                    .is_none_or(|r| r.src != id && r.dst != id)
                {
                    return Err(format!("intersection {id:?} has an invalid road {road:?}"));
                }
            }
        }
        for (id, building) in self.buildings.iter() {
            if let Some(road) = building.connected_road {
                if self
                    .roads
                    .get(road)
                    .is_none_or(|r| !r.connected_buildings.contains(&id))
                {
                    return Err(format!("building {id:?} has an invalid road {road:?}"));
                }
            }
        }
        Ok(())
    }

    fn validate_spatial_geometry(&self) -> Result<(), String> {
        for (id, road) in self.roads.iter() {
            if !road.width.is_finite() || road.width < 0.0 {
                return Err(format!("road {id:?} has an invalid width"));
            }
            let radius = Vec2::splat(road.width * 0.5);
            if road.points.len() < 2
                || !road.points.length().is_finite()
                || road.points.iter().any(|point| {
                    !point.is_finite()
                        || !(point.xy() - radius).is_finite()
                        || !(point.xy() + radius).is_finite()
                })
            {
                return Err(format!("road {id:?} has an invalid polyline"));
            }
        }
        for (id, intersection) in self.intersections.iter() {
            let bounds = intersection.bcircle();
            let radius = Vec2::splat(bounds.radius);
            if bounds.radius < 0.0
                || !(bounds.center - radius).is_finite()
                || !(bounds.center + radius).is_finite()
            {
                return Err(format!("intersection {id:?} has invalid spatial bounds"));
            }
        }
        for (id, building) in self.buildings.iter() {
            let points = match &building.zone {
                Some(zone) => zone.poly.as_slice(),
                None => &building.obb.corners,
            };
            if points.iter().any(|point| !point.is_finite()) {
                return Err(format!("building {id:?} has invalid spatial bounds"));
            }
        }
        for (id, lot) in self.lots.iter() {
            if lot.shape.corners.iter().any(|point| !point.is_finite()) {
                return Err(format!("lot {id:?} has invalid spatial bounds"));
            }
        }
        Ok(())
    }

    fn into_map(self, lot_origins: BTreeMap<BuildingID, LotOrigin>) -> Map {
        let spatial_map = mk_spatial_map(&self);
        let mut m = Map {
            roads: self.roads,
            lanes: self.lanes,
            intersections: self.intersections,
            buildings: self.buildings,
            spatial_map,
            lots: self.lots,
            parking: self.parking,
            environment: self.environment,
            external_train_stations: self.external_train_stations,
            lot_origins,
            ..Map::empty()
        };
        m.electricity = ElectricityCache::build(&m);
        m
    }
}

fn mk_spatial_map(m: &SerializedMap) -> SpatialMap {
    let mut sm = SpatialMap::default();
    for b in m.buildings.values() {
        sm.insert(b);
    }
    for r in m.roads.values() {
        sm.insert(r);
    }
    for i in m.intersections.values() {
        sm.insert(i);
    }
    for l in m.lots.values() {
        sm.insert(l);
    }
    sm
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::{LanePatternBuilder, MapProject, RoadID};
    use common::saveload::{Bincode, Encoder};
    use geom::{vec3, PolyLine3, Vec3};

    fn serialized_map_with_road() -> (MapWithLotOrigins, RoadID) {
        let mut map = Map::empty();
        let (_, road) = map
            .make_connection(
                MapProject::ground(vec3(10.0, 10.0, 0.0)),
                MapProject::ground(vec3(150.0, 10.0, 0.0)),
                None,
                &LanePatternBuilder::default().build(),
            )
            .unwrap();

        (
            MapWithLotOrigins {
                map: (&map).into(),
                lot_origins: BTreeMap::new(),
            },
            road,
        )
    }

    #[test]
    fn corrupt_serialized_map_references_are_rejected() {
        let (mut serialized, road) = serialized_map_with_road();
        let map = &mut serialized.map;
        map.intersections.remove(map.roads[road].src);
        let bytes = Bincode::encode(&serialized).unwrap();
        let error = Bincode::decode::<Map>(&bytes)
            .err()
            .expect("missing road intersection");
        assert!(
            error.to_string().contains("invalid intersection"),
            "{error}"
        );
    }

    #[test]
    fn invalid_serialized_road_polylines_are_rejected() {
        let (mut serialized, road) = serialized_map_with_road();
        for points in [
            &[][..],
            &[Vec3::ZERO],
            &[Vec3::ZERO, vec3(1.0, 0.0, f32::NAN)],
        ] {
            // Encode the fields directly because PolyLine3::new requires at least one point.
            let polyline = Bincode::encode(&(points, 0.0_f32)).unwrap();
            serialized.map.roads[road].points = Bincode::decode::<PolyLine3>(&polyline).unwrap();

            let bytes = Bincode::encode(&serialized).unwrap();
            let error = Bincode::decode::<Map>(&bytes)
                .err()
                .expect("invalid polyline");
            assert!(error.to_string().contains("polyline"), "{error}");
        }
    }
}
