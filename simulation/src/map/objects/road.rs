use serde::{Deserialize, Serialize};
use slotmapd::new_key_type;

use geom::PolyLine;
use geom::{BoldLine, Degrees, PolyLine3, Spline, Spline1};
use geom::{Vec2, Vec3};

use crate::map::{
    BuildingID, Environment, Intersection, IntersectionID, Lane, LaneDirection, LaneID, LaneKind,
    LanePattern, Lanes, ParkingSpots, Roads, SpatialMap, MAX_SLOPE, ROAD_Z_OFFSET,
};

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

    src_interface: SourceInterface,
    dst_interface: f32,

    lanes_forward: Vec<(LaneID, LaneKind)>,
    lanes_backward: Vec<(LaneID, LaneKind)>,
}

/// Stores the elevated-span flag in the sign bit of the source interface distance.
/// This keeps the positional Bincode layout of a road. Older saves stored
/// nonnegative distances, which decode as terrain-following.
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(transparent)]
struct SourceInterface(f32);

impl SourceInterface {
    fn new(distance: f32, elevated_span: bool) -> Self {
        Self(if elevated_span { -distance } else { distance })
    }

    fn elevated_span(self) -> bool {
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

pub enum PointGenerateError {
    OutsideOfMap,
    TooSteep,
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
        let width = lane_pattern.width();
        let elevated_span = Self::is_elevated_span(src.pos, dst.pos, env);
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

        let id = roads.insert_with_key(|id| Self {
            id,
            src: src.id,
            dst: dst.id,
            src_interface: SourceInterface::new(9.0, elevated_span),
            dst_interface: 9.0,
            width,
            lanes_forward: vec![],
            lanes_backward: vec![],
            interfaced_points: PolyLine3::new(vec![points.first()]),
            points,
            connected_buildings: vec![],
        });
        #[allow(clippy::indexing_slicing)]
        let road = &mut roads[id];

        let mut dist_from_bottom = 0.0;
        for (lane_k, dir, limit) in lane_pattern.lanes() {
            let id = Lane::make(road, lanes, lane_k, limit, dir, dist_from_bottom);

            match dir {
                LaneDirection::Forward => road.lanes_forward.insert(0, (id, lane_k)),
                LaneDirection::Backward => road.lanes_backward.push((id, lane_k)),
            }

            dist_from_bottom += lane_k.width();
        }

        road.update_lanes(lanes, parking, env);

        spatial.insert(road);
        road.id
    }

    pub fn is_one_way(&self) -> bool {
        self.lanes_forward.is_empty() || self.lanes_backward.is_empty()
    }

    pub fn n_lanes(&self) -> usize {
        self.lanes_backward.len() + self.lanes_forward.len()
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

    pub fn pattern(&self, lanes: &Lanes) -> LanePattern {
        LanePattern {
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
                let h = env.true_height(pos.xy())?;
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

    fn update_interfaced_points(&mut self, env: &Environment) {
        let points = &self.points;
        let clipped = points.cut(self.interface_from(self.src), self.interface_from(self.dst));

        if self.src_interface.elevated_span() {
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

    fn is_elevated_span(from: Vec3, to: Vec3, env: &Environment) -> bool {
        // Allow for rounding when an endpoint height round-trips through ROAD_Z_OFFSET.
        [from, to].iter().all(|pos| {
            env.height(pos.xy())
                .is_some_and(|ground| pos.z - ground > 0.001)
        })
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
            let h = env.height(pos).unwrap_or_else(|| {
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

        // merge interface points that overlap
        let mut i = 0;
        while i + 3 < interface.len() {
            if interface[i + 1] >= interface[i + 2] {
                interface.remove(i + 2);
                interface.remove(i + 1);
            } else {
                i += 2;
            }
        }

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

    fn generate_points_with_span(
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
            (self.src_interface.distance(), self.dst_interface)
        } else if id == self.dst {
            (self.dst_interface, self.src_interface.distance())
        } else {
            panic!("Asking interface from from an intersection not connected to the road");
        }
    }

    pub fn set_interface(&mut self, id: IntersectionID, v: f32) {
        if id == self.src {
            self.src_interface.set_distance(v);
        } else if id == self.dst {
            self.dst_interface = v;
        } else {
            panic!("Setting interface from from an intersection not connected to the road");
        }
    }

    pub fn max_interface(&mut self, id: IntersectionID, v: f32) {
        if id == self.src {
            self.src_interface
                .set_distance(self.src_interface.distance().max(v));
        } else if id == self.dst {
            self.dst_interface = self.dst_interface.max(v);
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
