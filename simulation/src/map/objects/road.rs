use serde::{Deserialize, Serialize};
use slotmapd::new_key_type;

use geom::PolyLine;
use geom::{BoldLine, Degrees, PolyLine3, Spline, Spline1};
use geom::{Vec2, Vec3};

use crate::map::{
    BuildingID, Environment, Intersection, IntersectionID, Lane, LaneDirection, LaneID, LaneKind,
    LanePattern, Lanes, Map, MapProject, ParkingSpots, Roads, SpatialMap, MAX_SLOPE, ROAD_Z_OFFSET,
};

#[path = "../water_crossing.rs"]
mod water_crossing;
pub use water_crossing::*;

const MIN_ELEVATED_SPAN_CLEARANCE: f32 = 2.5;
const MIN_DECK_END_CLEARANCE: f32 = 0.5;

new_key_type! {
    pub struct RoadID;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum RoadSegmentKind {
    Straight,
    Curved((Vec2, Vec2)), // The two derivatives for the spline
    Arbitrary(PolyLine3),
}

impl RoadSegmentKind {
    pub fn from_elbow(from: Vec2, to: Vec2, elbow: Vec2) -> RoadSegmentKind {
        RoadSegmentKind::Curved((
            (elbow - from) * std::f32::consts::FRAC_1_SQRT_2,
            (to - elbow) * std::f32::consts::FRAC_1_SQRT_2,
        ))
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Road {
    pub id: RoadID,
    pub src: IntersectionID,
    pub dst: IntersectionID,

    /// always from src to dst
    /// don't try to make points go away from the road as it would be impossible to split them correctly afterward
    pub points: PolyLine3,
    pub interfaced_points: PolyLine3,
    pub width: f32,

    pub connected_buildings: Vec<BuildingID>,

    src_interface: TaggedInterface,
    dst_interface: TaggedInterface,

    lanes_forward: Vec<(LaneID, LaneKind)>,
    lanes_backward: Vec<(LaneID, LaneKind)>,
}

/// The sign bits of the source and destination interface distances record
/// elevated spans and the local one-way preset, respectively. The positional
/// Bincode layout is unchanged and older destination distances are untagged.
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(transparent)]
struct TaggedInterface(f32);

impl TaggedInterface {
    fn new(distance: f32, tagged: bool) -> Self {
        Self(if tagged { -distance } else { distance })
    }

    fn is_tagged(self) -> bool {
        self.0.is_sign_negative()
    }

    fn distance(self) -> f32 {
        self.0.abs()
    }

    fn set_distance(&mut self, distance: f32) {
        self.0 = distance.copysign(self.0);
    }
}

#[derive(Copy, Clone)]
pub struct LanePair {
    pub incoming: Option<LaneID>,
    pub outgoing: Option<LaneID>,
}

pub struct PylonPosition {
    pub terrain_height: f32,
    pub pos: Vec3,
    pub dir: Vec3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointGenerateError {
    OutsideOfMap,
    TooSteep,
}

pub(crate) fn profile_height(env: &Environment, xy: Vec2) -> Option<f32> {
    env.height(xy).or_else(|| {
        let bounds = env.bounds();
        if !env.has_water_configuration() || !bounds.contains(xy) {
            return None;
        }
        let interior = Vec2::new(bounds.ur.x.next_down(), bounds.ur.y.next_down());
        Some(env.base_height(xy.min(interior))?.max(0.0))
    })
}

impl MapProject {
    /// Position used for road placement, including any terrain snap at a split.
    /// The road surface offset is excluded.
    pub fn road_connection_position(self, map: &Map) -> Option<Vec3> {
        map.resolve_connection_endpoint(self)
            .map(|endpoint| endpoint.pos)
    }
}

impl Road {
    /// Builds the road and its associated lanes
    /// Must not fail or it make keeping invariants very complicated be cause of road splitting
    pub fn make(
        src: &Intersection,
        dst: &Intersection,
        segment: RoadSegmentKind,
        lane_pattern: &LanePattern,
        env: &Environment,
        roads: &mut Roads,
        lanes: &mut Lanes,
        parking: &mut ParkingSpots,
        spatial: &mut SpatialMap,
    ) -> RoadID {
        Self::make_with_span(
            src,
            dst,
            segment,
            lane_pattern,
            env,
            roads,
            lanes,
            parking,
            spatial,
            None,
        )
    }

    pub(crate) fn make_with_span(
        src: &Intersection,
        dst: &Intersection,
        segment: RoadSegmentKind,
        lane_pattern: &LanePattern,
        env: &Environment,
        roads: &mut Roads,
        lanes: &mut Lanes,
        parking: &mut ParkingSpots,
        spatial: &mut SpatialMap,
        inherited_span: Option<bool>,
    ) -> RoadID {
        let width = lane_pattern.width();
        let elevated_span =
            inherited_span.unwrap_or_else(|| Self::is_elevated_span(src.pos, dst.pos, env));
        let (points, error) = Self::generate_points_with_span(
            src.pos,
            dst.pos,
            segment,
            lane_pattern.lanes().any(|(a, _, _)| a.is_rail()),
            env,
            elevated_span,
        );
        let elevated_span =
            elevated_span && !matches!(error, Some(PointGenerateError::OutsideOfMap));

        let id = Self::insert_profile(roads, src.id, dst.id, points, width, elevated_span);
        let road = &mut roads[id];
        road.make_lanes(lane_pattern, lanes);
        road.update_lanes(lanes, parking, env);

        spatial.insert(road);
        road.id
    }

    pub(crate) fn insert_profile(
        roads: &mut Roads,
        src: IntersectionID,
        dst: IntersectionID,
        points: PolyLine3,
        width: f32,
        elevated_span: bool,
    ) -> RoadID {
        roads.insert_with_key(|id| Self {
            id,
            src,
            dst,
            src_interface: TaggedInterface::new(9.0, elevated_span),
            dst_interface: TaggedInterface::new(9.0, false),
            width,
            lanes_forward: vec![],
            lanes_backward: vec![],
            interfaced_points: PolyLine3::new(vec![points.first()]),
            points,
            connected_buildings: vec![],
        })
    }

    pub(crate) fn copy_geometry(
        &self,
        roads: &mut Roads,
        src: IntersectionID,
        dst: IntersectionID,
    ) -> RoadID {
        let id = Self::insert_profile(
            roads,
            src,
            dst,
            self.points.clone(),
            self.width,
            self.elevated_span(),
        );
        let road = &mut roads[id];
        road.src_interface = self.src_interface;
        road.dst_interface = self.dst_interface;
        road.interfaced_points = self.interfaced_points.clone();
        id
    }

    pub(crate) fn make_lanes(&mut self, pattern: &LanePattern, lanes: &mut Lanes) {
        self.dst_interface = TaggedInterface::new(
            self.dst_interface.distance(),
            pattern.local_one_way && pattern.has_local_one_way_layout(),
        );
        let mut dist_from_bottom = 0.0;
        for (kind, dir, limit) in pattern.lanes() {
            let id = Lane::make(self, lanes, kind, limit, dir, dist_from_bottom);
            match dir {
                LaneDirection::Forward => self.lanes_forward.insert(0, (id, kind)),
                LaneDirection::Backward => self.lanes_backward.push((id, kind)),
            }
            dist_from_bottom += kind.width();
        }
    }

    pub(crate) fn mark_staged_path(&mut self) {
        self.lanes_forward
            .push((LaneID::default(), LaneKind::Walking));
    }

    pub(crate) fn clear_staged_lanes(&mut self) {
        self.lanes_forward.clear();
        self.lanes_backward.clear();
    }

    pub(crate) fn install_lane_keys(
        &mut self,
        slots: impl IntoIterator<Item = (LaneID, LaneKind, LaneDirection)>,
    ) {
        self.clear_staged_lanes();
        for (id, kind, direction) in slots {
            match direction {
                LaneDirection::Forward => self.lanes_forward.insert(0, (id, kind)),
                LaneDirection::Backward => self.lanes_backward.push((id, kind)),
            }
        }
    }

    pub(crate) fn merged_profile(&self, next: &Road, joint: IntersectionID) -> PolyLine3 {
        let mut points = self.points.clone();
        if self.src == joint {
            points.reverse();
        }
        if next.src == joint {
            points.extend(next.points.iter().skip(1));
        } else {
            points.extend(next.points.iter().rev().skip(1));
        }
        points
    }

    pub(crate) fn split_profiles(&self, pos: Vec3, segment: usize) -> [PolyLine3; 2] {
        let mut before: Vec<_> = self.points.iter().copied().take(segment).collect();
        while before.last().is_some_and(|point| point.xy() == pos.xy()) {
            before.pop();
        }
        before.push(pos);
        let after_split = self
            .points
            .iter()
            .copied()
            .skip(segment)
            .skip_while(|point| point.xy() == pos.xy());
        let mut after: Vec<_> = [pos].into_iter().chain(after_split).collect();
        before.dedup();
        after.dedup();
        [PolyLine3::new(before), PolyLine3::new(after)]
    }

    pub fn is_walking_path(&self) -> bool {
        matches!(self.lanes_forward.as_slice(), [(_, LaneKind::Walking)])
            && self.lanes_backward.is_empty()
    }

    pub fn allows_motor_access(&self) -> bool {
        self.lanes_iter().any(|(_, kind)| kind.vehicles())
    }

    pub fn is_one_way(&self) -> bool {
        let traffic = |lanes: &[(LaneID, LaneKind)]| {
            lanes.iter().any(|(_, kind)| kind.vehicles() || kind.is_rail())
        };
        traffic(&self.lanes_forward) != traffic(&self.lanes_backward)
    }

    pub(crate) fn sidewalk_on(&self, side: crate::map::RoadSide) -> Option<LaneID> {
        if self.is_walking_path() {
            return None;
        }
        let lanes = match side {
            crate::map::RoadSide::Left => &self.lanes_backward,
            crate::map::RoadSide::Right => &self.lanes_forward,
        };
        let &(id, kind) = lanes.last()?;
        (kind == LaneKind::Walking).then_some(id)
    }

    pub fn n_lanes(&self) -> usize {
        self.lanes_backward.len() + self.lanes_forward.len()
    }

    pub(crate) fn directed_lanes(&self) -> [&[(LaneID, LaneKind)]; 2] {
        [&self.lanes_forward, &self.lanes_backward]
    }

    /// Returns lanes in left to right order from the source
    pub fn lanes_iter(&self) -> impl DoubleEndedIterator<Item = (LaneID, LaneKind)> + Clone + '_ {
        self.lanes_forward
            .iter()
            .rev()
            .chain(self.lanes_backward.iter())
            .copied()
    }

    pub fn sidewalks(&self, from: IntersectionID) -> LanePair {
        self.mk_pair(from, |lanes| {
            lanes
                .iter()
                .find(|(_, kind)| matches!(kind, LaneKind::Walking))
                .map(|&(id, _)| id)
        })
    }

    pub fn has_sidewalks(&self) -> bool {
        if self.is_walking_path() {
            return false;
        }
        self.lanes_forward
            .iter()
            .any(|(_, kind)| matches!(kind, LaneKind::Walking))
            || self
                .lanes_backward
                .iter()
                .any(|(_, kind)| matches!(kind, LaneKind::Walking))
    }

    pub fn parking_next_to(&self, lane: &Lane) -> Option<LaneID> {
        let lanes = if lane.src == self.src {
            &self.lanes_forward
        } else {
            &self.lanes_backward
        };

        lanes
            .iter()
            .find(|(_, kind)| matches!(kind, LaneKind::Parking))
            .map(|&(id, _)| id)
    }

    fn mk_pair(
        &self,
        from: IntersectionID,
        find: fn(&[(LaneID, LaneKind)]) -> Option<LaneID>,
    ) -> LanePair {
        let fw = find(&self.lanes_forward);
        let bw = find(&self.lanes_backward);

        if from == self.src {
            LanePair {
                incoming: bw,
                outgoing: fw,
            }
        } else {
            LanePair {
                incoming: fw,
                outgoing: bw,
            }
        }
    }

    pub fn update_lanes(
        &mut self,
        lanes: &mut Lanes,
        parking: &mut ParkingSpots,
        env: &Environment,
    ) {
        self.update_interfaced_points(env);
        for (id, _) in self.lanes_iter() {
            let l = unwrap_contlog!(lanes.get_mut(id), "lane in road does not exist anymore");
            l.gen_pos(self);
            if matches!(l.kind, LaneKind::Parking) {
                parking.generate_spots(l);
            }
        }
        parking.clean_reuse();
    }

    pub fn length(&self) -> f32 {
        self.points.length()
    }

    pub fn boldline(&self) -> BoldLine {
        BoldLine::new(self.points.flatten(), self.width * 0.5)
    }

    pub(in crate::map) fn is_local_one_way(&self) -> bool {
        self.dst_interface.is_tagged()
    }

    pub fn pattern(&self, lanes: &Lanes) -> LanePattern {
        LanePattern {
            local_one_way: self.is_local_one_way(),
            lanes_forward: self
                .lanes_forward
                .iter()
                .flat_map(|&(id, kind)| {
                    Some((
                        kind,
                        unwrap_or!(lanes.get(id), {
                            log::error!("lane doesn't exist while gettign pattern");
                            return None;
                        })
                        .speed_limit,
                    ))
                })
                .collect(),
            lanes_backward: self
                .lanes_backward
                .iter()
                .flat_map(|&(id, kind)| {
                    Some((
                        kind,
                        unwrap_or!(lanes.get(id), {
                            log::error!("lane doesn't exist while gettign pattern");
                            return None;
                        })
                        .speed_limit,
                    ))
                })
                .collect(),
        }
    }

    pub fn pylons_positions<'a>(
        interfaced_points: &'a PolyLine3,
        env: &'a Environment,
    ) -> impl Iterator<Item = PylonPosition> + 'a {
        interfaced_points
            .equipoints_dir(80.0, true)
            .filter_map(move |(pos, dir)| {
                let h = if env.is_water(pos.xy()) {
                    env.base_height(pos.xy())?
                } else {
                    env.true_height(pos.xy())?
                };
                if (h - pos.z).abs() <= 2.0 {
                    return None;
                }
                Some(PylonPosition {
                    terrain_height: h,
                    pos,
                    dir,
                })
            })
    }

    pub fn points(&self) -> &PolyLine3 {
        &self.points
    }
    pub fn interfaced_points(&self) -> &PolyLine3 {
        &self.interfaced_points
    }

    pub fn elevated_span(&self) -> bool {
        self.src_interface.is_tagged()
    }

    pub(crate) fn split_surface_height(&self, point: Vec3, env: &Environment) -> f32 {
        if self.elevated_span() {
            return point.z;
        }

        // Sample the terrain at the split in near-ground sections. Larger
        // differences can be part of the road's grade through cuts or fills.
        let road_height = point.z - ROAD_Z_OFFSET;
        env.height(point.xy())
            .filter(|&ground| (road_height - ground).abs() < MIN_ELEVATED_SPAN_CLEARANCE)
            .map_or(point.z, |ground| ground + ROAD_Z_OFFSET)
    }

    pub(crate) fn update_interfaced_points(&mut self, env: &Environment) {
        let points = &self.points;
        let clipped = points.cut(self.interface_from(self.src), self.interface_from(self.dst));

        if self.elevated_span() {
            // Cutting the original 3D deck interpolates its grade at both interfaces.
            self.interfaced_points = clipped;
            return;
        }

        // Keep the original terrain-following profile with the junction heights.
        let (p, _) = Self::heightfinder_with_span(
            &clipped.flatten(),
            points.first().z - ROAD_Z_OFFSET,
            points.last().z - ROAD_Z_OFFSET,
            MAX_SLOPE,
            env,
            false,
        );
        self.interfaced_points = p;
    }

    pub(crate) fn is_elevated_span(from: Vec3, to: Vec3, env: &Environment) -> bool {
        let (Some(from_ground), Some(to_ground)) =
            (profile_height(env, from.xy()), profile_height(env, to.xy()))
        else {
            return false;
        };
        // One end must clear the terrain by 2.5 m. The other may be a low
        // approach, with at least 0.5 m of clearance to exclude ground clicks.
        from.z >= from_ground + MIN_DECK_END_CLEARANCE
            && to.z >= to_ground + MIN_DECK_END_CLEARANCE
            && (from.z >= from_ground + MIN_ELEVATED_SPAN_CLEARANCE
                || to.z >= to_ground + MIN_ELEVATED_SPAN_CLEARANCE)
    }

    pub fn heightfinder(
        p: &PolyLine,
        start_height: f32,
        end_height: f32,
        maxslope: f32,
        env: &Environment,
    ) -> (PolyLine3, Option<PointGenerateError>) {
        let elevated_span =
            Self::is_elevated_span(p.first().z(start_height), p.last().z(end_height), env);
        Self::heightfinder_with_span(p, start_height, end_height, maxslope, env, elevated_span)
    }

    // Run an algorithm to find the height of the road at each point
    // This is not easy because the terrain can take many shapes
    // The algorithm is as follow:
    // - First compute the terrain contour every meter
    // - Then find out which points are airborn (according to maxslope)
    // - Then find the interface points where points become airborn
    // - Then linear interpolate the points between the interface points
    // - Then smooth out the result to avoid huge derivative changes
    // - Then simplify the result to avoid too many points
    //
    // maxslope is the maximum meter of height difference per meter of distance (1.0 is a 45° slope)
    fn heightfinder_with_span(
        p: &PolyLine,
        start_height: f32,
        end_height: f32,
        maxslope: f32,
        env: &Environment,
        elevated_span: bool,
    ) -> (PolyLine3, Option<PointGenerateError>) {
        // first calculate the contour

        let mut contour = Vec::with_capacity(p.length() as usize + 2);
        let mut points = Vec::with_capacity(contour.len());

        let mut height_error = false;
        for pos in std::iter::once(p.first())
            .chain(
                p.points_dirs_along((1..p.length() as u32).map(|v| v as f32))
                    .map(|v| v.0),
            )
            .chain(std::iter::once(p.last()))
        {
            let h = profile_height(env, pos).unwrap_or_else(|| {
                height_error = true;
                0.0
            });
            contour.push(h);
            points.push(pos.z(h));
        }

        let last = contour.len() - 1;
        let elevated_span = elevated_span && !height_error;
        let length = p.length();
        let rise = end_height - start_height;
        // Interior samples are spaced one meter apart along the path.
        let deck_height = |i: usize| start_height + rise * (i as f32 / length);
        let clear_span = elevated_span
            && contour[1..last]
                .iter()
                .enumerate()
                .all(|(i, &height)| height <= deck_height(i + 1));
        contour[0] = start_height;
        contour[last] = end_height;

        let mut slope_was_too_steep = !clear_span && Self::follow_terrain(&mut contour, maxslope);
        if elevated_span {
            for (i, height) in contour[..last].iter_mut().enumerate().skip(1) {
                let grade = deck_height(i);
                *height = if clear_span { grade } else { height.max(grade) };
            }
            slope_was_too_steep |= rise.abs() > maxslope * length;
        }

        for (h, v) in contour.into_iter().zip(points.iter_mut()) {
            v.z = h + ROAD_Z_OFFSET;
        }

        let mut points = PolyLine3::new(points);
        points.simplify(Degrees(1.0).into(), 1.0, 100.0);

        if height_error {
            return (points, Some(PointGenerateError::OutsideOfMap));
        }

        if slope_was_too_steep {
            return (points, Some(PointGenerateError::TooSteep));
        }

        (points, None)
    }

    fn follow_terrain(contour: &mut [f32], maxslope: f32) -> bool {
        // Then find out which points are airborn (according to maxslope)
        // To do that, we do two passes (one forward, one backward) to find the airborn points

        let mut airborn = Vec::with_capacity(contour.len());

        let mut cur_height = contour[0];
        for &h in contour.iter() {
            let diff = cur_height - h;

            airborn.push(diff > maxslope);
            cur_height -= diff.min(maxslope);
        }

        let mut cur_height = contour.last().copied().unwrap();
        {
            let mut i = airborn.len();
            for &h in contour.iter().rev() {
                i -= 1;
                let diff = cur_height - h;

                airborn[i] |= diff > maxslope;
                cur_height -= diff.min(maxslope);
            }
        }

        // Then find the interface points where points become airborn
        // To do that, we just find the points where airborn changes

        *airborn.first_mut().unwrap() = false;
        *airborn.last_mut().unwrap() = false;

        let mut interface = Vec::with_capacity(airborn.len());
        for i in 1..airborn.len() - 1 {
            if airborn[i] && !airborn[i - 1] {
                interface.push(i.saturating_sub(3));
            }
            if airborn[i] && !airborn[i + 1] {
                interface.push((i + 3).min(airborn.len() - 1));
            }
        }

        // Compact overlapping spans in place. Both endpoints are ordered, so
        // an overlap extends the last retained span to the current end.
        let mut merged = 0;
        for i in (0..interface.len()).step_by(2) {
            let start = interface[i];
            let end = interface[i + 1];
            if merged > 0 && interface[merged - 1] >= start {
                interface[merged - 1] = end;
            } else {
                interface[merged] = start;
                interface[merged + 1] = end;
                merged += 2;
            }
        }
        interface.truncate(merged);

        let mut slope_was_too_steep = false;

        // Then linear interpolate the points between the interface points that aren't on the ground using a nice spline
        for w in interface.chunks(2) {
            let i1 = w[0];
            let i2 = w[1];

            let h1 = contour[i1];
            let h2 = contour[i2];

            let derivative_1 = h1 - contour.get(i1.wrapping_sub(1)).unwrap_or(&h1);
            let derivative_2 = contour.get(i2 + 1).unwrap_or(&h2) - h2;

            let d = (h2 - h1).abs();

            let slope = d / (i2 - i1) as f32;

            if slope > maxslope {
                slope_was_too_steep = true;
            }

            let s = Spline1 {
                from: h1,
                to: h2,
                from_derivative: derivative_1 * d,
                to_derivative: derivative_2 * d,
            };

            for (j, h) in s.points(i2 - i1 + 1).enumerate() {
                contour[i1 + j] = h;
            }
        }

        // Then smooth out the result to avoid huge derivative changes
        //let mut smoothed = vec![0.0; contour.len()];
        //smoothed[0] = contour[0];
        //let l = smoothed.len();
        //smoothed[l - 1] = contour[contour.len() - 1];
        //// must be odd to have the result in contour
        //for _ in 0..3 {
        //    for i in 1..smoothed.len() - 1 {
        //        smoothed[i] = (contour[i - 1] + contour[i] + contour[i + 1]) / 3.0;
        //    }
        //    std::mem::swap(&mut smoothed, &mut contour);
        //}

        slope_was_too_steep
    }

    pub fn generate_points(
        from: Vec3,
        to: Vec3,
        segment: RoadSegmentKind,
        precise: bool,
        env: &Environment,
    ) -> (PolyLine3, Option<PointGenerateError>) {
        let elevated_span = Self::is_elevated_span(from, to, env);
        Self::generate_points_with_span(from, to, segment, precise, env, elevated_span)
    }

    pub(crate) fn generate_points_with_span(
        from: Vec3,
        to: Vec3,
        segment: RoadSegmentKind,
        precise: bool,
        env: &Environment,
        elevated_span: bool,
    ) -> (PolyLine3, Option<PointGenerateError>) {
        let spline = match segment {
            RoadSegmentKind::Straight => {
                let p = PolyLine::new(vec![from.xy(), to.xy()]);
                return Self::heightfinder_with_span(
                    &p,
                    from.z,
                    to.z,
                    MAX_SLOPE,
                    env,
                    elevated_span,
                );
            }
            RoadSegmentKind::Curved((from_derivative, to_derivative)) => Spline {
                from: from.xy(),
                to: to.xy(),
                from_derivative,
                to_derivative,
            },
            RoadSegmentKind::Arbitrary(pos) => {
                if pos.len() < 2 {
                    unreachable!("Arbitrary road segment with less than 2 points")
                }
                return (pos, None);
            }
        };

        let iter = spline.smart_points(if precise { 0.1 } else { 1.0 }, 0.0, 1.0);
        let mut p = PolyLine::new(vec![from.xy()]);

        for v in iter {
            if v.is_close(from.xy(), 1.0) {
                continue;
            }
            if v.is_close(to.xy(), 1.0) {
                continue;
            }
            p.push(v);
        }
        p.push(to.xy());

        Self::heightfinder_with_span(&p, from.z, to.z, MAX_SLOPE, env, elevated_span)
    }

    pub fn interface_point(&self, id: IntersectionID) -> Vec3 {
        if id == self.src {
            self.interfaced_points().first()
        } else if id == self.dst {
            self.interfaced_points().last()
        } else {
            panic!("Asking interface from an intersection not connected to the road");
        }
    }

    pub fn interface_from(&self, id: IntersectionID) -> f32 {
        let (my_interf, other_interf) = self.interfaces_from(id);

        let l = self.points.length() - 2.0;
        let half = l * 0.5;

        if my_interf + other_interf > l {
            if my_interf > half && other_interf > half {
                half
            } else if my_interf > half {
                l - other_interf
            } else {
                my_interf
            }
        } else {
            my_interf
        }
    }

    fn interfaces_from(&self, id: IntersectionID) -> (f32, f32) {
        if id == self.src {
            (self.src_interface.distance(), self.dst_interface.distance())
        } else if id == self.dst {
            (self.dst_interface.distance(), self.src_interface.distance())
        } else {
            panic!("Asking interface from from an intersection not connected to the road");
        }
    }

    pub fn set_interface(&mut self, id: IntersectionID, v: f32) {
        if id == self.src {
            self.src_interface.set_distance(v);
        } else if id == self.dst {
            self.dst_interface.set_distance(v);
        } else {
            panic!("Setting interface from from an intersection not connected to the road");
        }
    }

    pub fn max_interface(&mut self, id: IntersectionID, v: f32) {
        if id == self.src {
            self.src_interface
                .set_distance(self.src_interface.distance().max(v));
        } else if id == self.dst {
            self.dst_interface
                .set_distance(self.dst_interface.distance().max(v));
        } else {
            panic!("Setting interface from from an intersection not connected to the road");
        }
    }

    pub fn dir_from(&self, id: IntersectionID) -> Vec2 {
        if id == self.src {
            self.src_dir()
        } else if id == self.dst {
            self.dst_dir()
        } else {
            panic!("Asking dir from from an intersection not connected to the road");
        }
    }

    pub fn incoming_lanes_to(&self, id: IntersectionID) -> &Vec<(LaneID, LaneKind)> {
        if id == self.src {
            &self.lanes_backward
        } else if id == self.dst {
            &self.lanes_forward
        } else {
            panic!("Asking incoming lanes from from an intersection not connected to the road");
        }
    }

    pub fn outgoing_lanes_from(&self, id: IntersectionID) -> &Vec<(LaneID, LaneKind)> {
        if id == self.src {
            &self.lanes_forward
        } else if id == self.dst {
            &self.lanes_backward
        } else {
            panic!("Asking outgoing lanes from from an intersection not connected to the road");
        }
    }

    pub fn src_dir(&self) -> Vec2 {
        self.points.first_dir().unwrap_or(Vec3::X).xy().normalize()
    }

    pub fn dst_dir(&self) -> Vec2 {
        -self.points.last_dir().unwrap_or(Vec3::X).xy().normalize()
    }

    pub fn other_end(&self, my_end: IntersectionID) -> Option<IntersectionID> {
        if self.src == my_end {
            return Some(self.dst);
        }
        if self.dst == my_end {
            return Some(self.src);
        }
        None
    }
}
