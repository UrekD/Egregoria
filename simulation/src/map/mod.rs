mod objects {
    mod building;
    mod intersection;
    mod lane;
    mod lot;
    mod parking;
    mod road;
    mod turn;

    pub use building::*;
    pub use intersection::*;
    pub use lane::*;
    pub use lot::*;
    pub use parking::*;
    pub use road::*;
    pub use turn::*;
}

pub use objects::*;

pub mod procgen {
    mod building;
    pub mod heightmap;
    mod presets;
    mod tiled_start;
    pub(crate) use tiled_start::commission_native_land;
    mod water;

    pub use building::*;
    pub use presets::*;
    pub use water::generate_waters;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum PresetImportError {
        WaterConfigured,
    }

    pub fn load_parismap(map: &mut super::Map) -> Result<(), PresetImportError> {
        if map.needs_topology_preparation() {
            return Err(PresetImportError::WaterConfigured);
        }
        presets::load_parismap(map);
        Ok(())
    }

    pub fn load_testfield(
        map: &mut super::Map,
        pos: geom::Vec2,
        size: u32,
        spacing: f32,
    ) -> Result<(), PresetImportError> {
        if map.needs_topology_preparation() {
            return Err(PresetImportError::WaterConfigured);
        }
        presets::load_testfield(map, pos, size, spacing);
        Ok(())
    }
}

mod authored;
mod change_detection;
mod electricity_cache;
mod height_override;
pub mod import;
pub(crate) mod land_frontage;
mod land_ownership;
mod land_site;
mod lane_scoring;
mod light_policy;
#[allow(clippy::module_inception)]
mod map;
mod indexed_astar;
mod pathfinding;
mod reachability;
pub mod road_edit;
mod serializing;
pub(crate) use serializing::CurrentSnapshotMap;
mod spatial_map;
pub mod building_parking;
pub mod special_placement;
pub mod terrain;
mod traffic_control;
mod traversable;
mod turn_policy;
mod water;
mod water_serde;
pub mod zonegrid;

// Use self or else it would be ambiguous with "pathfinding" crate
pub use self::pathfinding::*;
pub use authored::*;
pub use change_detection::*;
pub use electricity_cache::*;
pub use land_ownership::*;
pub(crate) use land_site::PreparedEffects;
pub(crate) use lane_scoring::lane_scoring_update;
pub use lane_scoring::{LaneScoringError, LaneScoringPilot};
pub use light_policy::*;
pub use map::*;
pub(crate) use pathpark::PreparedWalkingPath;
pub use pathpark::{CurbAnchor, PathEndpoint, RoadSide};
pub use spatial_map::*;
pub use terrain::*;
pub use traffic_control::*;
pub use traversable::*;
pub use turn_policy::*;
pub use water::water_service::*;
pub use water::*;
pub use zonegrid::*;

pub use ::pathfinding as pathfinding_crate;

pub const CROSSWALK_WIDTH: f32 = 2.0;
pub const ROAD_Z_OFFSET: f32 = 0.3;
pub const MAX_SLOPE: f32 = 0.25; // 25% grade

/// Checks the scalar fields that the wire decoder validates for water edits.
/// WaterStroke and WaterBasin can only be constructed with checked geometry.
pub(crate) fn validate_water_edit_wire(edit: &WaterEdit) -> Result<(), WaterEditError> {
    let (level, depth) = match *edit {
        WaterEdit::SetSeaLevel { level_dm } => (level_dm, None),
        WaterEdit::CarveRiver {
            width_m,
            depth_m,
            level_dm,
            ..
        } => {
            if !water::valid_river_width(width_m) {
                return Err(WaterEditError::InvalidInput);
            }
            (level_dm, Some(depth_m))
        }
        WaterEdit::FillLake {
            level_dm, depth_m, ..
        } => (Some(level_dm), Some(depth_m)),
        WaterEdit::RemoveFreshWater { .. } => return Ok(()),
    };
    if depth.is_some_and(|depth| !water::valid_water_depth(f64::from(depth))) {
        return Err(WaterEditError::InvalidInput);
    }
    if let Some(level) = level {
        water::packed_bed_limit(level, water::MIN_DEPTH)?;
    }
    Ok(())
}

pub(crate) fn validate_loaded_land(sim: &crate::Simulation) -> Result<(), String> {
    land_ownership::validate_loaded_land(sim)?;
    sim.map()
        .validate_commissioned_reserves(sim.read::<crate::SimulationOptions>().land_recipe)
        .map_err(|error| format!("Invalid commissioned land: {error:?}"))
}

mod pathpark {
    use super::{
        road_edit::{stamp, RoadEditStamp},
        ConnectedRoad, ConnectionEndpoint, ConnectionError, IntersectionID, JunctionRules, LaneID, Map,
        PreparedTopologyEdit, ProjectKind, Road, RoadID, SmallRoadPreset, TurnID, WaterTopologyWork,
        ROAD_Z_OFFSET,
    };
    use geom::{vec2, Vec2, Vec3};
    use serde::{Deserialize, Serialize};
    use std::collections::{BTreeMap, BTreeSet};

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum RoadSide {
        Left,
        Right,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct CurbAnchor {
        pub road: RoadID,
        pub side: RoadSide,
        pub station_cm: u32,
        pub expected: RoadEditStamp,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum PathEndpoint {
        Ground {
            point_cm: [i32; 2],
            expected: u64,
        },
        Path {
            road: RoadID,
            station_cm: u32,
            expected: RoadEditStamp,
        },
        Sidewalk(CurbAnchor),
        PathEnd {
            road: RoadID,
            joint: IntersectionID,
            expected: RoadEditStamp,
        },
    }

    impl PathEndpoint {
        pub fn road(self) -> Option<RoadID> {
            match self {
                Self::Ground { .. } => None,
                Self::Path { road, .. }
                | Self::PathEnd { road, .. }
                | Self::Sidewalk(CurbAnchor { road, .. }) => Some(road),
            }
        }
    }

    pub(crate) struct PreparedWalkingPath {
        from: ConnectionEndpoint,
        to: ConnectionEndpoint,
        elbow: Option<Vec2>,
        topology: PreparedTopologyEdit,
        rules: Vec<SplitRules>,
    }

    struct SplitRules {
        road: RoadID,
        joint: IntersectionID,
        child: usize,
        lanes: Vec<LaneID>,
        rules: JunctionRules,
    }

    impl Map {
        pub fn pick_path_ground(&self, x_cm: i32, y_cm: i32) -> Option<PathEndpoint> {
            let point_cm = [x_cm, y_cm];
            let (_, expected) = self.path_ground(point_cm)?;
            Some(PathEndpoint::Ground { point_cm, expected })
        }

        fn path_ground(&self, point_cm: [i32; 2]) -> Option<(Vec3, u64)> {
            let xy = point_at(point_cm);
            let env = self.environment();
            if !env.bounds().contains(xy) || env.is_water(xy) {
                return None;
            }
            let z = super::profile_height(env, xy)?;
            Some((
                xy.z(z),
                common::hash_u64((z.to_bits(), env.water_revision())),
            ))
        }

        pub fn pick_curb(&self, road: RoadID, side: RoadSide, station_cm: u32) -> Option<CurbAnchor> {
            let r = self.get(road)?;
            r.sidewalk_on(side)?;
            self.path_station(r, station_cm)?;
            Some(CurbAnchor {
                road,
                side,
                station_cm,
                expected: stamp(self, r, &[]).ok()?,
            })
        }

        pub(crate) fn authorize_driveway_pad(&self, points: &[Vec2]) -> bool {
            self.authorize_land_footprint(points, None).is_ok()
        }

        pub fn pick_path_sidewalk(
            &self,
            road: RoadID,
            side: RoadSide,
            station_cm: u32,
        ) -> Option<PathEndpoint> {
            self.pick_curb(road, side, station_cm)
                .map(PathEndpoint::Sidewalk)
        }

        pub fn pick_path_road(&self, road: RoadID, station_cm: u32) -> Option<PathEndpoint> {
            let r = self.get(road)?;
            if !r.is_walking_path() {
                return None;
            }
            self.path_station(r, station_cm)?;
            Some(PathEndpoint::Path {
                road,
                station_cm,
                expected: stamp(self, r, &[]).ok()?,
            })
        }

        // Stations are measured on the full canonical profile. Keep the join clear
        // of endpoint interfaces so the split cannot inherit a different deck.
        pub(crate) fn path_station(&self, road: &Road, station_cm: u32) -> Option<(Vec3, usize)> {
            if road.elevated_span() || road.points().len() > 1024 || road.n_lanes() > 32 {
                return None;
            }
            let mut offset = station_cm as f32 * 0.01;
            let margin = road.width.max(3.0);
            if offset < road.interface_from(road.src) + margin
                || offset > road.length() - road.interface_from(road.dst) - margin
            {
                return None;
            }
            for (index, &[a, b]) in road.points().array_windows::<2>().enumerate() {
                let length = a.distance(b);
                if length > 0.0 && offset <= length {
                    let point = a + (b - a) * (offset / length);
                    let ground = super::profile_height(self.environment(), point.xy())?;
                    if self.environment().is_water(point.xy())
                        || (point.z - ROAD_Z_OFFSET - ground).abs() > 0.35
                    {
                        return None;
                    }
                    return Some((point, index + 1));
                }
                offset -= length;
            }
            None
        }

        pub fn pick_path_attachment(
            &self,
            kind: ProjectKind,
            pointer: Vec3,
        ) -> Option<PathEndpoint> {
            match kind {
                ProjectKind::Road(id) => {
                    let road = self.get(id)?;
                    let points = road.points();
                    let (center, end) = points.project_segment_2d(pointer.xy());
                    let start = end.checked_sub(1)?;
                    let forward = (points[end] - points[start]).xy().try_normalize()?;
                    let station = points
                        .segments()
                        .take(start)
                        .map(|s| s.src.distance(s.dst))
                        .sum::<f32>()
                        + points[start].distance(center);
                    let station = (station * 100.0).round() as u32;
                    if road.is_walking_path() {
                        self.pick_path_road(id, station).or_else(|| {
                            let joint = if center.distance2(points.first())
                                < center.distance2(points.last())
                            {
                                road.src
                            } else {
                                road.dst
                            };
                            self.pick_path_end(id, joint)
                        })
                    } else if road.allows_motor_access() {
                        let side = if forward.perpendicular().dot(pointer.xy() - center.xy()) > 0.0
                        {
                            RoadSide::Right
                        } else {
                            RoadSide::Left
                        };
                        self.pick_path_sidewalk(id, side, station)
                    } else {
                        None
                    }
                }
                ProjectKind::Intersection(joint) => {
                    let id = self.get(joint)?.roads.first().copied()?;
                    self.pick_path_end(id, joint)
                }
                _ => None,
            }
        }

        fn pick_path_end(&self, road: RoadID, joint: IntersectionID) -> Option<PathEndpoint> {
            let segment = self.get(road)?;
            let end = self.get(joint)?;
            if ![segment.src, segment.dst].contains(&joint)
                || segment.elevated_span()
                || !segment.is_walking_path()
                || !end
                    .roads
                    .iter()
                    .all(|&id| self.roads()[id].is_walking_path())
            {
                return None;
            }
            Some(PathEndpoint::PathEnd {
                road,
                joint,
                expected: stamp(self, segment, &[]).ok()?,
            })
        }

        pub fn path_endpoint_position(&self, endpoint: PathEndpoint) -> Option<Vec3> {
            Some(self.resolve_path_endpoint(endpoint).ok()?.pos)
        }

        fn resolve_path_endpoint(
            &self,
            endpoint: PathEndpoint,
        ) -> Result<ConnectionEndpoint, ConnectionError> {
            let (id, station, expected, side) = match endpoint {
                PathEndpoint::Ground { point_cm, expected } => {
                    let (pos, stamp) = self
                        .path_ground(point_cm)
                        .ok_or(ConnectionError::InvalidEndpoint)?;
                    if stamp != expected {
                        return Err(ConnectionError::Stale);
                    }
                    return Ok(ConnectionEndpoint {
                        pos,
                        kind: ProjectKind::Ground,
                        split_point: None,
                    });
                }
                PathEndpoint::Path {
                    road,
                    station_cm,
                    expected,
                } => (road, station_cm, expected, None),
                PathEndpoint::Sidewalk(at) => (at.road, at.station_cm, at.expected, Some(at.side)),
                PathEndpoint::PathEnd { road, joint, .. } => {
                    if self.pick_path_end(road, joint) != Some(endpoint) {
                        return Err(ConnectionError::Stale);
                    }
                    return Ok(ConnectionEndpoint {
                        pos: self.get(joint).ok_or(ConnectionError::Stale)?.pos,
                        kind: ProjectKind::Intersection(joint),
                        split_point: None,
                    });
                }
            };
            let road = self.get(id).ok_or(ConnectionError::Stale)?;
            let (mut point, segment) = self
                .path_station(road, station)
                .ok_or(ConnectionError::InvalidEndpoint)?;
            if stamp(self, road, &[]).ok() != Some(expected) {
                return Err(ConnectionError::Stale);
            }
            match side {
                None if road.is_walking_path() => (),
                Some(side) if road.sidewalk_on(side).is_some() && road.allows_motor_access() => (),
                _ => return Err(ConnectionError::InvalidEndpoint),
            }
            point.z = road.split_surface_height(point, self.environment());
            Ok(ConnectionEndpoint {
                pos: point - Vec3::z(ROAD_Z_OFFSET),
                kind: ProjectKind::Road(id),
                split_point: Some((point, segment)),
            })
        }

        pub(crate) fn prepare_walking_path(
            &self,
            from: PathEndpoint,
            to: PathEndpoint,
            control: Option<[i32; 2]>,
            admission: &crate::transportation::junction_admission::JunctionAdmission,
            work: &mut WaterTopologyWork,
        ) -> Result<PreparedWalkingPath, ConnectionError> {
            for id in [from, to].into_iter().filter_map(PathEndpoint::road) {
                let road = self.get(id).ok_or(ConnectionError::Stale)?;
                let work_units = road
                    .points()
                    .len()
                    .saturating_add(road.interfaced_points().len())
                    .saturating_add(road.n_lanes())
                    .saturating_add(admission.crossing_count() * 2);
                super::spend_water_work(&mut work.remaining, work_units)?;
            }
            let sides = [from, to].map(|endpoint| match endpoint {
                PathEndpoint::Sidewalk(anchor) => Some(anchor.side),
                _ => None,
            });
            let from = self.resolve_path_endpoint(from)?;
            let to = self.resolve_path_endpoint(to)?;
            let elbow = control.map(point_at);
            let pattern = SmallRoadPreset::WalkingPath.lane_pattern();
            if !Self::can_connect(from, to, elbow, &pattern) {
                return Err(ConnectionError::InvalidEndpoint);
            }

            let mut rules = Vec::new();
            for endpoint in [from, to] {
                if let ProjectKind::Road(id) = endpoint.kind {
                    let road = &self.roads()[id];
                    for (child, joint) in [road.src, road.dst].into_iter().enumerate() {
                        if admission.crossing_junction(joint) {
                            return Err(ConnectionError::Stale);
                        }
                        if let Some(existing) = self.junction_rules(joint) {
                            rules.push(SplitRules {
                                road: road.id,
                                joint,
                                child,
                                lanes: road.lanes_iter().map(|(lane, _)| lane).collect(),
                                rules: existing.clone(),
                            });
                        }
                    }
                }
            }

            let (samples, topology) =
                self.prepare_resolved_connection_inner(from, to, elbow, &pattern, false, work)?;
            work.spend_profile_samples(samples);
            let topology = topology.expect("WATER stages a walking path");
            if topology.walking.as_ref().map(|edit| edit.sides) != Some(sides) {
                return Err(ConnectionError::InvalidEndpoint);
            }
            Ok(PreparedWalkingPath {
                from,
                to,
                elbow,
                topology,
                rules,
            })
        }

        pub(crate) fn commit_walking_path(
            &mut self,
            path: PreparedWalkingPath,
        ) -> Result<ConnectedRoad, ConnectionError> {
            let PreparedWalkingPath {
                from,
                to,
                elbow,
                mut topology,
                rules,
            } = path;
            let approaches = std::mem::take(
                &mut topology
                    .walking
                    .as_mut()
                    .expect("prepared walking topology")
                    .approaches,
            );
            let pattern = SmallRoadPreset::WalkingPath.lane_pattern();
            let connected =
                self.commit_prepared_connection(from, to, elbow, &pattern, Some(topology))?;
            let child_road = |road, child: usize| {
                connected
                    .splits
                    .iter()
                    .flatten()
                    .find(|s| s.original == road)
                    .expect("prepared split committed")
                    .roads[child]
            };
            let mut touched = BTreeSet::new();
            for patch in approaches {
                let id = patch
                    .child
                    .map_or(patch.road, |child| child_road(patch.road, child));
                let road = &self.roads()[id];
                let (lane, _) = road
                    .lanes_iter()
                    .nth(patch.ordinal)
                    .expect("prepared lane ordinal");
                touched.extend([road.src, road.dst]);
                self.lanes[lane].points = patch.points;
            }
            for joint in touched {
                self.intersections[joint].update_turns(&self.lanes, &self.roads);
                let rules = self.junction_rules(joint).cloned();
                self.intersections[joint].apply_turn_mask(rules.as_ref());
            }
            // The prepared comparison has already verified the full movement set.
            // Translate both pieces together when two roads share a ruled junction.
            let mut translations = BTreeMap::new();
            for split in rules {
                let entry = translations
                    .entry(split.joint)
                    .or_insert_with(|| (split.rules, BTreeMap::new(), BTreeMap::new()));
                let child = child_road(split.road, split.child);
                entry.1.insert(split.road, child);
                entry.2.extend(
                    split
                        .lanes
                        .into_iter()
                        .zip(self.roads()[child].lanes_iter().map(|(id, _)| id)),
                );
            }
            for (joint, (rules, roads, lanes)) in translations {
                let lane = |id| lanes.get(&id).copied().unwrap_or(id);
                let rules = JunctionRules::new(
                    rules
                        .approaches()
                        .iter()
                        .map(|&(id, role)| (roads.get(&id).copied().unwrap_or(id), role))
                        .collect(),
                    rules
                        .forbidden()
                        .iter()
                        .map(|t| TurnID::new(joint, lane(t.src), lane(t.dst), t.bidirectional))
                        .collect(),
                )
                .expect("prepared lane correspondence is one-to-one");
                self.install_junction_rules(joint, rules);
            }
            self.check_invariants();
            Ok(connected)
        }
    }

    impl PreparedWalkingPath {
        pub(crate) fn split_roads(&self) -> impl Iterator<Item = RoadID> {
            [self.from.kind, self.to.kind]
                .into_iter()
                .filter_map(|kind| {
                    if let ProjectKind::Road(road) = kind {
                        Some(road)
                    } else {
                        None
                    }
                })
        }

        pub(crate) fn positions(&self) -> (Vec3, Vec3) {
            (self.from.pos, self.to.pos)
        }

        pub(crate) fn preview(self) -> super::RoadPreview {
            self.topology.preview
        }
    }

    fn point_at([x, y]: [i32; 2]) -> Vec2 {
        vec2(x as f32 * 0.01, y as f32 * 0.01)
    }
}
