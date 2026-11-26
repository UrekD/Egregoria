use crate::map::{
    Intersection, IntersectionID, LaneID, LaneKind, Lanes, Road, Roads, ROAD_Z_OFFSET,
};
use geom::{Intersect, PolyLine3, Polygon, Radians, Segment, Shape, Vec2};
use geom::{Spline, Vec3};
use serde::{Deserialize, Serialize};
use std::borrow::Borrow;
use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::f32::consts::TAU;

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash)]
pub struct TurnID {
    pub parent: IntersectionID,
    pub src: LaneID,
    pub dst: LaneID,
    pub bidirectional: bool,
}

impl TurnID {
    pub fn new(parent: IntersectionID, src: LaneID, dst: LaneID, bidirectional: bool) -> Self {
        Self {
            parent,
            src,
            dst,
            bidirectional,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum TurnKind {
    Crosswalk,
    WalkingCorner,
    Driving,
    Rail,
}

impl TurnKind {
    pub fn is_crosswalk(self) -> bool {
        matches!(self, TurnKind::Crosswalk)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Turn {
    pub id: TurnID,
    pub points: PolyLine3,
    pub kind: TurnKind,
}

impl Borrow<TurnID> for Turn {
    fn borrow(&self) -> &TurnID {
        &self.id
    }
}

impl PartialOrd for Turn {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Turn {
    fn cmp(&self, other: &Self) -> Ordering {
        self.id.cmp(&other.id)
    }
}

impl PartialEq for Turn {
    fn eq(&self, other: &Self) -> bool {
        self.id.eq(&other.id)
    }
}

impl Eq for Turn {}

const TURN_ANG_ADD: f32 = 0.29;
const TURN_ANG_MUL: f32 = 0.36;
const TURN_MUL: f32 = 0.46;
const N_SPLINE: usize = 6;

impl Turn {
    pub fn new(id: TurnID, kind: TurnKind) -> Self {
        Self {
            id,
            points: PolyLine3::new(vec![Vec3::ZERO; N_SPLINE + 2]),
            kind,
        }
    }

    pub(crate) fn make_points(
        &mut self,
        lanes: &Lanes,
        roads: &Roads,
        parent: &Intersection,
        roundabout: Option<RoundaboutGeometry>,
    ) {
        let (Some(src_lane), Some(dst_lane)) = (lanes.get(self.id.src), lanes.get(self.id.dst))
        else {
            return;
        };
        let pos_src = src_lane.get_inter_node_pos(self.id.parent);
        let pos_dst = dst_lane.get_inter_node_pos(self.id.parent);
        self.points.clear_push(pos_src);

        if self.kind.is_crosswalk() {
            self.points.push(pos_dst);
            return;
        }

        let src_dir = -src_lane.orientation_from(self.id.parent);
        let dst_dir = dst_lane.orientation_from(self.id.parent);

        if let Some(ring) = roundabout {
            let src_road = &roads[src_lane.parent];
            let dst_road = &roads[dst_lane.parent];
            let pose = (pos_src, pos_dst, src_dir, dst_dir);
            let points = match self.kind {
                TurnKind::Driving => Some(ring.driving(
                    pose,
                    parent.roundabout_port(roads, src_road.id, true),
                    parent.roundabout_port(roads, dst_road.id, false),
                )),
                TurnKind::WalkingCorner => Some(ring.corner(
                    pose,
                    src_road.dir_from(parent.id),
                    dst_road.dir_from(parent.id),
                    ring.curb_radius() + LaneKind::Walking.width() * 0.5,
                )),
                _ => None,
            };
            if let Some(points) = points {
                self.points.extend(points.skip(1));
                return;
            }
        }

        let spline = Self::spline(pos_src.xy(), pos_dst.xy(), src_dir, dst_dir);
        self.points.extend(
            spline
                .smart_points(0.3, 0.0, 1.0)
                .skip(1)
                .map(|x| x.z(pos_src.z)),
        );
    }

    /// Return points of a nice spline from `from` to `to` with derivatives `from_dir` and `to_dir`
    pub fn spline(from: Vec2, to: Vec2, from_dir: Vec2, to_dir: Vec2) -> Spline {
        let ang = from_dir.angle(to_dir);

        let dist = (to - from).mag() * (TURN_ANG_ADD + ang.abs() * TURN_ANG_MUL) * TURN_MUL;

        let derivative_src = from_dir * dist;
        let derivative_dst = to_dir * dist;

        Spline {
            from,
            to,
            from_derivative: derivative_src,
            to_derivative: derivative_dst,
        }
    }
}

/// Geometry shared by lane routing, the mesh and placement footprints.
#[derive(Clone, Copy)]
pub struct RoundaboutGeometry {
    pub center: Vec3,
    pub radius: f32,
    transition_scale: f32,
}

impl RoundaboutGeometry {
    pub(in crate::map) fn new(intersection: &Intersection, roads: &Roads) -> Option<Self> {
        let policy = intersection
            .turn_policy
            .roundabout
            .filter(|_| intersection.is_roundabout())?;
        if intersection.roads.len() > 16 || intersection.has_rail_arm(roads) {
            return None;
        }
        let center = intersection.pos.up(ROAD_Z_OFFSET);
        // Short roads may clip the requested setback. Keep the ring inside
        // their mouths so that entries always approach it from outside.
        let available = intersection
            .roads
            .iter()
            .map(|&id| {
                center
                    .xy()
                    .distance(roads[id].interface_point(intersection.id).xy())
                    - 6.0
            })
            .fold(f32::INFINITY, f32::min);
        let radius = policy.radius.min(available);
        (radius >= 4.0).then_some(Self {
            center,
            radius,
            transition_scale: 1.0,
        })
    }

    pub(super) fn tighter_transitions(self) -> Self {
        Self {
            transition_scale: 1.0 / 3.0,
            ..self
        }
    }

    pub(super) fn fits(
        self,
        junction: &Intersection,
        roads: &Roads,
        turns: &BTreeSet<Turn>,
    ) -> bool {
        let outline = |sidewalk| {
            let mut polygon = Polygon::default();
            for (&from, &to) in junction
                .roads
                .iter()
                .zip(junction.roads.iter().cycle().skip(1))
            {
                polygon.extend(
                    self.road_corner(junction.id, &roads[from], &roads[to], sidewalk)?
                        .map(Vec3::xy),
                );
            }
            Some(polygon)
        };
        let Some(asphalt) = outline(false) else {
            return false;
        };
        let Some(outer) = outline(true) else {
            return false;
        };
        let center = self.center.xy();
        let asphalt_edges = asphalt.segments().collect::<Vec<_>>();
        if !asphalt.contains(center)
            || asphalt_edges
                .iter()
                .any(|edge| edge.project(center).distance(center) < self.radius)
            || !simple_outline(&asphalt_edges)
            || !simple_outline(&outer.segments().collect::<Vec<_>>())
        {
            return false;
        }

        // Each lane has one fixed transition to or from the common arc.
        // Check the full straight segments consumed by movement, including
        // any excursions between tessellation vertices.
        let mut entries = BTreeSet::new();
        let mut exits = BTreeSet::new();
        for turn in turns.iter().filter(|turn| turn.kind == TurnKind::Driving) {
            let points = turn.points.as_slice();
            for (unique, transition, mouth) in [
                (
                    entries.insert(turn.id.src),
                    &points[..=TRANSITION_STEPS],
                    points[0],
                ),
                (
                    exits.insert(turn.id.dst),
                    &points[points.len() - TRANSITION_STEPS - 1..],
                    *points.last().unwrap(),
                ),
            ] {
                if !unique {
                    continue;
                }
                for points in transition.windows(2) {
                    let sweep = Segment::new(points[0].xy(), points[1].xy());
                    if sweep.project(center).distance(center) <= self.island_radius()
                        || !asphalt.contains(sweep.center())
                        || asphalt_edges.iter().any(|edge| {
                            sweep.bbox().intersects(&edge.bbox())
                                && sweep
                                    .intersection_point(edge)
                                    .is_some_and(|p| !p.is_close(mouth.xy(), 0.001))
                        })
                    {
                        return false;
                    }
                }
            }
        }
        true
    }

    pub fn curb_radius(self) -> f32 {
        self.radius + 4.0
    }

    pub fn island_radius(self) -> f32 {
        (self.radius - 3.0).max(self.radius * 0.4)
    }

    /// The boundary from one approach to the next in counter-clockwise order.
    pub fn road_corner(
        self,
        junction: IntersectionID,
        from: &Road,
        to: &Road,
        include_sidewalk: bool,
    ) -> Option<impl ExactSizeIterator<Item = Vec3>> {
        let mouth = |road: &Road, left: bool| {
            let points = road.interfaced_points();
            let (point, dir) = if road.src == junction {
                (points.first(), points.first_dir()?.xy().try_normalize()?)
            } else {
                (points.last(), -points.last_dir()?.xy().try_normalize()?)
            };
            let walks = road.sidewalks(junction);
            let sidewalk = if left { walks.incoming } else { walks.outgoing }.is_some();
            let width = road.width * 0.5
                - if sidewalk && !include_sidewalk {
                    LaneKind::Walking.width()
                } else {
                    0.0
                };
            let sign = if left { -1.0 } else { 1.0 };
            Some((
                point + (dir.perpendicular() * (width * sign)).z0(),
                dir,
                sidewalk,
            ))
        };
        let (a, from_dir, a_walk) = mouth(from, true)?;
        let (b, to_dir, b_walk) = mouth(to, false)?;
        let sidewalk = if include_sidewalk && a_walk && b_walk {
            LaneKind::Walking.width()
        } else {
            0.0
        };
        Some(self.corner(
            (a, b, -from_dir, to_dir),
            from.dir_from(junction),
            to.dir_from(junction),
            self.curb_radius() + sidewalk,
        ))
    }

    fn corner(
        self,
        pose: (Vec3, Vec3, Vec2, Vec2),
        from_arm: Vec2,
        to_arm: Vec2,
        radius: f32,
    ) -> std::vec::IntoIter<Vec3> {
        let from = from_arm.angle_cossin().0;
        let sweep = from_arm.angle(to_arm).rem_euclid(TAU);
        let trim = (sweep * 0.28).min(0.62);
        self.connect(pose, from + trim, sweep - 2.0 * trim, radius)
    }

    fn driving(
        self,
        pose: (Vec3, Vec3, Vec2, Vec2),
        entry: f32,
        exit: f32,
    ) -> std::vec::IntoIter<Vec3> {
        self.connect(pose, entry, (exit - entry).rem_euclid(TAU), self.radius)
    }

    fn connect(
        self,
        (from, to, into, out): (Vec3, Vec3, Vec2, Vec2),
        start: f32,
        sweep: f32,
        radius: f32,
    ) -> std::vec::IntoIter<Vec3> {
        let a = Radians(start).vec2();
        let b = Radians(start + sweep).vec2();
        let center = self.center;
        let mut enter = Turn::spline(
            from.xy(),
            center.xy() + a * radius,
            into,
            -a.perpendicular(),
        );
        let mut leave = Turn::spline(center.xy() + b * radius, to.xy(), -b.perpendicular(), out);
        for transition in [&mut enter, &mut leave] {
            transition.from_derivative *= self.transition_scale;
            transition.to_derivative *= self.transition_scale;
        }
        let steps = (sweep / 0.065).ceil().max(1.0) as usize;
        let mut points = Vec::with_capacity(steps + 32);
        // Fixed tessellation also preserves the tangent on both sides of each arc join.
        for i in 0..TRANSITION_STEPS {
            let t = i as f32 / TRANSITION_STEPS as f32;
            points.push(enter.get(t).z(from.z + (center.z - from.z) * t));
        }
        for i in 0..=steps {
            let angle = start + sweep * i as f32 / steps as f32;
            points.push(center + (Radians(angle).vec2() * radius).z0());
        }
        for i in 1..TRANSITION_STEPS {
            let t = i as f32 / TRANSITION_STEPS as f32;
            points.push(leave.get(t).z(center.z + (to.z - center.z) * t));
        }
        points.push(to);
        points.into_iter()
    }
}

const TRANSITION_STEPS: usize = 16;

fn simple_outline(edges: &[Segment]) -> bool {
    if edges.len() < 3 {
        return false;
    }
    edges.iter().enumerate().all(|(index, edge)| {
        edges
            .iter()
            .enumerate()
            .skip(index + 2)
            .all(|(other_index, other)| {
                (index == 0 && other_index == edges.len() - 1)
                    || !edge.bbox().intersects(&other.bbox())
                    || !edge.intersects(other)
            })
    })
}
