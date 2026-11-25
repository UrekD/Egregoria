use crate::economy::Government;
use crate::map::{
    ConnectionError, LaneID, LaneKind, LanePattern, LanePatternBuilder, Map, PreparedEffects,
    ProjectFilter, ProjectKind, Road, RoadID,
};
use crate::world_command::{PrepareBudget, Rejection};
use crate::Simulation;
use geom::{PolyLine3, AABB};
use prototypes::Money;
use serde::{Deserialize, Serialize};
use slotmapd::Key;

const MAX_PROFILE_POINTS: usize = 65_536;
const MAX_LANES: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoadEditStamp(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoadQuote(u64);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum RoadUpgrade {
    Replace(LanePattern),
    ReverseOneWay,
}

#[derive(Clone, Debug)]
pub enum RoadEditRequest {
    Upgrade {
        road: RoadID,
        expected: RoadEditStamp,
        change: RoadUpgrade,
    },
}

#[derive(Clone, Debug)]
pub struct RoadEditPreview {
    pub road: RoadID,
    pub stamp: RoadEditStamp,
    pub quote: RoadQuote,
    pub cost: Money,
    pub deck: PolyLine3,
    pub retained_lanes: Vec<LaneID>,
    pub attached_buildings: usize,
    pub attached_lots: usize,
    pub changed_lane_costs: usize,
    /// Present for geometry-changing requests while live handoffs are gated.
    pub projected_topology: Option<RoadTopologyPreview>,
}

#[derive(Clone, Debug)]
pub struct RoadTopologyPreview {
    pub junctions: Vec<Vec<geom::Vec3>>,
    pub crosses_water: bool,
    pub affected_roads: Vec<RoadID>,
    pub omitted_roads: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoadEditStatus {
    Applied,
    NoChange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditReceipt {
    pub road: RoadID,
    pub status: RoadEditStatus,
    pub stamp: RoadEditStamp,
    pub quote: RoadQuote,
    pub debit: Money,
    pub retained_lanes: Vec<LaneID>,
    pub changed_lane_costs: usize,
}

/// A material stamp is reproducible after loading the same road or replaying its creation.
pub fn road_edit_stamp(
    sim: &Simulation,
    road: RoadID,
    budget: &mut PrepareBudget,
) -> Result<RoadEditStamp, Rejection> {
    let map = sim.map();
    let road = map.get(road).ok_or(Rejection::MissingReference)?;
    reserve_target(road, budget)?;
    stamp(&map, road, &[])
}

pub fn preview_road_edit(
    sim: &Simulation,
    request: &RoadEditRequest,
    budget: &mut PrepareBudget,
) -> Result<RoadEditPreview, Rejection> {
    let RoadEditRequest::Upgrade {
        road,
        expected,
        change,
    } = request;
    Ok(prepare_upgrade(sim, *road, *expected, change, budget)?.preview)
}

pub(crate) struct PreparedRoadEdit {
    pub(crate) preview: RoadEditPreview,
    map_identity: u64,
    topology_revision: (u64, u64, u64, u64, usize),
    lane_edits: Vec<(LaneID, f32)>,
    new_stamp: RoadEditStamp,
}

impl PreparedRoadEdit {
    pub(crate) fn commit(self, sim: &mut Simulation) -> Result<EditReceipt, Rejection> {
        let Self {
            preview,
            map_identity,
            topology_revision,
            lane_edits,
            new_stamp,
        } = self;
        if preview.projected_topology.is_some() {
            return Err(Rejection::RoadTopologyUnavailable);
        }
        let mut map = sim.map_mut();
        if map.identity() != map_identity || map.road_preview_revision() != topology_revision {
            return Err(Rejection::StaleRoadEdit);
        }
        let road = map.get(preview.road).ok_or(Rejection::StaleRoadEdit)?;
        if stamp(&map, road, &[])? != preview.stamp {
            return Err(Rejection::StaleRoadEdit);
        }
        if preview.cost > Money::ZERO && sim.read::<Government>().money < preview.cost {
            return Err(Rejection::InsufficientFunds);
        }
        for (id, speed) in lane_edits {
            map.lanes[id].speed_limit = speed;
        }
        Ok(EditReceipt {
            road: preview.road,
            status: if preview.changed_lane_costs == 0 {
                RoadEditStatus::NoChange
            } else {
                RoadEditStatus::Applied
            },
            stamp: new_stamp,
            quote: preview.quote,
            debit: preview.cost,
            retained_lanes: preview.retained_lanes,
            changed_lane_costs: preview.changed_lane_costs,
        })
    }
}

pub(crate) fn prepare_upgrade(
    sim: &Simulation,
    id: RoadID,
    expected: RoadEditStamp,
    change: &RoadUpgrade,
    budget: &mut PrepareBudget,
) -> Result<PreparedRoadEdit, Rejection> {
    if let RoadUpgrade::Replace(pattern) = change {
        validate_pattern(pattern)?;
    }
    let map = sim.map();
    let road = map.get(id).ok_or(Rejection::MissingReference)?;
    reserve_target(road, budget)?;
    let current = stamp(&map, road, &[])?;
    if current != expected {
        return Err(Rejection::StaleRoadEdit);
    }
    let mut existing = road.pattern(map.lanes());
    validate_pattern_layout(&existing)?;
    if map.land_ownership().is_tiled() {
        authorize_road_deck(&map, road, budget)?;
    }
    let pattern = match change {
        RoadUpgrade::Replace(pattern) => pattern,
        RoadUpgrade::ReverseOneWay => {
            let carries_vehicles = |lanes: &[(LaneKind, f32)]| {
                lanes.iter().any(|&(kind, _)| kind == LaneKind::Driving)
            };
            if carries_vehicles(&existing.lanes_forward)
                == carries_vehicles(&existing.lanes_backward)
            {
                return Err(Rejection::InvalidRoadPattern);
            }
            std::mem::swap(&mut existing.lanes_forward, &mut existing.lanes_backward);
            validate_pattern(&existing)?;
            &existing
        }
    };
    let pattern_matches = |lanes: &[(LaneID, LaneKind)], requested: &[(LaneKind, f32)]| {
        lanes.len() == requested.len()
            && lanes
                .iter()
                .zip(requested)
                .all(|((_, current), (kind, _))| current == kind)
    };
    let [forward, backward] = road.directed_lanes();
    let same_geometry = pattern_matches(forward, &pattern.lanes_forward)
        && pattern_matches(backward, &pattern.lanes_backward);
    let (deck, projected_topology, material, retained_lanes, lane_edits) = if same_geometry {
        let retained = road.lanes_iter().map(|(id, _)| id).collect::<Vec<_>>();
        let edits = forward
            .iter()
            .chain(backward)
            .zip(pattern.lanes_forward.iter().chain(&pattern.lanes_backward))
            .filter_map(|(&(id, _), &(_, speed))| {
                (map.lanes[id].speed_limit != speed).then_some((id, speed))
            })
            .collect::<Vec<_>>();
        (road.interfaced_points().clone(), None, 0, retained, edits)
    } else {
        let projection =
            super::prepare_road_replacement(&map, id, pattern, budget.road_work_remaining())
                .map_err(|error| match error {
                    ConnectionError::WorkLimit => Rejection::RoadWorkLimit,
                    ConnectionError::LandDenied(reason) => Rejection::LandDenied(reason),
                    error => Rejection::WaterConnection(error),
                })?;
        let topology = RoadTopologyPreview {
            junctions: projection.preview.junctions,
            crosses_water: projection.preview.crosses_water,
            affected_roads: projection.affected_roads,
            omitted_roads: projection.affected_roads_omitted,
        };
        let pattern_key = common::hash_iter(
            pattern
                .lanes()
                .map(|(kind, dir, speed)| (kind as u8, dir as u8, speed.to_bits())),
        );
        (
            projection.preview.deck,
            Some(topology),
            common::hash_u64((projection.material, pattern_key)),
            Vec::new(),
            Vec::new(),
        )
    };
    let cost = if same_geometry && lane_edits.is_empty() {
        Money::ZERO
    } else {
        let length = road
            .points()
            .segments()
            .map(|s| s.src.distance(s.dst))
            .sum();
        Government::connection_cost_at_length(length, pattern).ok_or(Rejection::CostOverflow)?
    };
    if cost > Money::ZERO && sim.read::<Government>().money < cost {
        return Err(Rejection::InsufficientFunds);
    }
    let new_stamp = stamp(&map, road, &lane_edits)?;
    let attached_buildings = road.connected_buildings.len();
    let attached_lots = map.road_lot_count(id);
    let quote = RoadQuote(common::hash_u64((
        id,
        current.0,
        new_stamp.0,
        material,
        cost.inner(),
        attached_buildings,
        attached_lots,
    )));
    Ok(PreparedRoadEdit {
        preview: RoadEditPreview {
            road: id,
            stamp: current,
            quote,
            cost,
            deck,
            retained_lanes,
            attached_buildings,
            attached_lots,
            changed_lane_costs: lane_edits.len(),
            projected_topology,
        },
        map_identity: map.identity(),
        topology_revision: map.road_preview_revision(),
        lane_edits,
        new_stamp,
    })
}

fn authorize_road_deck(
    map: &Map,
    road: &Road,
    budget: &mut PrepareBudget,
) -> Result<(), Rejection> {
    map.mutable_object(road.id.into())?;
    for triangle in super::land_deck_triangles(road) {
        let points = triangle.map(|p| p.xy());
        let [a, b, c] = points;
        let bounds = AABB::new_ll_ur(a.min(b).min(c), a.max(b).max(c));
        let span = (bounds.ur - bounds.ll) / super::LAND_TILE_SIZE;
        budget.spend_road_work(
            (span.x.ceil() as usize)
                .saturating_add(2)
                .saturating_mul((span.y.ceil() as usize).saturating_add(2)),
        )?;
        // Pay for both the reservation pass and the authority's visit to the
        // same broad-phase cells, including candidates its filter rejects.
        let spatial = &map.spatial_map;
        spatial
            .reserve_construction_query(bounds, budget.road_work_remaining())
            .map_err(|_| Rejection::RoadWorkLimit)?;
        for candidate in spatial.construction_candidates(
            bounds,
            ProjectFilter::ROAD | ProjectFilter::INTER | ProjectFilter::BUILDING,
        ) {
            if !map.land_ownership().is_protected(candidate) {
                continue;
            }
            let geometry = match candidate {
                ProjectKind::Road(id) => {
                    map.roads()[id].interfaced_points().len().saturating_mul(2)
                }
                ProjectKind::Intersection(id) => map.intersections()[id].roads.len(),
                ProjectKind::Building(id) => map.buildings()[id].zone.as_ref().map_or(4, |zone| {
                    4usize.saturating_add(zone.poly.len().saturating_pow(2))
                }),
                _ => unreachable!("construction filter"),
            };
            budget.spend_road_work(geometry)?;
        }
        map.authorize_land_effects(PreparedEffects::Footprint(&points))?;
    }
    Ok(())
}

fn reserve_target(road: &Road, budget: &mut PrepareBudget) -> Result<(), Rejection> {
    let points = road
        .points()
        .len()
        .saturating_add(road.interfaced_points().len());
    if points > MAX_PROFILE_POINTS || road.n_lanes() > MAX_LANES {
        return Err(Rejection::RoadWorkLimit);
    }
    // The journal rechecks the stamp and clones the clipped deck before returning it.
    budget.spend_road_work(points * 5 + road.n_lanes() * 5 + 1)
}

fn stamp(map: &Map, road: &Road, edit: &[(LaneID, f32)]) -> Result<RoadEditStamp, Rejection> {
    let geometry = |points: &PolyLine3| {
        common::hash_iter(
            points
                .iter()
                .map(|p| (p.x.to_bits(), p.y.to_bits(), p.z.to_bits())),
        )
    };
    let mut lanes = Vec::with_capacity(road.n_lanes());
    for (id, kind) in road.lanes_iter() {
        let lane = map.get(id).ok_or(Rejection::MissingReference)?;
        let speed = edit
            .iter()
            .find(|&&(key, _)| key == id)
            .map_or(lane.speed_limit, |&(_, speed)| speed);
        lanes.push((
            id.data().as_ffi(),
            kind as u8,
            lane.src.data().as_ffi(),
            lane.dst.data().as_ffi(),
            speed.to_bits(),
        ));
    }
    Ok(RoadEditStamp(common::hash_u64((
        road.id.data().as_ffi(),
        road.src.data().as_ffi(),
        road.dst.data().as_ffi(),
        road.width.to_bits(),
        road.elevated_span(),
        geometry(road.points()),
        geometry(road.interfaced_points()),
        &lanes,
    ))))
}

fn validate_pattern(pattern: &LanePattern) -> Result<(), Rejection> {
    let count = pattern
        .lanes_forward
        .len()
        .saturating_add(pattern.lanes_backward.len());
    if count > MAX_LANES {
        return Err(Rejection::RoadWorkLimit);
    }
    if pattern
        .lanes()
        .any(|(_, _, speed)| !(4.0..=40.0).contains(&speed))
    {
        return Err(Rejection::InvalidRoadPattern);
    }
    validate_pattern_layout(pattern)
}

// Legacy roads may have speeds outside the live knob range. A replacement
// can bring a supported road layout into range without converting its type.
fn validate_pattern_layout(pattern: &LanePattern) -> Result<(), Rejection> {
    let matches = |a: &[(LaneKind, f32)], b: &[(LaneKind, f32)]| {
        a.iter()
            .map(|&(kind, _)| kind)
            .eq(b.iter().map(|&(kind, _)| kind))
    };
    for reversed in [false, true] {
        let (forward, backward) = if reversed {
            (&pattern.lanes_backward, &pattern.lanes_forward)
        } else {
            (&pattern.lanes_forward, &pattern.lanes_backward)
        };
        let driving = forward
            .iter()
            .take_while(|&&(kind, _)| kind == LaneKind::Driving)
            .count();
        if !(1..=3).contains(&driving) {
            continue;
        }
        let candidate = LanePatternBuilder::new()
            .n_lanes(driving as u32)
            .one_way(!backward.iter().any(|&(kind, _)| kind == LaneKind::Driving))
            .parking(forward.iter().any(|&(kind, _)| kind == LaneKind::Parking))
            .sidewalks(forward.iter().any(|&(kind, _)| kind == LaneKind::Walking))
            .build();
        if matches(forward, &candidate.lanes_forward)
            && matches(backward, &candidate.lanes_backward)
        {
            return Ok(());
        }
    }
    Err(Rejection::InvalidRoadPattern)
}
