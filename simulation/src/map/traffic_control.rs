use crate::map::{
    IntersectionID, LaneKind, LightPolicy, Map, ProjectFilter, ProjectKind, RoadID, TurnID,
    TurnKind,
};
use crate::transportation::junction_admission::segments_close;
use crate::transportation::VehicleKind;
use crate::world_command::{bounded_command_vec, PrepareBudget, Rejection};
use geom::{Segment3, Vec3, AABB};
use serde::{Deserialize, Serialize};

const MAX_APPROACH_RULES: usize = 32;
const MAX_FORBIDDEN_TURNS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ApproachRole {
    Priority,
    Yield,
    Stop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JunctionRuleError {
    Limit,
    MissingJunction,
    InvalidApproach,
    InvalidTurn,
    DuplicateApproach,
    DuplicateTurn,
    Signalized,
    RailConflict,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "JunctionRulesWire")]
pub struct JunctionRules {
    approaches: Vec<(RoadID, ApproachRole)>,
    forbidden: Vec<TurnID>,
}

#[derive(Deserialize)]
struct JunctionRulesWire {
    #[serde(deserialize_with = "bounded_command_vec::<_, _, MAX_APPROACH_RULES>")]
    approaches: Vec<(RoadID, ApproachRole)>,
    #[serde(deserialize_with = "bounded_command_vec::<_, _, MAX_FORBIDDEN_TURNS>")]
    forbidden: Vec<TurnID>,
}

impl TryFrom<JunctionRulesWire> for JunctionRules {
    type Error = &'static str;

    fn try_from(wire: JunctionRulesWire) -> Result<Self, Self::Error> {
        Self::new(wire.approaches, wire.forbidden).map_err(|_| "invalid junction rule list")
    }
}

impl JunctionRules {
    pub fn new(
        mut approaches: Vec<(RoadID, ApproachRole)>,
        mut forbidden: Vec<TurnID>,
    ) -> Result<Self, JunctionRuleError> {
        if approaches.len() > MAX_APPROACH_RULES || forbidden.len() > MAX_FORBIDDEN_TURNS {
            return Err(JunctionRuleError::Limit);
        }
        approaches.sort_unstable_by_key(|&(road, _)| road);
        forbidden.sort_unstable();
        if approaches.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err(JunctionRuleError::DuplicateApproach);
        }
        if forbidden.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(JunctionRuleError::DuplicateTurn);
        }
        Ok(Self {
            approaches,
            forbidden,
        })
    }

    pub fn approaches(&self) -> &[(RoadID, ApproachRole)] {
        &self.approaches
    }

    pub fn forbidden(&self) -> &[TurnID] {
        &self.forbidden
    }

    pub fn is_empty(&self) -> bool {
        self.approaches.is_empty() && self.forbidden.is_empty()
    }

    pub fn role(&self, road: RoadID) -> Option<ApproachRole> {
        let index = self
            .approaches
            .binary_search_by_key(&road, |&(id, _)| id)
            .ok()?;
        Some(self.approaches[index].1)
    }

    pub fn allows(&self, turn: TurnID) -> bool {
        self.forbidden.binary_search(&turn).is_err()
    }

    pub fn validate(&self, map: &Map, junction: IntersectionID) -> Result<(), JunctionRuleError> {
        let inter = map
            .get(junction)
            .ok_or(JunctionRuleError::MissingJunction)?;
        if inter.roads.len() > MAX_APPROACH_RULES {
            return Err(JunctionRuleError::Limit);
        }
        for &(road, _) in &self.approaches {
            if !inter.roads.contains(&road)
                || !map.get(road).is_some_and(|road| {
                    road.incoming_lanes_to(junction)
                        .iter()
                        .any(|&(_, kind)| kind.vehicles())
                })
            {
                return Err(JunctionRuleError::InvalidApproach);
            }
        }
        for &turn in &self.forbidden {
            if turn.parent != junction
                || turn.bidirectional
                || !inter
                    .raw_turn(turn)
                    .is_some_and(|turn| turn.kind == TurnKind::Driving)
            {
                return Err(JunctionRuleError::InvalidTurn);
            }
        }
        if !self.approaches.is_empty() {
            if self.effective_mode(map, junction) == Some(LightPolicy::Lights) {
                return Err(JunctionRuleError::Signalized);
            }
            if inter
                .roads
                .iter()
                .filter_map(|&id| map.get(id))
                .any(|road| road.lanes_iter().any(|(_, kind)| kind.is_rail()))
            {
                return Err(JunctionRuleError::RailConflict);
            }
        }
        Ok(())
    }

    pub(crate) fn effective_mode(
        &self,
        map: &Map,
        junction: IntersectionID,
    ) -> Option<LightPolicy> {
        let inter = map.get(junction)?;
        let groups = inter
            .roads
            .iter()
            .filter_map(|&id| map.get(id))
            .filter(|road| {
                road.incoming_lanes_to(junction)
                    .iter()
                    .any(|&(_, kind)| kind.needs_light())
            })
            .count();
        Some(
            inter
                .light_policy
                .resolve(groups, inter.turn_policy.left_turns),
        )
    }

    pub(crate) fn check_rail_sweeps(
        &self,
        map: &Map,
        junction: IntersectionID,
        budget: &mut PrepareBudget,
    ) -> Result<(), Rejection> {
        if self.approaches.is_empty() {
            return Ok(());
        }
        const MAX_RAIL_SWEEP_WORK: usize = 16_384;
        let allowance = (*budget.road_work_remaining()).min(MAX_RAIL_SWEEP_WORK);
        let mut local = PrepareBudget::for_preview(allowance);
        let result = Self::check_rail_sweeps_bounded(map, junction, &mut local);
        budget.spend_road_work(allowance - *local.road_work_remaining())?;
        result
    }

    fn check_rail_sweeps_bounded(
        map: &Map,
        junction: IntersectionID,
        budget: &mut PrepareBudget,
    ) -> Result<(), Rejection> {
        let inter = map.get(junction).ok_or(Rejection::MissingReference)?;
        let clearance = (VehicleKind::Bus.width() + LaneKind::Rail.width()) * 0.5 + 0.8;
        let mut bounds = AABB::new_ll_ur(inter.pos.xy(), inter.pos.xy());
        for turn in inter.raw_turns() {
            budget.spend_road_work(turn.points.len() + 1)?;
            for point in turn.points.iter() {
                bounds.ll = bounds.ll.min(point.xy());
                bounds.ur = bounds.ur.max(point.xy());
            }
        }
        let bounds = bounds.expand(clearance);
        let spatial = map.spatial_map();
        spatial
            .reserve_construction_query(bounds, budget.road_work_remaining())
            .map_err(|_| Rejection::RoadWorkLimit)?;

        // A same-deck track needs to participate in admission before it can
        // share the junction. A separate deck keeps its ordinary reservations.
        const VERTICAL_CLEARANCE: f32 = 3.0;
        let scaled_segment = |segment: Segment3| {
            let point = |p: Vec3| p.xy().z(p.z * clearance / VERTICAL_CLEARANCE);
            Segment3::new(point(segment.src), point(segment.dst))
        };
        for candidate in spatial.construction_candidates(bounds, ProjectFilter::ROAD) {
            let ProjectKind::Road(road) = candidate else {
                continue;
            };
            let road = map.get(road).ok_or(Rejection::MissingReference)?;
            budget.spend_road_work(road.n_lanes() + 1)?;
            for (id, kind) in road.lanes_iter() {
                if !kind.is_rail() {
                    continue;
                }
                let rail = map.get(id).ok_or(Rejection::MissingReference)?;
                for turn in inter.raw_turns().filter(|turn| turn.kind != TurnKind::Rail) {
                    budget.spend_road_work(1)?;
                    for a in turn.points.segments() {
                        let a = scaled_segment(a);
                        for b in rail.points.segments() {
                            budget.spend_road_work(1)?;
                            if segments_close(a, scaled_segment(b), clearance * clearance) {
                                return Err(Rejection::InvalidJunctionRules(
                                    JunctionRuleError::RailConflict,
                                ));
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Copy, Clone, Serialize, Deserialize)]
pub enum TrafficBehavior {
    RED,
    ORANGE,
    GREEN,
    STOP,
}

impl TrafficBehavior {
    pub fn is_red(self) -> bool {
        matches!(self, TrafficBehavior::RED)
    }
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
pub struct TrafficLightSchedule {
    period: u16,
    green: u16,
    orange: u16,
    red: u16,
    offset: u16,
}

impl TrafficLightSchedule {
    pub fn from_basic(green: u16, orange: u16, red: u16, offset: u16) -> Self {
        Self {
            period: green + orange + red,
            green,
            orange,
            red,
            offset,
        }
    }
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
pub enum TrafficControl {
    Always,
    Light(TrafficLightSchedule),
    StopSign,
}

impl TrafficControl {
    pub fn is_always(&self) -> bool {
        matches!(self, TrafficControl::Always)
    }

    pub fn is_stop_sign(&self) -> bool {
        matches!(self, TrafficControl::StopSign)
    }

    pub fn is_light(&self) -> bool {
        matches!(self, TrafficControl::Light(_))
    }

    pub fn get_behavior(&self, seconds: u32) -> TrafficBehavior {
        match self {
            TrafficControl::Always => TrafficBehavior::GREEN,
            TrafficControl::Light(schedule) => {
                let remainder =
                    ((seconds % schedule.period as u32) as u16 + schedule.offset) % schedule.period;
                if remainder < schedule.green {
                    TrafficBehavior::GREEN
                } else if remainder < schedule.green + schedule.orange {
                    TrafficBehavior::ORANGE
                } else {
                    TrafficBehavior::RED
                }
            }
            TrafficControl::StopSign => TrafficBehavior::STOP,
        }
    }
}
