use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use geom::{Shape, Vec2};
use serde::{Deserialize, Serialize};
use slotmapd::Key;

use crate::map::{
    BuildingID, Buildings, ElectricityCache, Environment, Intersections, Lanes, LotOrigin, Lots,
    Map, ParkingSpots, Roads, SpatialMap, TraverseKind,
};

#[derive(Serialize, Deserialize)]
struct MapWithLotOrigins<T, Origins = BTreeMap<BuildingID, LotOrigin>> {
    map: T,
    lot_origins: Origins,
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

#[derive(Serialize)]
struct BorrowedMap<'a> {
    roads: &'a Roads,
    intersections: &'a Intersections,
    buildings: &'a Buildings,
    lanes: &'a Lanes,
    parking: &'a ParkingSpots,
    lots: &'a Lots,
    environment: &'a Environment,
    external_train_stations: Cow<'a, [BuildingID]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    import_lane_reserve: Option<bool>,
}

impl Serialize for Map {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let import_lane_reserve = serializer
            .is_human_readable()
            .then_some(self.import_lane_reserve);
        let mut external_train_stations = Cow::Borrowed(self.external_train_stations.as_slice());
        // Keep the existing positional encoding for imported Bincode maps.
        if import_lane_reserve.is_none() && self.import_lane_reserve {
            external_train_stations.to_mut().push(BuildingID::null());
        }
        MapWithLotOrigins {
            map: BorrowedMap {
                roads: &self.roads,
                intersections: &self.intersections,
                buildings: &self.buildings,
                lanes: &self.lanes,
                parking: &self.parking,
                lots: &self.lots,
                environment: &self.environment,
                external_train_stations,
                import_lane_reserve,
            },
            lot_origins: &self.lot_origins,
        }
        .serialize(serializer)
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
        map.into_map(lot_origins, import_lane_reserve)
            .map_err(serde::de::Error::custom)
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
        let mut listed_lanes = BTreeSet::new();
        for (id, road) in self.roads.iter() {
            if road.id != id || road.src == road.dst {
                return Err(format!("road {id:?} has an invalid key or endpoints"));
            }
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
            let [forward, backward] = road.directed_lanes();
            for (direction, src, dst) in [
                (forward, road.src, road.dst),
                (backward, road.dst, road.src),
            ] {
                for &(lane, kind) in direction {
                    if !listed_lanes.insert(lane)
                        || self.lanes.get(lane).is_none_or(|record| {
                            record.id != lane
                                || record.parent != id
                                || record.kind != kind
                                || record.src != src
                                || record.dst != dst
                        })
                    {
                        return Err(format!("road {id:?} has an invalid lane {lane:?}"));
                    }
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
        if listed_lanes.len() != self.lanes.len() {
            return Err("map has a lane without an owning road".into());
        }
        for (id, intersection) in self.intersections.iter() {
            if intersection.id != id {
                return Err(format!("intersection {id:?} has an invalid key"));
            }
            let mut arms = BTreeSet::new();
            for &road in &intersection.roads {
                if !arms.insert(road)
                    || self
                        .roads
                        .get(road)
                        .is_none_or(|r| r.src != id && r.dst != id)
                {
                    return Err(format!("intersection {id:?} has an invalid road {road:?}"));
                }
            }
            for turn in intersection.raw_turns() {
                if turn.id.parent != id
                    || ![turn.id.src, turn.id.dst].into_iter().all(|key| {
                        self.lanes
                            .get(key)
                            .is_some_and(|lane| lane.src == id || lane.dst == id)
                    })
                    || !valid_path(&turn.points)
                {
                    return Err(format!(
                        "intersection {id:?} has an invalid turn lane or path"
                    ));
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
        for (id, lane) in self.lanes.iter() {
            if !valid_path(&lane.points)
                || !lane.dist_from_bottom.is_finite()
                || !lane.speed_limit.is_finite()
            {
                return Err(format!("lane {id:?} has invalid geometry"));
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
        let mut lot_memberships = 0;
        for (id, lot) in self.lots.iter() {
            let bad_bounds = || format!("lot {id:?} has invalid spatial bounds");
            if lot.shape.corners.iter().any(|point| {
                !point.is_finite() || point.x.abs() > index_limit || point.y.abs() > index_limit
            }) {
                return Err(bad_bounds());
            }
            lot_memberships += super::spatial_map::validated_lot_memberships(lot.shape.bbox())
                .ok_or_else(bad_bounds)?;
            if lot_memberships > super::spatial_map::MAX_LOT_MEMBERSHIPS {
                return Err("lot spatial membership limit exceeded".to_owned());
            }
        }
        Ok(())
    }

    fn into_map(
        mut self,
        lot_origins: BTreeMap<BuildingID, LotOrigin>,
        import_lane_reserve: bool,
    ) -> Result<Map, String> {
        let requested_roundabouts: Vec<_> = self
            .intersections
            .values()
            .filter(|junction| junction.is_roundabout())
            .map(|junction| junction.id)
            .collect();
        for junction in self.intersections.values_mut() {
            junction.index_crosswalks();
        }
        let spatial_map = mk_spatial_map(&self);
        let mut lots_by_road = BTreeMap::<_, std::collections::BTreeSet<_>>::new();
        for (id, lot) in self.lots.iter() {
            lots_by_road.entry(lot.parent).or_default().insert(id);
        }
        let mut m = Map {
            roads: self.roads,
            lanes: self.lanes,
            intersections: self.intersections,
            buildings: self.buildings,
            spatial_map,
            lots: self.lots,
            lots_by_road,
            parking: self.parking,
            environment: self.environment,
            external_train_stations: self.external_train_stations,
            import_lane_reserve,
            lot_origins,
            ..Map::empty()
        };
        let mut previous_paths = BTreeMap::new();
        for &id in &requested_roundabouts {
            remember_turns(&m.intersections[id], &mut previous_paths);
            let changed = super::roundabout_changed_clips(&m, id)
                .map_err(|error| format!("could not rebuild roundabout {id:?}: {error}"))?;
            for road in &changed {
                let road = &m.roads[*road];
                for (lane, _) in road.lanes_iter() {
                    previous_paths
                        .entry(TraverseKind::Lane(lane))
                        .or_insert_with(|| m.lanes[lane].points.as_slice().to_vec());
                }
                for junction in [road.src, road.dst] {
                    remember_turns(&m.intersections[junction], &mut previous_paths);
                }
            }
            if !changed.is_empty() {
                m.invalidate(id);
            }
        }
        for id in requested_roundabouts {
            m.refresh_junction_paths(id, true);
        }
        previous_paths.retain(|&traversal, old| {
            let current = match traversal {
                TraverseKind::Lane(id) => m.lanes.get(id).map(|lane| lane.points.as_slice()),
                TraverseKind::Turn(id) => m.intersections[id.parent]
                    .raw_turn(id)
                    .map(|turn| turn.points.as_slice()),
            };
            current != Some(old)
        });
        m.migrated_roundabout_paths = previous_paths;
        m.electricity = ElectricityCache::build(&m);
        m.water_services = super::WaterServiceCache::from_map(&m);
        Ok(m)
    }
}

fn valid_path(points: &geom::PolyLine3) -> bool {
    let length: f32 = points.segments().map(|s| s.src.distance(s.dst)).sum();
    !points.is_empty()
        && points.length().is_finite()
        && points.length() >= 0.0
        && points.iter().all(|p| p.is_finite())
        && length.is_finite()
        && (points.length() - length).abs() <= 0.01 + length * 0.001
}

fn remember_turns(
    junction: &crate::map::Intersection,
    paths: &mut BTreeMap<TraverseKind, Vec<geom::Vec3>>,
) {
    for turn in junction.raw_turns() {
        paths
            .entry(TraverseKind::Turn(turn.id))
            .or_insert_with(|| turn.points.as_slice().to_vec());
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
    use crate::map::{LanePatternBuilder, Lot, LotKind, MapProject, RoadID};
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
                map: Bincode::decode(&Bincode::encode(&map).unwrap())
                    .map(|data: MapWithLotOrigins<SerializedMap>| data.map)
                    .unwrap(),
                lot_origins: BTreeMap::new(),
            },
            road,
        )
    }

    #[test]
    fn p7_spatial_load_bounds() {
        let (mut serialized, road) = serialized_map_with_road();
        serialized.map.lots.insert_with_key(|id| Lot {
            id,
            parent: road,
            shape: geom::OBB::new(Vec2::ZERO, Vec2::X, 50_000.0, 1.0),
            height: 0.0,
            kind: LotKind::Unassigned,
        });
        let bytes = Bincode::encode(&serialized).unwrap();
        let error = Bincode::decode::<Map>(&bytes).err().expect("oversized lot");
        assert!(error.to_string().contains("lot"), "{error}");
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
        let legacy = MapWithLotOrigins::<SerializedMap> {
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
    fn roadtools_rejects_inconsistent_serialized_lane_records() {
        for damage in 0..4 {
            let (mut serialized, road_id) = serialized_map_with_road();
            let map = &mut serialized.map;
            let lane_id = map.roads[road_id].lanes_iter().next().unwrap().0;
            let lane = &mut map.lanes[lane_id];
            match damage {
                0 => lane.parent = RoadID::null(),
                1 => lane.src = super::super::IntersectionID::null(),
                2 => {
                    lane.points =
                        Bincode::decode(&Bincode::encode(&(&[] as &[Vec3], 0.0_f32)).unwrap())
                            .unwrap()
                }
                _ => lane.points = PolyLine3::new(vec![Vec3::ZERO, vec3(f32::NAN, 0.0, 0.0)]),
            }
            let bytes = Bincode::encode(&serialized).unwrap();
            let error = Bincode::decode::<Map>(&bytes)
                .err()
                .expect("accepted corrupt lane");
            assert!(error.to_string().contains("lane"), "{error}");
        }
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
