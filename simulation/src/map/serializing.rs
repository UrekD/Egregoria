use std::collections::BTreeMap;

use geom::{Shape, Vec2};
use serde::{Deserialize, Serialize};
use slotmapd::Key;

use crate::map::{
    BuildingID, Buildings, ElectricityCache, Environment, Intersections, Lanes, LotOrigin, Lots,
    Map, ParkingSpots, Roads, SpatialMap,
};

#[derive(Serialize, Deserialize)]
struct MapWithLotOrigins<T> {
    map: T,
    lot_origins: BTreeMap<BuildingID, LotOrigin>,
}

#[derive(Serialize, Deserialize)]
struct ReadableMap {
    roads: Roads,
    intersections: Intersections,
    buildings: Buildings,
    lanes: Lanes,
    parking: ParkingSpots,
    lots: Lots,
    environment: Environment,
    external_train_stations: Vec<BuildingID>,
    #[serde(default)]
    import_lane_reserve: bool,
}

impl ReadableMap {
    fn from_serialized(map: SerializedMap, import_lane_reserve: bool) -> Self {
        Self {
            roads: map.roads,
            intersections: map.intersections,
            buildings: map.buildings,
            lanes: map.lanes,
            parking: map.parking,
            lots: map.lots,
            environment: map.environment,
            external_train_stations: map.external_train_stations,
            import_lane_reserve,
        }
    }

    fn into_serialized(self) -> SerializedMap {
        SerializedMap {
            roads: self.roads,
            intersections: self.intersections,
            buildings: self.buildings,
            lanes: self.lanes,
            parking: self.parking,
            lots: self.lots,
            environment: self.environment,
            external_train_stations: self.external_train_stations,
        }
    }
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
        let mut map = SerializedMap::from(self);
        // The stored placement geometry is authoritative. Generated faces
        // are rebuilt on load and do not participate in map hashes.
        for building in map.buildings.values_mut() {
            if building
                .development()
                .is_some_and(|site| site.frozen_shell.is_some())
            {
                building.mesh = Default::default();
            }
        }
        let lot_origins = self.lot_origins.clone();
        if serializer.is_human_readable() {
            return MapWithLotOrigins {
                map: ReadableMap::from_serialized(map, self.import_lane_reserve),
                lot_origins,
            }
            .serialize(serializer);
        }
        // Bincode uses positional fields. A trailing null station key explicitly
        // marks an import and preserves the byte layout of ordinary maps.
        if self.import_lane_reserve {
            map.external_train_stations.push(BuildingID::null());
        }
        MapWithLotOrigins { map, lot_origins }.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Map {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let (mut map, lot_origins, import_lane_reserve) = if deserializer.is_human_readable() {
            let MapWithLotOrigins { map, lot_origins } =
                MapWithLotOrigins::<ReadableMap>::deserialize(deserializer)?;
            let imported = map.import_lane_reserve;
            (map.into_serialized(), lot_origins, imported)
        } else {
            let MapWithLotOrigins {
                mut map,
                lot_origins,
            } = MapWithLotOrigins::<SerializedMap>::deserialize(deserializer)?;
            let imported = map.external_train_stations.last().is_some_and(Key::is_null);
            if imported {
                map.external_train_stations.pop();
            }
            (map, lot_origins, imported)
        };
        map.validate_references()
            .map_err(serde::de::Error::custom)?;
        map.validate_spatial_geometry()
            .map_err(serde::de::Error::custom)?;
        map.rebuild_growable_shells()
            .map_err(serde::de::Error::custom)?;
        Ok(map.into_map(lot_origins, import_lane_reserve))
    }
}

impl SerializedMap {
    fn rebuild_growable_shells(&mut self) -> Result<(), String> {
        for (id, building) in self.buildings.iter_mut() {
            let Some(site) = building.development() else {
                continue;
            };
            let Some(shell) = site.frozen_shell.as_ref() else {
                continue;
            };
            let invalid = || format!("building {id:?} has invalid frozen shell geometry");
            if !shell.valid_for(&building.obb)
                || shell.fit().base_z() != building.height
                || shell.fit().door() != building.door_pos
            {
                return Err(invalid());
            }
            let (mesh, roof_z) = shell
                .mesh(site.zone, site.floors, site.level)
                .ok_or_else(invalid)?;
            if roof_z != site.roof_z
                || mesh
                    .faces
                    .iter()
                    .flat_map(|(face, _)| face)
                    .any(|p| !p.is_finite())
            {
                return Err(invalid());
            }
            building.mesh = mesh;
        }
        Ok(())
    }

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
        // Grid cell coordinates must stay bounded even when all points in a
        // malformed record are equal and its cached length is therefore finite.
        let world = self.environment.bounds().ur;
        let index_limit =
            world.x.max(world.y) + crate::NewGameOptions::MAX_TERRAIN_CHUNKS as f32 * 512.0;
        for (id, road) in self.roads.iter() {
            if !road.width.is_finite() || road.width < 0.0 {
                return Err(format!("road {id:?} has an invalid width"));
            }
            let radius = Vec2::splat(road.width * 5.0 + 2.0);
            let valid = |points: &geom::PolyLine3, min_len| {
                points.len() >= min_len
                    && points.length().is_finite()
                    && points.length() >= 0.0
                    && points.iter().all(|point| {
                        point.is_finite()
                            && (point.xy() - radius).is_finite()
                            && (point.xy() + radius).is_finite()
                    })
                    && points
                        .segments()
                        .map(|segment| segment.src.distance(segment.dst))
                        .sum::<f32>()
                        .is_finite()
            };
            if !valid(&road.points, 2) || !valid(road.interfaced_points(), 1) {
                return Err(format!("road {id:?} has an invalid polyline"));
            }
            let raw_bounds = road
                .points()
                .bbox()
                .flatten()
                .expand(road.width * 5.0 + 2.0);
            let deck_bounds = super::road_support_bounds(road);
            if !raw_bounds.contains(deck_bounds.ll)
                || !raw_bounds.contains(deck_bounds.ur)
                || raw_bounds.ll.x < -index_limit
                || raw_bounds.ll.y < -index_limit
                || raw_bounds.ur.x > index_limit
                || raw_bounds.ur.y > index_limit
                || !deck_bounds.size().is_finite()
            {
                return Err(format!("road {id:?} has invalid deck index bounds"));
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
            if building.obb.corners.iter().any(|point| !point.is_finite())
                || !super::valid_building_index_bounds(building.obb.bbox())
                || building
                    .zone
                    .as_ref()
                    .is_some_and(|zone| zone.validated_area().is_none())
            {
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

    fn into_map(
        self,
        lot_origins: BTreeMap<BuildingID, LotOrigin>,
        import_lane_reserve: bool,
    ) -> Map {
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
            import_lane_reserve,
            lot_origins,
            ..Map::empty()
        };
        m.electricity = ElectricityCache::build(&m);
        m.water_services = super::WaterServiceCache::from_map(&m);
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
        sm.set_additional_occupancy(i.id.into(), super::junction_support_bounds(i, &m.roads));
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
    use common::saveload::{Bincode, Encoder, JSON};
    use geom::{vec3, PolyLine3, Vec3};

    fn serialized_map_with_road() -> (MapWithLotOrigins<SerializedMap>, RoadID) {
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
    fn w1_water_readable_map_stops_before_excess_stroke_payload() {
        let map = String::from_utf8(JSON::encode(&Map::empty()).unwrap()).unwrap();
        let points = vec!["0"; crate::map::MAX_WATER_VERTICES].join(",");
        let extra = "x".repeat(24 * 1024);
        let bodies = format!(
            r#""bodies":[{{"id":1,"level_dm":0,"shape":{{"River":{{"stroke":[{},"{}"],"width_m":32}}}}}}]"#,
            points, extra
        );
        let input = map.replacen(r#""bodies":[]"#, &bodies, 1);
        let mut reader = input.as_bytes();
        let error = JSON::decode_reader::<Map>(&mut reader).err().unwrap();
        assert!(
            reader.len() >= extra.len(),
            "consumed the excess stroke payload"
        );
        assert!(
            error.to_string().contains("water record limit exceeded"),
            "{error}"
        );
    }

    #[test]
    fn osm_trim_mode_round_trips_and_defaults_to_ordinary() {
        let legacy = MapWithLotOrigins {
            map: SerializedMap::default(),
            lot_origins: BTreeMap::new(),
        };
        let legacy_bytes = Bincode::encode(&legacy).unwrap();
        let mut ordinary = Bincode::decode::<Map>(&legacy_bytes).unwrap();
        assert!(!ordinary.import_lane_reserve);
        assert_eq!(Bincode::encode(&ordinary).unwrap(), legacy_bytes);
        let legacy_json = JSON::encode(&legacy).unwrap();
        assert!(
            !JSON::decode::<Map>(&legacy_json)
                .unwrap()
                .import_lane_reserve
        );

        ordinary.import_lane_reserve = true;
        assert!(ordinary.clone().import_lane_reserve);
        let imported = Bincode::decode::<Map>(&Bincode::encode(&ordinary).unwrap()).unwrap();
        assert!(imported.import_lane_reserve);
        assert!(imported.external_train_stations.is_empty());
        let imported = JSON::decode::<Map>(&JSON::encode(&ordinary).unwrap()).unwrap();
        assert!(imported.import_lane_reserve);
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
