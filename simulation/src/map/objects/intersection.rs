use crate::map::{
    Intersections, LaneID, LaneKind, Lanes, LightPolicy, Road, RoadID, Roads, RoundaboutGeometry,
    SpatialMap, TraverseDirection, Turn, TurnID, TurnPolicy,
};
use geom::{pseudo_angle, Circle, Ray};
use geom::{Vec2, Vec3};
use ordered_float::OrderedFloat;
use serde::{Deserialize, Serialize};
use slotmapd::new_key_type;
use std::collections::BTreeSet;

use crate::transportation::junction_admission::roundabout_exit_storage;

new_key_type! {
    pub struct IntersectionID;
}

impl IntersectionID {
    pub fn as_ffi(self) -> u64 {
        self.0.as_ffi()
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Intersection {
    pub id: IntersectionID,
    pub pos: Vec3,
    pub radius: f32,

    turns: BTreeSet<Turn>,

    #[serde(skip)]
    legal_turns: Option<BTreeSet<Turn>>,

    #[serde(skip)]
    roundabout_layout: Option<RoundaboutGeometry>,

    #[serde(skip)]
    crosswalks: Vec<TurnID>,

    // sorted by angle
    pub roads: Vec<RoadID>,

    pub turn_policy: TurnPolicy,
    pub light_policy: LightPolicy,
}

impl Intersection {
    pub fn make(store: &mut Intersections, spatial: &mut SpatialMap, pos: Vec3) -> IntersectionID {
        let id = store.insert_with_key(|id| Intersection {
            id,
            pos,
            radius: 0.0,
            turns: Default::default(),
            legal_turns: None,
            crosswalks: Vec::new(),
            roundabout_layout: None,
            roads: Default::default(),
            turn_policy: Default::default(),
            light_policy: Default::default(),
        });
        spatial.insert(&store[id]);
        id
    }

    pub fn add_road(&mut self, roads: &Roads, road: &Road) {
        self.roads.push(road.id);

        let id = self.id;
        self.roads.retain(|&id| roads.contains_key(id));
        self.roads.sort_by_key(|&road| {
            #[allow(clippy::indexing_slicing)]
            OrderedFloat(pseudo_angle(roads[road].dir_from(id)))
        });
    }

    pub fn bcircle(&self) -> Circle {
        Circle {
            center: self.pos.xy(),
            radius: self.radius,
        }
    }

    fn update_radius(&mut self, roads: &Roads) {
        self.radius = self
            .roads
            .iter()
            .flat_map(|x| roads.get(*x))
            .map(|x| {
                if self.is_roundabout() {
                    OrderedFloat(x.interface_from(self.id))
                } else {
                    OrderedFloat(x.width)
                }
            })
            .max()
            .map(|x| x.0)
            .unwrap_or(10.0);
    }

    pub fn remove_road(&mut self, road_id: RoadID) {
        self.roads.retain(|x| *x != road_id);
    }

    pub fn update_turns(&mut self, lanes: &Lanes, roads: &Roads) {
        self.rebuild_turns(lanes, roads, true);
    }

    pub(in crate::map) fn use_stock_turns(&mut self, lanes: &Lanes, roads: &Roads) {
        self.rebuild_turns(lanes, roads, false);
    }

    fn rebuild_turns(&mut self, lanes: &Lanes, roads: &Roads, allow_roundabout: bool) {
        self.legal_turns = None;
        self.roundabout_layout = None;
        let candidate = allow_roundabout
            .then(|| RoundaboutGeometry::new(self, roads))
            .flatten()
            .filter(|_| {
                self.roads.iter().all(|&id| {
                    roads[id]
                        .outgoing_lanes_from(self.id)
                        .iter()
                        .all(|&(lane, kind)| {
                            !kind.vehicles()
                                || lanes[lane].points.length() >= roundabout_exit_storage()
                        })
                })
            });
        let build_paths = |ring: Option<RoundaboutGeometry>| {
            let mut policy = self.turn_policy;
            if ring.is_none() {
                policy.roundabout = None;
            }
            policy
                .generate_turns(self, lanes, roads)
                .into_iter()
                .map(|(id, kind)| {
                    let mut turn = Turn::new(id, kind);
                    turn.make_points(lanes, roads, self, ring);
                    turn
                })
                .collect::<BTreeSet<_>>()
        };
        if let Some(ring) = candidate {
            for ring in [ring, ring.tighter_transitions()] {
                let paths = build_paths(Some(ring));
                if ring.fits(self, roads, &paths) {
                    self.roundabout_layout = Some(ring);
                    self.turns = paths;
                    self.index_crosswalks();
                    return;
                }
            }
        }
        self.turns = build_paths(None);
        self.index_crosswalks();
    }

    pub(crate) fn index_crosswalks(&mut self) {
        self.crosswalks = self.turns.iter()
            .filter(|turn| turn.kind == crate::map::TurnKind::Crosswalk)
            .map(|turn| turn.id)
            .collect();
    }

    pub(crate) fn crosswalks(&self) -> impl Iterator<Item = &Turn> {
        self.crosswalks.iter().filter_map(|&id| self.find_turn(id))
    }

    pub fn update_traffic_control(&self, lanes: &mut Lanes, roads: &Roads) {
        let light = if self.roundabout_layout.is_some() {
            LightPolicy::NoLights
        } else {
            self.light_policy
        };
        light.apply(self, lanes, roads);
    }

    const MIN_INTERFACE: f32 = 9.0;
    pub fn update_interface_radius(&mut self, roads: &mut Roads) {
        let arms = crate::map::turn_policy::non_path_arms(&self.roads, roads);
        let motor_junction = !arms.is_empty();
        let arms = if motor_junction { &*arms } else { &self.roads };
        self.update_arm_interfaces(roads, arms, motor_junction);
        for &id in &self.roads {
            if !motor_junction || !roads[id].is_walking_path() {
                continue;
            }
            let path = &roads[id];
            let direction = path.dir_from(self.id);
            let trim = arms
                .iter()
                .map(|&arm| {
                    let road = &roads[arm];
                    let lateral = road.dir_from(self.id).perpendicular().dot(direction).abs();
                    (road.width * 0.5 + 0.25) / lateral.max(0.35)
                })
                .fold(1.0, f32::max);
            roads[id].set_interface(self.id, trim);
        }
        if arms.len() != self.roads.len() || self.roads.len() > 2 || self.turn_policy.roundabout.is_some() {
            self.update_radius(roads);
        }
    }

    fn update_arm_interfaces(&self, roads: &mut Roads, arms: &[RoadID], motor_junction: bool) {
        let roundabout = self
            .turn_policy
            .roundabout
            .filter(|policy| motor_junction && policy.radius >= 4.0 && !self.has_rail_arm(roads));
        self.set_interfaces(roads, arms, false);
        let Some(roundabout) = roundabout.filter(|_| arms.len() >= 2) else {
            return;
        };
        // Leave space for the longest vehicle admitted by the junction. A road
        // that cannot accommodate a ring retains its ordinary junction setback.
        if arms.iter().any(|&id| {
            let road = &roads[id];
            road.length()
                - road.interface_from(road.other_end(self.id).unwrap())
                - roundabout_exit_storage()
                < 10.0
        }) {
            return;
        }
        self.set_interfaces(roads, arms, true);
        // Give neighboring mouth edges separate angular sectors. The ordinary
        // junction setback only clears the deck centerlines at shallow angles.
        for (&a, &b) in arms.iter().zip(arms.iter().cycle().skip(1)) {
            let a_road = &roads[a];
            let b_road = &roads[b];
            let gap = a_road
                .dir_from(self.id)
                .angle(b_road.dir_from(self.id))
                .abs();
            if gap > 0.0 && gap < std::f32::consts::FRAC_PI_4 {
                let setback = ((a_road.width + b_road.width) * 0.25 + 0.75) / (gap * 0.5).tan();
                roads[a].max_interface(self.id, setback);
                roads[b].max_interface(self.id, setback);
            }
        }
        for &id in arms {
            let road = &mut roads[id];
            let approach = road
                .interface_from(self.id)
                .max(roundabout.radius + 10.0 + road.width * 0.2);
            let other = road.interface_from(road.other_end(self.id).unwrap());
            road.set_interface(
                self.id,
                approach.min(road.length() - other - (roundabout_exit_storage() + 0.05)),
            );
        }
    }

    fn set_interfaces(&self, roads: &mut Roads, arms: &[RoadID], roundabout: bool) {
        let id = self.id;
        let position = self.pos;
        match *arms {
            [] => return,
            [r1_id] => {
                let r = &mut roads[r1_id];
                r.set_interface(id, Self::empty_interface(r.width));
                return;
            }
            [r1_id, r2_id] if !roundabout => {
                let (r1, r2) = (&roads[r1_id], &roads[r2_id]);
                let (dir1, dir2) = (r1.dir_from(id), r2.dir_from(id));
                let (r1w, r2w) = (r1.width, r2.width);
                let elbow = (dir1 + dir2) * 0.5;

                if elbow.mag() < 0.001 {
                    roads[r1_id].set_interface(id, 1.0);
                    roads[r2_id].set_interface(id, 1.0);
                    return;
                }

                let ray1 = Ray::new(
                    position.xy()
                        + dir1.perpendicular()
                            * dir1.perpendicular().dot(elbow).signum()
                            * r1w
                            * 0.5,
                    dir1,
                );
                let ray2 = Ray::new(
                    position.xy()
                        + dir2.perpendicular()
                            * dir2.perpendicular().dot(elbow).signum()
                            * r2w
                            * 0.5,
                    dir2,
                );

                let Some((dist_a, dist_b)) = ray1.both_dist_to_inter(&ray2) else {
                    roads[r1_id].set_interface(id, Self::empty_interface(r1w));
                    roads[r2_id].set_interface(id, Self::empty_interface(r2w));
                    return;
                };

                roads[r1_id].set_interface(id, dist_a);
                roads[r2_id].set_interface(id, dist_b);
                return;
            }
            _ => {}
        }

        for &r in arms {
            let r = &mut roads[r];
            r.set_interface(id, Self::empty_interface(r.width));
        }

        for i in 0..arms.len() {
            let r1_id = arms[i];
            let r2_id = arms[(i + 1) % arms.len()];

            let (r1, r2) = (&roads[r1_id], &roads[r2_id]);
            let (dir1, dir2) = (r1.dir_from(id), r2.dir_from(id));

            let min_dist = if dir1.angle(dir2).abs() < 0.17453292 {
                Self::interface_calc_numerically(id, position, r1.width, r2.width, r1, r2)
            } else {
                Self::interface_calc_formula(r1.width, r2.width, dir1, dir2)
            };

            roads[r1_id].max_interface(id, min_dist);
            roads[r2_id].max_interface(id, min_dist);
        }
    }

    fn interface_calc_formula(w1: f32, w2: f32, dir1: Vec2, dir2: Vec2) -> f32 {
        let hwidth1 = w1 * 0.5;
        let hwidth2 = w2 * 0.5;

        let w = hwidth1.hypot(hwidth2);

        let d = dir1.dot(dir2).clamp(0.0, 1.0);
        let sin = (1.0 - d * d).sqrt();

        (w * 1.1 / sin).min(50.0)
    }

    fn interface_calc_numerically(
        id: IntersectionID,
        position: Vec3,
        w1: f32,
        w2: f32,
        r1: &Road,
        r2: &Road,
    ) -> f32 {
        let w: f32 = (w1 + w2) * 0.80;

        let mut points1: Vec<(Vec3, Vec3)> = r1
            .points()
            .points_dirs_along((1..r1.points().length() as i32).map(|d| d as f32))
            .collect();
        let mut points2: Vec<(Vec3, Vec3)> = r2
            .points()
            .points_dirs_along((1..r2.points().length() as i32).map(|d| d as f32))
            .collect();

        if r1.src != id {
            points1.reverse();
        }
        if r2.src != id {
            points2.reverse();
        }

        points1
            .into_iter()
            .zip(points2)
            .map(|((p1, _), (p2, _))| (p1.xy(), p2.xy()))
            .find(|p| p.0.distance(p.1) > w)
            .map(|p| (position.xy().distance(p.0) + position.xy().distance(p.0)) * 0.5)
            .unwrap_or(50.0)
    }

    pub fn empty_interface(width: f32) -> f32 {
        (width * 0.8).max(Self::MIN_INTERFACE)
    }

    pub fn interface_at(&self, roads: &Roads, width: f32, dir: Vec2) -> f32 {
        let mut max_inter = Self::empty_interface(width);
        let id = self.id;
        for &r1_id in &self.roads {
            let r1 = unwrap_cont!(roads.get(r1_id));
            max_inter = max_inter.max(Self::interface_calc_formula(
                r1.width,
                width,
                r1.dir_from(id),
                dir,
            ));
        }
        max_inter
    }

    pub fn is_roundabout(&self) -> bool {
        self.turn_policy.roundabout.is_some() && self.roads.len() > 1
    }

    pub fn roundabout_geometry(&self) -> Option<RoundaboutGeometry> {
        self.roundabout_layout
    }

    pub(crate) fn has_rail_arm(&self, roads: &Roads) -> bool {
        self.roads
            .iter()
            .any(|&id| roads[id].lanes_iter().any(|(_, kind)| kind.is_rail()))
    }

    pub(super) fn roundabout_port(&self, roads: &Roads, arm: RoadID, entry: bool) -> f32 {
        let index = self.roads.iter().position(|&road| road == arm).unwrap();
        let count = self.roads.len();
        let dir = roads[arm].dir_from(self.id);
        let angle = dir.angle_cossin().0;
        let (neighbor, sign) = if entry {
            (self.roads[(index + 1) % count], 1.0)
        } else {
            (self.roads[(index + count - 1) % count], -1.0)
        };
        let gap =
            (sign * dir.angle(roads[neighbor].dir_from(self.id))).rem_euclid(std::f32::consts::TAU);
        angle + sign * (gap * 0.25).min(0.40)
    }

    pub fn undirected_neighbors<'a>(
        &'a self,
        roads: &'a Roads,
    ) -> impl Iterator<Item = IntersectionID> + 'a {
        self.roads
            .iter()
            .flat_map(move |&x| roads.get(x).and_then(|r| r.other_end(self.id)))
    }

    pub fn vehicle_neighbours<'a>(
        &'a self,
        roads: &'a Roads,
    ) -> impl Iterator<Item = IntersectionID> + 'a {
        let id = self.id;
        self.roads.iter().flat_map(move |&x| {
            let r = roads.get(x)?;
            r.outgoing_lanes_from(id).iter().find(|(_, kind)| {
                matches!(kind, LaneKind::Driving | LaneKind::Rail | LaneKind::Bus)
            })?;
            r.other_end(id)
        })
    }

    pub fn find_turn(&self, needle: TurnID) -> Option<&Turn> {
        self.legal_turns
            .as_ref()
            .unwrap_or(&self.turns)
            .get(&needle)
    }

    pub(crate) fn raw_turn(&self, needle: TurnID) -> Option<&Turn> {
        self.turns.get(&needle)
    }

    pub(crate) fn raw_turns(&self) -> impl ExactSizeIterator<Item = &Turn> {
        self.turns.iter()
    }

    pub(crate) fn apply_turn_mask(&mut self, rules: Option<&crate::map::JunctionRules>) {
        self.legal_turns = rules
            .filter(|rules| !rules.forbidden().is_empty())
            .map(|rules| {
                self.turns
                    .iter()
                    .filter(|turn| rules.allows(turn.id))
                    .cloned()
                    .collect()
            });
    }

    pub fn turns_from(
        &self,
        lane: LaneID,
    ) -> impl Iterator<Item = (TurnID, TraverseDirection)> + '_ {
        self.turns().filter_map(move |Turn { id, .. }| {
            if id.src == lane {
                Some((*id, TraverseDirection::Forward))
            } else if id.bidirectional && id.dst == lane {
                Some((*id, TraverseDirection::Backward))
            } else {
                None
            }
        })
    }

    pub fn turns_to(&self, lane: LaneID) -> impl Iterator<Item = (TurnID, TraverseDirection)> + '_ {
        self.turns().filter_map(move |Turn { id, .. }| {
            if id.dst == lane {
                Some((*id, TraverseDirection::Forward))
            } else if id.bidirectional && id.src == lane {
                Some((*id, TraverseDirection::Backward))
            } else {
                None
            }
        })
    }

    pub(crate) fn next_turn(&self, after: Option<TurnID>) -> Option<&Turn> {
        use std::ops::Bound::{Excluded, Unbounded};
        let turns = self.legal_turns.as_ref().unwrap_or(&self.turns);
        turns
            .range::<TurnID, _>((after.map_or(Unbounded, Excluded), Unbounded))
            .next()
    }

    pub fn turns(&self) -> impl ExactSizeIterator<Item = &Turn> {
        self.legal_turns.as_ref().unwrap_or(&self.turns).iter()
    }
}

debug_inspect_impl!(IntersectionID);
