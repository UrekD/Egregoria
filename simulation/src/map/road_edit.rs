use crate::economy::Government;
use crate::map::{
    ConnectionError, IntersectionID, JunctionRules, Lane, LaneDirection, LaneID, LaneKind,
    LanePattern, LanePatternBuilder, Map, PreparedEffects, ProjectFilter, ProjectKind, Road,
    RoadID, TraverseKind, TurnID, MAX_CONFIGURED_JUNCTIONS,
};
use crate::transportation::junction_admission::JunctionAdmission;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JunctionEditStamp(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JunctionEditPreview {
    pub junction: IntersectionID,
    pub stamp: JunctionEditStamp,
    pub quote: RoadQuote,
    pub cost: Money,
    pub changed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JunctionEditReceipt {
    pub junction: IntersectionID,
    pub stamp: JunctionEditStamp,
    pub quote: RoadQuote,
    pub status: RoadEditStatus,
}

pub(crate) struct PreparedJunctionEdit {
    pub(crate) preview: JunctionEditPreview,
    rules: JunctionRules,
    map_identity: u64,
    policy_revision: u64,
}

impl PreparedJunctionEdit {
    pub(crate) fn commit(self, sim: &mut Simulation) -> Result<JunctionEditReceipt, Rejection> {
        let mut map = sim.map_mut();
        let junction = self.preview.junction;
        if map.identity() != self.map_identity
            || map.junction_policy_revision() != self.policy_revision
            || junction_stamp(&map, junction, map.junction_rules(junction))? != self.preview.stamp
        {
            return Err(Rejection::StaleRoadEdit);
        }
        let status = if self.preview.changed {
            map.install_junction_rules(junction, self.rules);
            sim.write::<JunctionAdmission>().reset_junction(junction);
            RoadEditStatus::Applied
        } else {
            RoadEditStatus::NoChange
        };
        let stamp = junction_stamp(&map, junction, map.junction_rules(junction))?;
        Ok(JunctionEditReceipt {
            junction,
            stamp,
            quote: self.preview.quote,
            status,
        })
    }
}

pub fn junction_edit_stamp(
    sim: &Simulation,
    junction: IntersectionID,
    budget: &mut PrepareBudget,
) -> Result<JunctionEditStamp, Rejection> {
    let map = sim.map();
    reserve_junction(&map, junction, budget)?;
    junction_stamp(&map, junction, map.junction_rules(junction))
}

pub fn preview_junction_edit(
    sim: &Simulation,
    junction: IntersectionID,
    expected: JunctionEditStamp,
    rules: &JunctionRules,
    budget: &mut PrepareBudget,
) -> Result<JunctionEditPreview, Rejection> {
    Ok(prepare_junction_edit(sim, junction, expected, rules, budget)?.preview)
}

pub(crate) fn prepare_junction_edit(
    sim: &Simulation,
    junction: IntersectionID,
    expected: JunctionEditStamp,
    rules: &JunctionRules,
    budget: &mut PrepareBudget,
) -> Result<PreparedJunctionEdit, Rejection> {
    let map = sim.map();
    reserve_junction(&map, junction, budget)?;
    let previous = map.junction_rules(junction);
    let stamp = junction_stamp(&map, junction, previous)?;
    if stamp != expected {
        return Err(Rejection::StaleRoadEdit);
    }
    map.authorize_land_effects(PreparedEffects::Junction(junction))?;
    rules
        .validate(&map, junction)
        .map_err(Rejection::InvalidJunctionRules)?;
    let changed = previous.map_or(!rules.is_empty(), |previous| previous != rules);
    if changed {
        if previous.is_none() && map.junction_rule_count() >= MAX_CONFIGURED_JUNCTIONS {
            return Err(Rejection::RoadWorkLimit);
        }
        let old_roles = previous.map_or(&[][..], JunctionRules::approaches);
        let old_mask = previous.map_or(&[][..], JunctionRules::forbidden);
        let new_mask: Vec<_> = rules
            .forbidden()
            .iter()
            .copied()
            .filter(|turn| old_mask.binary_search(turn).is_err())
            .collect();
        let roles_changed = old_roles != rules.approaches();
        rules.check_rail_sweeps(&map, junction, budget)?;
        verify_junction_handoff(sim, &map, junction, rules, &new_mask, roles_changed, budget)?;
    }
    let next = junction_stamp(&map, junction, (!rules.is_empty()).then_some(rules))?;
    let quote = RoadQuote(common::hash_u64((junction, stamp.0, next.0)));
    Ok(PreparedJunctionEdit {
        preview: JunctionEditPreview {
            junction,
            stamp,
            quote,
            cost: Money::ZERO,
            changed,
        },
        rules: rules.clone(),
        map_identity: map.identity(),
        policy_revision: map.junction_policy_revision(),
    })
}

pub(super) fn reserve_junction(
    map: &Map,
    junction: IntersectionID,
    budget: &mut PrepareBudget,
) -> Result<(), Rejection> {
    let intersection = map.get(junction).ok_or(Rejection::MissingReference)?;
    if intersection.roads.len() > 32 || intersection.raw_turns().len() > 4096 {
        return Err(Rejection::RoadWorkLimit);
    }
    budget.spend_road_work(intersection.roads.len() * MAX_LANES + 1)?;
    let mut points = 0usize;
    for turn in intersection.raw_turns() {
        points = points.saturating_add(turn.points.len());
        if points > MAX_PROFILE_POINTS {
            return Err(Rejection::RoadWorkLimit);
        }
        budget.spend_road_work(1 + turn.points.len().saturating_mul(4))?;
    }
    Ok(())
}

fn junction_stamp(
    map: &Map,
    junction: IntersectionID,
    rules: Option<&JunctionRules>,
) -> Result<JunctionEditStamp, Rejection> {
    let intersection = map.get(junction).ok_or(Rejection::MissingReference)?;
    let graph = common::hash_iter(intersection.raw_turns().map(|turn| {
        (
            turn.id,
            turn.kind as u8,
            common::hash_iter(
                turn.points
                    .iter()
                    .map(|p| (p.x.to_bits(), p.y.to_bits(), p.z.to_bits())),
            ),
        )
    }));
    Ok(JunctionEditStamp(common::hash_u64((
        junction,
        &intersection.roads,
        intersection.light_policy as u8,
        graph,
        rules,
    ))))
}

fn verify_junction_handoff(
    sim: &Simulation,
    map: &Map,
    junction: IntersectionID,
    rules: &JunctionRules,
    new_mask: &[TurnID],
    roles_changed: bool,
    budget: &mut PrepareBudget,
) -> Result<(), Rejection> {
    let admission = sim.read::<JunctionAdmission>();
    budget.spend_road_work(
        admission
            .crossing_count()
            .saturating_mul(new_mask.len().max(1)),
    )?;
    if (roles_changed && admission.crossing_junction(junction))
        || new_mask.iter().any(|&turn| admission.crossing_turn(turn))
    {
        return Err(Rejection::BusyConflict);
    }
    let world = sim.world();
    budget.spend_road_work(world.vehicles.len().saturating_mul(new_mask.len().max(1)))?;
    for (_, vehicle) in world.vehicles.iter() {
        let extent = vehicle.vehicle.kind.width() * 0.5;
        if occupied_junction_edit(
            map,
            junction,
            &vehicle.it,
            vehicle.trans.pos,
            extent,
            roles_changed,
            new_mask,
        ) {
            return Err(Rejection::BusyConflict);
        }
    }
    if roles_changed {
        let intersection = map.get(junction).ok_or(Rejection::MissingReference)?;
        for &road in &intersection.roads {
            map.mutable_object(road.into())?;
        }
        budget.spend_road_work(world.humans.len())?;
        if world.humans.values().any(|human| {
            human.location == crate::transportation::Location::Outside
                && occupied_junction_edit(
                    map,
                    junction,
                    &human.it,
                    human.trans.pos,
                    0.3,
                    true,
                    new_mask,
                )
        }) {
            return Err(Rejection::BusyConflict);
        }
    }
    // Preserve service reachability before admitting any edit to its graph.
    // Replacing each removed edge with a legal directed path preserves all
    // existing route pairs, including lines whose vehicles are currently idle.
    for &turn in new_mask {
        for lane in [turn.src, turn.dst] {
            let road = map.get(lane).ok_or(Rejection::MissingReference)?.parent;
            map.mutable_object(road.into())?;
        }
        if (!sim.read::<crate::map_dynamic::Transit>().lines.is_empty()
            || !world.companies.is_empty()
            || !world.freight_stations.is_empty())
            && !has_directed_alternative(map, turn, rules, budget)?
        {
            return Err(Rejection::BusyConflict);
        }
    }
    Ok(())
}

fn occupied_junction_edit(
    map: &Map,
    junction: IntersectionID,
    itinerary: &crate::map_dynamic::Itinerary,
    position: geom::Vec3,
    extent: f32,
    roles_changed: bool,
    new_mask: &[TurnID],
) -> bool {
    let affected = |turn: TurnID| {
        turn.parent == junction && (roles_changed || new_mask.binary_search(&turn).is_ok())
    };
    match itinerary.get_travers().map(|path| path.kind) {
        Some(TraverseKind::Turn(turn)) if affected(turn) => return true,
        Some(TraverseKind::Lane(id))
            if roles_changed || new_mask.iter().any(|turn| turn.dst == id) =>
        {
            if let Some(lane) = map.get(id).filter(|lane| lane.src == junction) {
                if lane.points.first().is_close(position, extent) {
                    return true;
                }
            }
        }
        _ => {}
    }
    itinerary
        .get_route()
        .and_then(|route| route.reversed_route.last())
        .is_some_and(|next| {
            matches!(next.kind, TraverseKind::Turn(turn) if affected(turn))
                && itinerary
                    .local_path()
                    .first()
                    .is_some_and(|point| point.is_close(position, extent))
        })
}

fn has_directed_alternative(
    map: &Map,
    removed: TurnID,
    rules: &JunctionRules,
    budget: &mut PrepareBudget,
) -> Result<bool, Rejection> {
    use std::collections::{BTreeSet, VecDeque};

    let mut visited = BTreeSet::from([removed.src]);
    let mut pending = VecDeque::from([removed.src]);
    while let Some(id) = pending.pop_front() {
        budget.spend_road_work(1)?;
        let lane = map.get(id).ok_or(Rejection::MissingReference)?;
        let inter = map.get(lane.dst).ok_or(Rejection::MissingReference)?;
        for turn in inter.raw_turns() {
            budget.spend_road_work(1)?;
            if turn.id.src != id
                || turn.kind != super::TurnKind::Driving
                || if turn.id.parent == removed.parent {
                    !rules.allows(turn.id)
                } else {
                    !map.allows_turn(turn.id)
                }
            {
                continue;
            }
            if turn.id.dst == removed.dst {
                return Ok(true);
            }
            if visited.insert(turn.id.dst) {
                pending.push_back(turn.id.dst);
            }
        }
    }
    Ok(false)
}

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
    JunctionRules {
        junction: IntersectionID,
        expected: JunctionEditStamp,
        rules: JunctionRules,
    },
}

#[derive(Clone, Debug)]
pub enum RoadEditPreview {
    Upgrade(RoadUpgradePreview),
    JunctionRules(JunctionEditPreview),
}

#[derive(Clone, Debug)]
pub struct RoadUpgradePreview {
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
    match request {
        RoadEditRequest::Upgrade {
            road,
            expected,
            change,
        } => preview_road_upgrade(sim, *road, *expected, change, budget)
            .map(RoadEditPreview::Upgrade),
        RoadEditRequest::JunctionRules {
            junction,
            expected,
            rules,
        } => preview_junction_edit(sim, *junction, *expected, rules, budget)
            .map(RoadEditPreview::JunctionRules),
    }
}

pub fn preview_road_upgrade(
    sim: &Simulation,
    road: RoadID,
    expected: RoadEditStamp,
    change: &RoadUpgrade,
    budget: &mut PrepareBudget,
) -> Result<RoadUpgradePreview, Rejection> {
    Ok(prepare_upgrade(sim, road, expected, change, budget)?.preview)
}

pub(crate) struct PreparedRoadEdit {
    pub(crate) preview: RoadUpgradePreview,
    map_identity: u64,
    topology_revision: (u64, u64, u64, u64, usize),
    lane_edits: Vec<(LaneID, f32)>,
    direction_edits: Option<DirectionEdits>,
    new_stamp: RoadEditStamp,
}

struct DirectionEdits {
    remove: Vec<LaneID>,
    lanes: Vec<(LaneDirection, LanePlacement)>,
}

enum LanePlacement {
    Retain { id: LaneID, kind: LaneKind },
    Add(Lane),
}

impl DirectionEdits {
    fn commit(self, map: &mut Map, road: RoadID) {
        for id in self.remove {
            if map.lanes[id].kind == LaneKind::Parking {
                map.parking.remove_spots(id);
            }
            map.lanes.remove(id);
        }
        let lanes = self.lanes.into_iter().map(|(direction, placement)| {
            let (id, kind) = match placement {
                LanePlacement::Retain { id, kind } => (id, kind),
                LanePlacement::Add(mut lane) => {
                    let kind = lane.kind;
                    let id = map.lanes.insert_with_key(|id| {
                        lane.id = id;
                        lane
                    });
                    if kind == LaneKind::Parking {
                        map.parking.generate_spots(&map.lanes[id]);
                    }
                    (id, kind)
                }
            };
            (id, kind, direction)
        });
        map.roads[road].install_lane_keys(lanes);
        map.refresh_driving_road(road);
    }
}

impl PreparedRoadEdit {
    pub(crate) fn commit(self, sim: &mut Simulation) -> Result<EditReceipt, Rejection> {
        let Self {
            preview,
            map_identity,
            topology_revision,
            lane_edits,
            direction_edits,
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
        let status = if direction_edits.is_some() || preview.changed_lane_costs != 0 {
            RoadEditStatus::Applied
        } else {
            RoadEditStatus::NoChange
        };
        let new_stamp = if let Some(edits) = direction_edits {
            edits.commit(&mut map, preview.road);
            stamp(&map, &map.roads[preview.road], &[])
                .expect("committed lane plan contains live lane keys")
        } else {
            new_stamp
        };
        Ok(EditReceipt {
            road: preview.road,
            status,
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
    if road.is_walking_path() {
        return Err(Rejection::InvalidRoadPattern);
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
    if !same_geometry
        && [road.src, road.dst].into_iter().any(|id| {
            map.get(id).is_some_and(|joint| {
                joint
                    .roads
                    .iter()
                    .any(|&arm| map.get(arm).is_some_and(Road::is_walking_path))
            })
        })
    {
        return Err(Rejection::StaleRoadEdit);
    }
    let mut direction_edits = None;
    let local_reversal = matches!(change, RoadUpgrade::ReverseOneWay) && {
        let curb_parking = pattern
            .lanes()
            .any(|(kind, _, _)| kind == LaneKind::Parking);
        let local = super::SmallRoadPreset::LocalOneWay { curb_parking }.lane_pattern();
        let same_kinds = |a: &[(LaneKind, f32)], b: &[(LaneKind, f32)]| {
            a.iter()
                .map(|(kind, _)| kind)
                .eq(b.iter().map(|(kind, _)| kind))
        };
        (same_kinds(&pattern.lanes_forward, &local.lanes_forward)
            && same_kinds(&pattern.lanes_backward, &local.lanes_backward))
            || (same_kinds(&pattern.lanes_forward, &local.lanes_backward)
                && same_kinds(&pattern.lanes_backward, &local.lanes_forward))
    };
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
    } else if let Some((plan, retained, edits)) =
        prepare_same_width_directions(sim, &map, road, pattern, local_reversal, budget)?
    {
        let material = common::hash_iter(
            pattern
                .lanes()
                .map(|(kind, direction, speed)| (kind as u8, direction as u8, speed.to_bits())),
        );
        direction_edits = Some(plan);
        (
            road.interfaced_points().clone(),
            None,
            material,
            retained,
            edits,
        )
    } else {
        if local_reversal {
            return Err(Rejection::RoadTopologyUnavailable);
        }
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
        preview: RoadUpgradePreview {
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
        direction_edits,
        new_stamp,
    })
}

fn prepare_same_width_directions(
    sim: &Simulation,
    map: &Map,
    road: &Road,
    pattern: &LanePattern,
    local_reversal: bool,
    budget: &mut PrepareBudget,
) -> Result<Option<(DirectionEdits, Vec<LaneID>, Vec<(LaneID, f32)>)>, Rejection> {
    // Local reversing keeps the deck and sidewalk lanes. Its curb lane can
    // move to the opposite side once all bays are clear.
    if road.width != pattern.width()
        || (!local_reversal
            && (!road.connected_buildings.is_empty()
                || !sim.read::<crate::map_dynamic::Transit>().lines.is_empty()
                || road.lanes_iter().any(|(_, kind)| kind == LaneKind::Parking)
                || pattern
                    .lanes()
                    .any(|(kind, _, _)| kind == LaneKind::Parking)
                || !sim.world().companies.is_empty()
                || !sim.world().freight_stations.is_empty()))
    {
        return Ok(None);
    }
    let replaceable =
        |kind| kind == LaneKind::Driving || (local_reversal && kind == LaneKind::Parking);
    for id in [road.src, road.dst] {
        if map.junction_rules(id).is_some() {
            return Ok(None);
        }
        map.authorize_land_effects(PreparedEffects::Junction(id))?;
        reserve_junction(map, id, budget)?;
        // Reserve turn-generation candidates even where no movement is made.
        let intersection = map.get(id).ok_or(Rejection::MissingReference)?;
        let mut lanes = MAX_LANES;
        for &id in &intersection.roads {
            let neighbor = map.get(id).ok_or(Rejection::MissingReference)?;
            if neighbor.lanes_iter().any(|(_, kind)| kind.is_rail()) {
                return Ok(None);
            }
            lanes += neighbor.n_lanes();
        }
        if lanes > 128 {
            return Err(Rejection::RoadWorkLimit);
        }
        budget.spend_road_work(lanes * lanes * 16)?;
    }

    let mut retained = Vec::new();
    let mut speed_edits = Vec::new();
    let mut lanes = Vec::new();
    let mut offset = 0.0;
    for (kind, direction, speed) in pattern.lanes() {
        let (src, dst) = match direction {
            LaneDirection::Forward => (road.src, road.dst),
            LaneDirection::Backward => (road.dst, road.src),
        };
        let existing = road.lanes_iter().find_map(|(id, _)| {
            map.get(id).filter(|lane| {
                lane.kind == kind
                    && lane.src == src
                    && lane.dst == dst
                    && lane.dist_from_bottom == offset
            })
        });
        let placement = if let Some(existing) = existing {
            retained.push(existing.id);
            if existing.speed_limit != speed {
                speed_edits.push((existing.id, speed));
            }
            LanePlacement::Retain {
                id: existing.id,
                kind,
            }
        } else {
            if !replaceable(kind) {
                return Ok(None);
            }
            budget.spend_road_work(road.interfaced_points().len() * 2)?;
            let mut lane = Lane {
                id: LaneID::default(),
                parent: road.id,
                src,
                dst,
                kind,
                control: super::TrafficControl::Always,
                speed_limit: speed,
                points: road.interfaced_points().clone(),
                dist_from_bottom: offset,
            };
            lane.gen_pos(road);
            if kind == LaneKind::Parking {
                budget.spend_road_work(
                    (lane.points.length() / super::PARKING_SPOT_LENGTH).ceil() as usize,
                )?;
            }
            LanePlacement::Add(lane)
        };
        offset += kind.width();
        lanes.push((direction, placement));
    }
    let mut remove = Vec::new();
    for (id, kind) in road.lanes_iter().filter(|(id, _)| !retained.contains(id)) {
        if !replaceable(kind) {
            return Ok(None);
        }
        if kind == LaneKind::Parking {
            let parking = sim.read::<crate::map_dynamic::ParkingManagement>();
            if let Some(spots) = map.parking.lane_spots.get(id) {
                budget.spend_road_work(spots.len())?;
                if spots.iter().any(|&spot| !parking.is_spot_free(spot)) {
                    return Err(Rejection::BusyConflict);
                }
            }
        }
        remove.push(id);
    }
    verify_direction_handoff(sim, road, &remove, budget)?;
    Ok(Some((
        DirectionEdits { remove, lanes },
        retained,
        speed_edits,
    )))
}

fn service_path_survives(
    map: &Map,
    from: (LaneID, f32),
    to: (LaneID, f32),
    removed: &[LaneID],
    budget: &mut PrepareBudget,
) -> Result<bool, Rejection> {
    use std::collections::{BTreeSet, VecDeque};

    if removed.contains(&from.0) || removed.contains(&to.0) {
        return Ok(false);
    }
    if from.0 == to.0 && from.1 <= to.1 {
        return Ok(true);
    }
    let mut visited = BTreeSet::from([from.0]);
    let mut frontier = VecDeque::from([from.0]);
    while let Some(lane) = frontier.pop_front() {
        budget.spend_road_work(1)?;
        let joint = map.get(map.get(lane).ok_or(Rejection::MissingReference)?.dst)
            .ok_or(Rejection::MissingReference)?;
        for turn in joint.raw_turns() {
            budget.spend_road_work(1)?;
            if turn.id.src != lane
                || turn.kind != super::TurnKind::Driving
                || removed.contains(&turn.id.dst)
                || !map.allows_turn(turn.id)
            {
                continue;
            }
            if turn.id.dst == to.0 {
                return Ok(true);
            }
            if visited.insert(turn.id.dst) {
                frontier.push_back(turn.id.dst);
            }
        }
    }
    Ok(false)
}

fn verify_direction_handoff(
    sim: &Simulation,
    road: &Road,
    removed: &[LaneID],
    budget: &mut PrepareBudget,
) -> Result<(), Rejection> {
    let transit = sim.read::<crate::map_dynamic::Transit>();
    for stop in transit.stops.values() {
        budget.spend_road_work(road.interfaced_points().len())?;
        if road
            .interfaced_points()
            .project(stop.berth)
            .is_close(stop.berth, road.width * 0.5 + 0.25)
        {
            return Err(Rejection::BusyConflict);
        }
    }
    let map = sim.map();
    for line in transit.lines.values().filter(|line| !line.suspended) {
        budget.spend_road_work(line.stops.len())?;
        for (&from, &to) in line.stops.iter().zip(line.stops.iter().cycle().skip(1)) {
            let Some((from, to)) = transit.stored_stop_lane(from).zip(transit.stored_stop_lane(to)) else {
                continue;
            };
            if !service_path_survives(&map, from, to, removed, budget)?
                && service_path_survives(&map, from, to, &[], budget)?
            {
                return Err(Rejection::BusyConflict);
            }
        }
    }
    let vehicles = &sim.world().vehicles;
    budget.spend_road_work(vehicles.len())?;
    let affected = |kind| match kind {
        TraverseKind::Lane(id) => removed.contains(&id),
        TraverseKind::Turn(turn) => turn.parent == road.src || turn.parent == road.dst,
    };
    for (_, vehicle) in vehicles.iter() {
        if matches!(
            vehicle.it.target(),
            Some(crate::map_dynamic::ItineraryTarget::DirectedLane(lane, _)) if removed.contains(&lane)
        ) {
            return Err(Rejection::BusyConflict);
        }
        if vehicle.collider.is_some() {
            let rear = vehicle.vehicle.kind.width() * 0.5;
            let position = vehicle.trans.pos;
            if let Some(path) = vehicle.it.get_travers() {
                if let TraverseKind::Lane(id) = path.kind {
                    if let Some(lane) = map.get(id).filter(|lane| lane.kind.vehicles()) {
                        let (entrance, start) = match path.dir {
                            super::TraverseDirection::Forward => (lane.src, lane.points.first()),
                            super::TraverseDirection::Backward => (lane.dst, lane.points.last()),
                        };
                        if (entrance == road.src || entrance == road.dst)
                            && start.is_close(position, rear + 0.8)
                        {
                            return Err(Rejection::BusyConflict);
                        }
                    }
                }
            } else {
                let body = vehicle.trans.dir * rear;
                let envelope = geom::Segment3::new(position - body, position + body);
                for id in [road.src, road.dst] {
                    let junction = &map.intersections()[id];
                    if !junction
                        .pos
                        .is_close(position, junction.radius + rear + 1.0)
                    {
                        continue;
                    }
                    for turn in junction.raw_turns() {
                        budget.spend_road_work(1)?;
                        if turn.kind != super::TurnKind::Driving {
                            continue;
                        }
                        budget.spend_road_work(turn.points.len())?;
                        if turn.points.segments().any(|segment| {
                            crate::transportation::junction_admission::segments_close(
                                segment, envelope, 1.0,
                            )
                        }) {
                            return Err(Rejection::BusyConflict);
                        }
                    }
                }
            }
        }
        if let Some(route) = vehicle.it.get_route() {
            budget.spend_road_work(route.reversed_route.len())?;
            if affected(route.cur.kind)
                || route.reversed_route.iter().any(|path| affected(path.kind))
            {
                return Err(Rejection::BusyConflict);
            }
        } else if vehicle.collider.is_some() {
            budget.spend_road_work(road.interfaced_points().len())?;
            let (position, width) = (vehicle.trans.pos, vehicle.vehicle.kind.width());
            if road
                .interfaced_points()
                .project(position)
                .is_close(position, 0.5 * (road.width + width))
            {
                return Err(Rejection::BusyConflict);
            }
        }
    }
    Ok(())
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
        map.authorize_existing_road_triangle(road, triangle)?;
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

pub(super) fn stamp(
    map: &Map,
    road: &Road,
    edit: &[(LaneID, f32)],
) -> Result<RoadEditStamp, Rejection> {
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
    if pattern.is_walking_only() {
        return Err(Rejection::InvalidConnection);
    }
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
