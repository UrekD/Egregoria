use geom::{Vec2, Vec2d, Vec3};

pub(super) fn bounds_overlap(a: (Vec2, Vec2), b: (Vec2, Vec2), radius: f32) -> bool {
    let ((alo, ahi), (blo, bhi)) = (a, b);
    alo.x <= bhi.x + radius
        && blo.x <= ahi.x + radius
        && alo.y <= bhi.y + radius
        && blo.y <= ahi.y + radius
}

pub(super) fn segment_bounds(a: Vec3, b: Vec3) -> (Vec2, Vec2) {
    (a.xy().min(b.xy()), a.xy().max(b.xy()))
}

pub(super) fn live_conflict(
    a: &[Vec3],
    b: &[Vec3],
    radius: f32,
    junction: Option<(Vec2, f32)>,
    clearance: [f64; 2],
) -> bool {
    let segments = |profile: &[Vec3]| {
        profile
            .windows(2)
            .map(|pair| {
                let [start, end] = [pair[0], pair[1]];
                let segment = Segment {
                    xy: xy64(start.xy()),
                    run: xy64(end.xy()) - xy64(start.xy()),
                    z: f64::from(start.z),
                    rise: f64::from(end.z) - f64::from(start.z),
                };
                let outside = segment.outside(junction);
                (segment, outside, segment_bounds(start, end))
            })
            .collect::<Vec<_>>()
    };
    let a = segments(a);
    let b = segments(b);
    let full = [0.0, 1.0];
    let radius_sq = f64::from(radius).powi(2);
    for &(a, outside_a, bounds_a) in &a {
        for &(b, outside_b, bounds_b) in &b {
            if !bounds_overlap(bounds_a, bounds_b, radius) {
                continue;
            }
            // A shared junction exempts only pairs with both points inside its
            // pavement envelope. Search wherever either point lies outside.
            if outside_a
                .into_iter()
                .flatten()
                .any(|t| segment_conflict(a, b, t, full, radius_sq, clearance))
                || (outside_a[0] != Some(full)
                    && outside_b
                        .into_iter()
                        .flatten()
                        .any(|u| segment_conflict(a, b, full, u, radius_sq, clearance)))
            {
                return true;
            }
        }
    }
    false
}

fn xy64(point: Vec2) -> Vec2d {
    Vec2d::new(f64::from(point.x), f64::from(point.y))
}

#[derive(Clone, Copy)]
struct Segment {
    xy: Vec2d,
    run: Vec2d,
    z: f64,
    rise: f64,
}

impl Segment {
    fn outside(self, junction: Option<(Vec2, f32)>) -> [Option<[f64; 2]>; 2] {
        let full = [Some([0.0, 1.0]), None];
        let Some((centre, radius)) = junction else {
            return full;
        };
        let offset = self.xy - xy64(centre);
        let radius_sq = f64::from(radius).powi(2);
        if offset.magnitude2() <= radius_sq && (offset + self.run).magnitude2() <= radius_sq {
            return [None, None];
        }
        let length_sq = self.run.magnitude2();
        if length_sq == 0.0 {
            return full;
        }
        let nearest = -offset.dot(self.run) / length_sq;
        let inside_sq = radius_sq - (offset + nearest * self.run).magnitude2();
        if inside_sq <= 0.0 {
            return full;
        }
        let half = (inside_sq / length_sq).sqrt();
        let enter = nearest - half;
        let exit = nearest + half;
        [
            (enter > 0.0).then_some([0.0, enter.min(1.0)]),
            (exit < 1.0).then_some([exit.max(0.0), 1.0]),
        ]
    }
}

fn segment_conflict(
    a: Segment,
    b: Segment,
    [t0, t1]: [f64; 2],
    [u0, u1]: [f64; 2],
    radius_sq: f64,
    [lower, upper]: [f64; 2],
) -> bool {
    // The parameters of two points form a rectangle. Their height difference
    // is affine, so insufficient clearance clips it to a convex polygon.
    let rectangle = [
        Vec2d::new(t0, u0),
        Vec2d::new(t1, u0),
        Vec2d::new(t1, u1),
        Vec2d::new(t0, u1),
    ];
    let dz = |p: Vec2d| (a.z - b.z) + a.rise * p.x - b.rise * p.y;
    let heights = rectangle.map(dz);
    if heights.iter().all(|&z| z <= lower) || heights.iter().all(|&z| z >= upper) {
        return false;
    }

    // The only possible interior minimum of horizontal separation for
    // nonparallel runs is a centreline intersection. For parallel runs the
    // boundary also attains any interior minimum.
    let offset = a.xy - b.xy;
    let determinant = a.run.cross(b.run);
    if determinant != 0.0 {
        let t = b.run.cross(offset) / determinant;
        let u = a.run.cross(offset) / determinant;
        if (t0..=t1).contains(&t) && (u0..=u1).contains(&u) {
            let separation = dz(Vec2d::new(t, u));
            if (lower..=upper).contains(&separation) {
                return true;
            }
        }
    }

    let polygon = clip(&rectangle, |p| dz(p) - lower);
    let polygon = clip(&polygon, |p| upper - dz(p));
    let difference = |p: Vec2d| offset + p.x * a.run - p.y * b.run;
    let mut sides = edges(&polygon);
    sides.any(|(p, q)| {
        let start = difference(p);
        let delta = difference(q) - start;
        let length_sq = delta.magnitude2();
        let nearest = if length_sq > 0.0 {
            start + (-start.dot(delta) / length_sq).clamp(0.0, 1.0) * delta
        } else {
            start
        };
        nearest.magnitude2() < radius_sq
    })
}

fn clip(polygon: &[Vec2d], margin: impl Fn(Vec2d) -> f64) -> Vec<Vec2d> {
    let mut clipped = Vec::with_capacity(polygon.len() + 1);
    for (a, b) in edges(polygon) {
        let (ma, mb) = (margin(a), margin(b));
        if ma >= 0.0 {
            clipped.push(a);
        }
        if (ma < 0.0 && mb > 0.0) || (ma > 0.0 && mb < 0.0) {
            clipped.push(a.lerp(b, ma / (ma - mb)));
        }
    }
    clipped
}

fn edges(polygon: &[Vec2d]) -> impl Iterator<Item = (Vec2d, Vec2d)> + '_ {
    polygon
        .iter()
        .copied()
        .zip(polygon.iter().copied().cycle().skip(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::vec3;

    #[test]
    fn citymap_sweep_checks_clearance_at_the_edge_of_a_crossing() {
        let road = [vec3(-10.0, 0.0, 10.0), vec3(10.0, 0.0, 10.0)];
        let track = [vec3(0.0, -10.0, 28.0), vec3(0.0, 10.0, 8.0)];
        assert!(live_conflict(&road, &track, 5.0, None, [-6.0, 6.5]));
        assert!(!live_conflict(&road, &track, 2.0, None, [-6.0, 6.5]));
    }

    #[test]
    fn citymap_sweep_respects_the_clearance_and_width_boundaries() {
        let road = [vec3(0.0, 0.0, 10.0), vec3(20.0, 0.0, 10.0)];
        let track = road.map(|p| p + vec3(0.0, 8.0, -6.0));
        assert!(live_conflict(&road, &track, 9.0, None, [-6.0, 6.5]));
        assert!(!live_conflict(&road, &track, 8.0, None, [-6.0, 6.5]));
        let lower_track = track.map(|p| p - vec3(0.0, 0.0, 0.5));
        assert!(!live_conflict(&road, &lower_track, 9.0, None, [-6.0, 6.5]));
    }

    #[test]
    fn citymap_sweep_only_exempts_overlap_inside_the_shared_junction() {
        let road = [Vec3::ZERO, vec3(20.0, 0.0, 0.0)];
        let other = [Vec3::ZERO, vec3(0.0, 20.0, 0.0)];
        assert!(live_conflict(
            &road,
            &other,
            5.0,
            Some((Vec2::ZERO, 4.0)),
            [-6.0, 6.0]
        ));
        assert!(!live_conflict(
            &road,
            &other,
            5.0,
            Some((Vec2::ZERO, 5.0)),
            [-6.0, 6.0]
        ));
    }
}
