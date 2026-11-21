use serde::{Deserialize, Serialize};

use crate::map::{
    BuildingID, Buildings, ElectricityCache, Environment, Intersections, Lanes, Lots, Map,
    ParkingSpots, Roads, SpatialMap,
};

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct SerializedMap {
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
        SerializedMap::from(self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Map {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let map = SerializedMap::deserialize(deserializer)?;
        map.validate_references()
            .map_err(serde::de::Error::custom)?;
        Ok(map.into_map())
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

    fn into_map(self) -> Map {
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
    use crate::map::{LanePatternBuilder, MapProject};
    use common::saveload::{Bincode, Encoder};
    use geom::vec3;

    #[test]
    fn corrupt_serialized_map_references_are_rejected() {
        let mut map = Map::empty();
        let (_, road) = map
            .make_connection(
                MapProject::ground(vec3(10.0, 10.0, 0.0)),
                MapProject::ground(vec3(150.0, 10.0, 0.0)),
                None,
                &LanePatternBuilder::default().build(),
            )
            .unwrap();

        let mut serialized = SerializedMap::from(&map);
        serialized.intersections.remove(map.get(road).unwrap().src);
        let bytes = Bincode::encode(&serialized).unwrap();
        assert!(Bincode::decode::<Map>(&bytes).is_err());
    }
}
