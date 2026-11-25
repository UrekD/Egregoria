//! Frontage-aligned massing and construction grade for checked growables.
use crate::map::procgen::ColoredMesh;
use crate::map::terrain::CELL_SIZE;
use crate::map::{Building, Density, Environment, Map, ZoneCell, ZoneKind};
use geom::{vec2, LinearColor, Ray3, Shape, Vec2, Vec3, AABB, AABB3, OBB};
use prototypes::{
    GrowableMassing, GrowableMassingStyle, GROWABLE_FLOOR_HEIGHT_M, MAX_PROFILED_SLOTS,
    MIXED_USE_SHOP_FLOOR_HEIGHT_M,
};

use serde::{Deserialize, Serialize};

const SETBACK_M: f32 = 1.0;
const WALK_HALF_WIDTH_M: f32 = 1.2;
const WALK_MAX_GRADE: f32 = 0.20;
const MAX_PLOT_AXIS_M: f32 = u16::MAX as f32 / 10.0;
const FOUNDATION_CLEARANCE_M: f32 = 0.025;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GrowableForm {
    Stepped,
    BroadBase,
    FlatCap,
}

impl GrowableForm {
    const ALL: [Self; 3] = [Self::Stepped, Self::BroadBase, Self::FlatCap];

    fn seeded(center: Vec2, zone: ZoneCell) -> Self {
        // Pack the two signed, rounded decimeter coordinates in two's complement.
        // Salt them with the fixed zoning ordinals, then use the SplitMix64 finalizer.
        let dm = |axis: f32| (f64::from(axis) * 10.0).round() as i32 as u32;
        let xy = (u64::from(dm(center.x)) << 32) | u64::from(dm(center.y));
        let salt = 1 + u64::from(zone.kind.ordinal()) * 3 + u64::from(zone.density.ordinal());
        let mut bits = xy ^ salt.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        bits = (bits ^ (bits >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        bits = (bits ^ (bits >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        bits ^= bits >> 31;
        Self::ALL[(bits % 3) as usize]
    }

    fn index(self) -> usize {
        self as usize
    }
}

pub(crate) fn grade_revision(map: &Map) -> (u64, u64, u64) {
    let env = map.environment();
    (
        env.base_revision,
        env.water_revision(),
        map.construction_cut_revision,
    )
}

pub(crate) fn grade_nodes(env: &Environment, parcel: OBB) -> impl Iterator<Item = u32> + '_ {
    let bounds = parcel.bbox();
    // Include the next node on each axis. Bilinear cells along the plot edge,
    // including the frontage landing, also depend on nodes outside the plot.
    let ll = (bounds.ll / CELL_SIZE).floor();
    let ur = (bounds.ur / CELL_SIZE).floor() + Vec2::splat(1.0);
    grid_points(ll, ur).map(|point| {
        env.heightmap
            .height_nearest(point)
            .map_or(f32::NAN.to_bits(), f32::to_bits)
    })
}

/// An inset feasible at construction grade. The lot remains the reserved map footprint.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct GrowableFormFit {
    form: GrowableForm,
    ground: OBB,
    shaft: OBB,
    base_z: f32,
    door: Vec3,
    walk: [Vec3; 4],
    frontage_landing: Vec3,
}

impl PartialEq for GrowableFormFit {
    fn eq(&self, other: &Self) -> bool {
        self.form == other.form
            && self.ground.corners == other.ground.corners
            && self.shaft.corners == other.shaft.corners
            && self.base_z == other.base_z
            && self.door == other.door
            && self.walk == other.walk
            && self.frontage_landing == other.frontage_landing
    }
}

impl GrowableFormFit {
    pub fn form(self) -> GrowableForm {
        self.form
    }

    pub fn footprint(self) -> OBB {
        self.ground
    }

    pub fn shaft(self) -> OBB {
        self.shaft
    }

    pub fn base_z(self) -> f32 {
        self.base_z
    }

    pub fn door(self) -> Vec3 {
        self.door
    }

    /// Part of the connector on the road-facing plot edge, in world space.
    pub fn frontage_landing(self) -> Vec3 {
        self.frontage_landing
    }

    pub(crate) fn check(
        env: &Environment,
        parcel: OBB,
        zone: ZoneCell,
        style: GrowableMassingStyle,
        band: u8,
    ) -> Option<Self> {
        Self::check_grade(parcel, zone, style, band, |point| env.true_height(point))
    }

    fn check_grade(
        parcel: OBB,
        zone: ZoneCell,
        style: GrowableMassingStyle,
        band: u8,
        terrain: impl Fn(Vec2) -> Option<f32>,
    ) -> Option<Self> {
        let [frontage, depth] = parcel.axis();
        let [width_m, depth_m] = [frontage.mag(), depth.mag()];
        if ![width_m, depth_m]
            .into_iter()
            .all(|axis| (0.1..=MAX_PLOT_AXIS_M).contains(&axis))
        {
            return None;
        }
        let front = (parcel.corners[0] + parcel.corners[1]) * 0.5;
        let across = frontage / width_m;
        let inward = depth / depth_m;
        if !across.is_finite() || !inward.is_finite() || across.dot(inward).abs() > 1e-3 {
            return None;
        }
        let center = parcel.center();
        let seeded = GrowableForm::seeded(center, zone);

        std::iter::once(seeded)
            .chain(GrowableForm::ALL.into_iter().filter(|&form| form != seeded))
            .find_map(|form| {
                let tall = zone.density == Density::High || style == GrowableMassingStyle::Tower;
                let (ratios, caps): ([[f32; 2]; 3], [[f32; 2]; 3]) = match zone.density {
                    Density::High => (
                        [[0.42, 0.52], [0.50, 0.44], [0.56, 0.49]],
                        [[16.0, 19.0], [19.0, 16.0], [21.0, 19.0]],
                    ),
                    Density::Medium => (
                        [[0.54, 0.61], [0.63, 0.53], [0.69, 0.57]],
                        [[22.0, 24.0], [25.0, 22.0], [28.0, 24.0]],
                    ),
                    Density::Low => (
                        [[0.48, 0.52], [0.56, 0.44], [0.62, 0.48]],
                        [[14.0, 17.0], [17.0, 14.0], [19.0, 16.0]],
                    ),
                };
                // Reserve a metre of headroom in the caps for the two value increments.
                let minimum = if tall { 9.0 } else { 4.0 };
                let dims = [width_m, depth_m];
                let mut shaft = [0.0; 2];
                let mut ground = [0.0; 2];
                for (i, dim) in dims.into_iter().enumerate() {
                    let cap = (caps[form.index()][i]).min(dim - SETBACK_M * 2.0);
                    if cap < minimum {
                        return None;
                    }
                    let nominal = (dim * ratios[form.index()][i]).min(cap - 1.0).max(minimum);
                    shaft[i] = (((nominal + f32::from(band) * 0.5) * 10.0).round())
                        .min((cap * 10.0).floor())
                        / 10.0;
                    let podium = form == GrowableForm::BroadBase
                        || style == GrowableMassingStyle::Industrial
                        || zone.kind == ZoneKind::MixedUse;
                    let extension = if podium { 2.0 } else { 0.0 };
                    ground[i] = ((shaft[i] + extension) * 10.0)
                        .round()
                        .min(((dim - SETBACK_M * 2.0) * 10.0).floor())
                        / 10.0;
                }
                let shape =
                    |[width, depth]: [f32; 2]| rectangle(center, across, inward, width, depth);
                let shaft = shape(shaft);
                let ground = shape(ground);
                let base_z = maximum_grade(ground, &terrain)?.max(0.0) + FOUNDATION_CLEARANCE_M;
                let door_xy = (ground.corners[0] + ground.corners[1]) * 0.5;
                let offset = across * WALK_HALF_WIDTH_M;
                let a = front - offset;
                let b = front + offset;
                let fit = Self {
                    form,
                    ground,
                    shaft,
                    base_z,
                    door: door_xy.z(base_z),
                    walk: [
                        a.z(terrain(a)?.max(0.0)),
                        b.z(terrain(b)?.max(0.0)),
                        (door_xy + offset).z(base_z),
                        (door_xy - offset).z(base_z),
                    ],
                    frontage_landing: front.z(terrain(front)?.max(0.0)),
                };
                for triangle in fit.walkway_triangles() {
                    check_walkway_triangle(triangle, &terrain)?;
                }
                Some(fit)
            })
    }

    fn walkway_triangles(self) -> [[Vec3; 3]; 4] {
        let [a, b, c, d] = self.walk;
        let front = self.frontage_landing;
        [
            [a, front, d],
            [front, self.door, d],
            [front, b, c],
            [front, c, self.door],
        ]
    }

    fn skirt_edges(self) -> impl Iterator<Item = (Vec3, Vec3)> {
        let ground = self.ground.segments().into_iter().flat_map(move |edge| {
            let mut previous = 0.0;
            cell_crossings(edge.src, edge.dst)
                .into_iter()
                .skip(1)
                .map(move |hi| {
                    let lo = std::mem::replace(&mut previous, hi);
                    (
                        point_along(edge.src, edge.dst, lo).z(self.base_z),
                        point_along(edge.src, edge.dst, hi).z(self.base_z),
                    )
                })
        });
        let [a, b, c, d] = self.walk;
        let walkway = [(d, a), (b, c)].into_iter().flat_map(|(from, to)| {
            // Limit mesh size on deep parcels. Each panel still samples the
            // lowest grade along the full interval when the shell is frozen.
            let panels = (from.xy().distance(to.xy()) / CELL_SIZE)
                .ceil()
                .clamp(1.0, 16.0) as u16;
            let step = (to - from) / f32::from(panels);
            (0..panels).map(move |i| (from + step * f32::from(i), from + step * f32::from(i + 1)))
        });
        ground.chain(walkway)
    }

    pub(crate) fn freeze(
        self,
        env: &Environment,
        style: GrowableMassingStyle,
    ) -> Option<FrozenGrowableShell> {
        let terrain = |point| env.true_height(point);
        let skirt_bottoms = self
            .skirt_edges()
            .map(|(a, b)| {
                let grade = segment_grade(a.xy(), b.xy(), &terrain)?.0.max(0.0);
                Some(grade.min(a.z).min(b.z) - 0.05)
            })
            .collect::<Option<Vec<_>>>()?;
        Some(FrozenGrowableShell {
            fit: self,
            style,
            skirt_bottoms,
        })
    }
}

/// The placement geometry and grade anchors required to rebuild the shell at any level.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FrozenGrowableShell {
    fit: GrowableFormFit,
    style: GrowableMassingStyle,
    skirt_bottoms: Vec<f32>,
}

impl FrozenGrowableShell {
    pub fn fit(&self) -> GrowableFormFit {
        self.fit
    }

    pub fn style(&self) -> GrowableMassingStyle {
        self.style
    }

    pub(crate) fn valid_for(&self, parcel: &OBB) -> bool {
        let fit = self.fit;
        let bounds = parcel.expand(0.01);
        let inside = |point: Vec2| point.is_finite() && bounds.contains(point);
        let valid_inset = |shape: OBB| {
            shape.corners.into_iter().all(inside)
                && shape
                    .axis()
                    .iter()
                    .all(|axis| (3.98..=31.0).contains(&axis.mag()))
        };
        fit.base_z.is_finite()
            && fit.base_z >= 0.0
            && fit.door.is_finite()
            && fit.door.z == fit.base_z
            && inside(fit.door.xy())
            && valid_inset(fit.ground)
            && valid_inset(fit.shaft)
            && fit
                .walk
                .iter()
                .chain(std::iter::once(&fit.frontage_landing))
                .all(|p| p.is_finite() && inside(p.xy()))
            && self.skirt_bottoms.len() <= 56
            && fit.skirt_edges().count() == self.skirt_bottoms.len()
            && fit
                .skirt_edges()
                .zip(&self.skirt_bottoms)
                .all(|((a, b), &bottom)| bottom.is_finite() && bottom < a.z.min(b.z))
    }

    pub(crate) fn mesh(&self, zone: ZoneCell, floors: u8, level: u8) -> Option<(ColoredMesh, f32)> {
        if !(1..=28).contains(&floors) || !(1..=5).contains(&level) {
            return None;
        }
        let fit = self.fit;
        let style = self.style;
        use GrowableMassingStyle::{House, Industrial};

        let mixed = zone.kind == ZoneKind::MixedUse;
        let ground_floor = if mixed {
            MIXED_USE_SHOP_FLOOR_HEIGHT_M
        } else {
            GROWABLE_FLOOR_HEIGHT_M
        };
        let base = fit.base_z;
        let eaves = base + ground_floor + f32::from(floors - 1) * GROWABLE_FLOOR_HEIGHT_M;
        let colors = match zone.kind {
            ZoneKind::Residential => [rgb(0.76, 0.72, 0.61), rgb(0.42, 0.54, 0.44)],
            ZoneKind::Commercial => [rgb(0.47, 0.62, 0.75), rgb(0.31, 0.45, 0.60)],
            ZoneKind::Industrial => [rgb(0.68, 0.61, 0.47), rgb(0.49, 0.42, 0.32)],
            ZoneKind::Office => [rgb(0.67, 0.61, 0.76), rgb(0.44, 0.41, 0.55)],
            ZoneKind::MixedUse => [rgb(0.62, 0.74, 0.68), rgb(0.27, 0.52, 0.49)],
        };

        let mut mesh = ColoredMesh::default();
        let has_podium = style != House && fit.ground.corners != fit.shaft.corners && floors > 1;
        let shaft_bottom = if has_podium {
            let top = base + ground_floor;
            walls(&mut mesh, fit.ground, base, top, colors[1]);
            ledge(&mut mesh, fit.ground, fit.shaft, top, colors[1]);
            top
        } else {
            base
        };

        let (roof_z, facade_top) = if style == House {
            walls(&mut mesh, fit.ground, base, eaves, colors[0]);
            let [a, b, c, d] = fit.ground.corners.map(|p| p.z(eaves));
            let height = 1.2 + fit.form.index() as f32 * 0.35 + f32::from(level - 1) * 0.2;
            let roof_z = eaves + height;
            let left = ((a + d) * 0.5).xy().z(roof_z);
            let right = ((b + c) * 0.5).xy().z(roof_z);
            mesh.faces.extend([
                (vec![a, b, right, left], colors[1]),
                (vec![d, left, right, c], colors[1]),
                (vec![a, left, d], colors[1]),
                (vec![b, c, right], colors[1]),
            ]);
            (roof_z, eaves)
        } else {
            let body = if has_podium { fit.shaft } else { fit.ground };
            let (main_top, crown, crown_height) = match fit.form {
                GrowableForm::Stepped => (eaves - 0.7, body.expand(-0.8), 1.1),
                GrowableForm::BroadBase => (eaves, body.expand(-1.2), 0.8),
                GrowableForm::FlatCap => (eaves, body, 0.35),
            };
            banded_walls(
                &mut mesh,
                body,
                shaft_bottom,
                main_top,
                colors,
                style == Industrial,
            );
            if body.corners != crown.corners {
                ledge(&mut mesh, body, crown, main_top, colors[1]);
            }
            // Leave a visible crown increment even when the profile caps the floor count.
            let roof_z = main_top + crown_height + f32::from(level - 1) * 0.2;
            walls(&mut mesh, crown, main_top, roof_z, colors[1]);
            mesh.faces
                .push((crown.corners.map(|p| p.z(roof_z)).to_vec(), colors[1]));
            (roof_z, if has_podium { shaft_bottom } else { main_top })
        };

        let [left, right, _, _] = fit.ground.corners;
        let across = (right - left).normalize();
        let outward = (fit.door.xy() - fit.ground.center()).normalize();
        let door = fit.door + outward.z0() * 0.025;
        facade_panel(
            &mut mesh,
            door,
            across,
            1.0,
            0.0,
            2.35,
            rgb(0.22, 0.26, 0.28),
        );
        if mixed || style == Industrial {
            let half_width = left.distance(right) * 0.5 - 0.35;
            let glass = if mixed {
                rgb(0.24, 0.46, 0.51)
            } else {
                rgb(0.38, 0.40, 0.41)
            };
            let band_top = (facade_top - base).min(3.4);
            let band_bottom = (band_top - 0.35).max(2.4);
            let glass_top = (band_bottom - 0.05).min(3.0);
            let offset = across.z0() * (half_width + 1.15) * 0.5;
            let half_panel = (half_width - 1.15) * 0.5;
            for side in [-1.0, 1.0] {
                facade_panel(
                    &mut mesh,
                    door + offset * side,
                    across,
                    half_panel,
                    0.35,
                    glass_top,
                    glass,
                );
            }
            facade_panel(
                &mut mesh,
                door,
                across,
                half_width,
                band_bottom,
                band_top,
                colors[1],
            );
        }

        let concrete = LinearColor::gray(0.37);
        mesh.faces.extend(
            fit.walkway_triangles()
                .into_iter()
                .map(|triangle| (triangle.to_vec(), concrete)),
        );
        for ((a, b), &bottom) in fit.skirt_edges().zip(&self.skirt_bottoms) {
            mesh.faces
                .push((vec![a.xy().z(bottom), b.xy().z(bottom), b, a], concrete));
        }
        Some((mesh, roof_z))
    }
}

fn rectangle(center: Vec2, across: Vec2, inward: Vec2, width: f32, depth: f32) -> OBB {
    let across = across * (width * 0.5);
    let inward = inward * (depth * 0.5);
    OBB::new_corners([
        center - across - inward,
        center + across - inward,
        center + across + inward,
        center - across + inward,
    ])
}

fn point_along(a: Vec2, b: Vec2, t: f32) -> Vec2 {
    a + (b - a) * t
}

fn cell_crossings(a: Vec2, b: Vec2) -> Vec<f32> {
    let mut cuts = vec![0.0, 1.0];
    for (a, b) in [(a.x, b.x), (a.y, b.y)] {
        let first = (a.min(b) / CELL_SIZE).floor() as i32 + 1;
        let last = (a.max(b) / CELL_SIZE).ceil() as i32 - 1;
        cuts.extend((first..=last).map(|i| (i as f32 * CELL_SIZE - a) / (b - a)));
    }
    cuts.sort_unstable_by(f32::total_cmp);
    cuts.dedup_by(|a, b| (*a - *b).abs() < 1e-6);
    cuts
}

fn grid_points(ll: Vec2, ur: Vec2) -> impl Iterator<Item = Vec2> {
    (ll.y as i32..=ur.y as i32).flat_map(move |y| {
        (ll.x as i32..=ur.x as i32).map(move |x| vec2(x as f32, y as f32) * CELL_SIZE)
    })
}

fn maximum_grid_grade(
    bounds: AABB,
    contains: impl Fn(Vec2) -> bool,
    height: &impl Fn(Vec2) -> Option<f32>,
) -> Option<f32> {
    let ll = (bounds.ll / CELL_SIZE).ceil();
    let ur = (bounds.ur / CELL_SIZE).floor();
    let mut maximum = f32::NEG_INFINITY;
    for point in grid_points(ll, ur).filter(|&point| contains(point)) {
        maximum = maximum.max(height(point).filter(|z| z.is_finite())?);
    }
    Some(maximum)
}

/// A bilinear patch has no strict interior maximum. Its grid nodes and the
/// quadratic extrema of footprint edges inside each terrain cell suffice.
fn maximum_grade(shape: OBB, height: &impl Fn(Vec2) -> Option<f32>) -> Option<f32> {
    let mut max_z = maximum_grid_grade(shape.bbox(), |point| shape.contains(point), height)?;
    for edge in shape.segments() {
        max_z = max_z.max(segment_grade(edge.src, edge.dst, height)?.1);
    }
    Some(max_z)
}

fn check_walkway_triangle(
    [a, b, c]: [Vec3; 3],
    height: &impl Fn(Vec2) -> Option<f32>,
) -> Option<()> {
    const CLEARANCE_EPSILON_M: f32 = 0.001;
    let normal = (b - a).cross(c - a);
    let slope = -normal.xy() / normal.z;
    if !slope.is_finite() || slope.mag2() > WALK_MAX_GRADE.powi(2) {
        return None;
    }

    // Subtracting the ramp plane leaves a bilinear surface in every terrain
    // cell. Interior grid nodes and the extrema along the triangle boundary
    // include the maximum on each clipped cell, including across the ramp.
    let plane = |point| a.z + slope.dot(point - a.xy());
    let residual = |point| Some(height(point)? - plane(point));
    let [a, b, c] = [a.xy(), b.xy(), c.xy()];
    let edges = [(a, b), (b, c), (c, a)];
    let bounds = AABB::new_ll_ur(a.min(b).min(c), a.max(b).max(c));
    let contains = |point| {
        edges
            .into_iter()
            .all(|(from, to)| (to - from).cross(point - from) * normal.z.signum() >= 0.0)
    };
    let mut maximum = maximum_grid_grade(bounds, contains, &residual)?;
    for (from, to) in edges {
        let (_, peak) = segment_cell_grade(from, to, |points| {
            // Sample both ends in the interval's own cell. At the outer
            // row or column the heightmap falls back to the lower-left node.
            let origin = (points[1] / CELL_SIZE).floor() * CELL_SIZE;
            let ll = height(origin)?;
            let [lr, ul, ur] = [Vec2::X, Vec2::Y, Vec2::splat(1.0)]
                .map(|offset| height(origin + CELL_SIZE * offset));
            Some(points.map(|point| {
                let grade = if let [Some(lr), Some(ul), Some(ur)] = [lr, ul, ur] {
                    let local = (point - origin) / CELL_SIZE;
                    let low = ll + local.x * (lr - ll);
                    let high = ul + local.x * (ur - ul);
                    low + local.y * (high - low)
                } else {
                    ll
                };
                grade - plane(point)
            }))
        })?;
        maximum = maximum.max(peak);
    }
    (maximum <= CLEARANCE_EPSILON_M).then_some(())
}

fn segment_grade(a: Vec2, b: Vec2, height: &impl Fn(Vec2) -> Option<f32>) -> Option<(f32, f32)> {
    segment_cell_grade(a, b, |[start, mid, end]| {
        Some([height(start)?, height(mid)?, height(end)?])
    })
}

fn segment_cell_grade(
    a: Vec2,
    b: Vec2,
    sample: impl Fn([Vec2; 3]) -> Option<[f32; 3]>,
) -> Option<(f32, f32)> {
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    let cuts = cell_crossings(a, b);
    for pair in cuts.windows(2) {
        let (lo, hi) = (pair[0], pair[1]);
        let [start, mid, end] = sample([lo, (lo + hi) * 0.5, hi].map(|t| point_along(a, b, t)))?;
        if ![start, mid, end].into_iter().all(f32::is_finite) {
            return None;
        }
        min = min.min(start).min(mid).min(end);
        max = max.max(start).max(mid).max(end);
        let square = 2.0 * (start + end - 2.0 * mid);
        let linear = end - start - square;
        let stationary = -linear / (2.0 * square);
        if (0.0..1.0).contains(&stationary) {
            let z = start + stationary * (linear + stationary * square);
            min = min.min(z);
            max = max.max(z);
        }
    }
    Some((min, max))
}

fn rgb(r: f32, g: f32, b: f32) -> LinearColor {
    LinearColor::new(r, g, b, 1.0)
}

fn walls(mesh: &mut ColoredMesh, shape: OBB, bottom: f32, top: f32, color: LinearColor) {
    for edge in shape.segments() {
        mesh.faces.push((
            vec![
                edge.src.z(bottom),
                edge.dst.z(bottom),
                edge.dst.z(top),
                edge.src.z(top),
            ],
            color,
        ));
    }
}

fn ledge(mesh: &mut ColoredMesh, lower: OBB, upper: OBB, z: f32, color: LinearColor) {
    for (outer, inner) in lower.segments().into_iter().zip(upper.segments()) {
        mesh.faces.push((
            vec![
                outer.src.z(z),
                outer.dst.z(z),
                inner.dst.z(z),
                inner.src.z(z),
            ],
            color,
        ));
    }
}

fn banded_walls(
    mesh: &mut ColoredMesh,
    shape: OBB,
    bottom: f32,
    top: f32,
    colors: [LinearColor; 2],
    industrial: bool,
) {
    let interval = GROWABLE_FLOOR_HEIGHT_M * if industrial { 2.0 } else { 3.0 };
    let mut z = bottom;
    while z + interval < top - 0.25 {
        let next = z + interval;
        walls(mesh, shape, z, next - 0.2, colors[0]);
        walls(mesh, shape, next - 0.2, next, colors[1]);
        z = next;
    }
    walls(mesh, shape, z, top, colors[0]);
}

fn facade_panel(
    mesh: &mut ColoredMesh,
    at: Vec3,
    across: Vec2,
    half_width: f32,
    bottom: f32,
    top: f32,
    color: LinearColor,
) {
    let a = at - across.z0() * half_width;
    let b = at + across.z0() * half_width;
    mesh.faces.push((
        vec![a.up(bottom), b.up(bottom), b.up(top), a.up(top)],
        color,
    ));
}

pub(crate) fn realized_floors(
    profile: GrowableMassing,
    density: Density,
    level: u8,
    form: GrowableForm,
    band: u8,
) -> Option<u8> {
    let nominal = profile.nominal_floors(level)?;
    let bonus = match density {
        Density::Low => 0,
        Density::Medium | Density::High => form.index() as u8 + band,
    };
    Some(nominal.saturating_add(bonus).min(profile.max_floors))
}

fn scale_slots(nominal: u32, floors: u8, nominal_floors: u8) -> Option<u64> {
    let divisor = u64::from(nominal_floors);
    let numerator = u64::from(nominal) * u64::from(floors) + divisor / 2;
    (nominal > 0 && floors > 0)
        .then_some(numerator)?
        .checked_div(divisor)
}

pub(crate) fn housing_slots(
    nominal: u32,
    floors: u8,
    nominal_floors: u8,
    mixed: bool,
) -> Option<u32> {
    let retail = u8::from(mixed);
    let slots = scale_slots(
        nominal,
        floors.checked_sub(retail)?,
        nominal_floors.checked_sub(retail)?,
    )?;
    Some(slots.clamp(1, MAX_PROFILED_SLOTS.into()) as u32)
}

pub(crate) fn job_slots(
    zone: ZoneKind,
    nominal: u32,
    floors: u8,
    nominal_floors: u8,
) -> Option<u32> {
    if zone == ZoneKind::MixedUse {
        return (nominal == 12).then_some(12);
    }
    let cap = if zone == ZoneKind::Office {
        u64::from(nominal) * 5 / 2
    } else {
        u64::from(nominal) * 2
    };
    let slots = scale_slots(nominal, floors, nominal_floors)?;
    Some(slots.clamp(1, cap.min(MAX_PROFILED_SLOTS.into())) as u32)
}

/// The same world-space surfaces for rendering and picking, including entrances,
/// foundation and roof. Faces are convex and use the triangle fan exposed here.
#[derive(Clone, Copy)]
pub struct ShellGeometry<'a> {
    building: &'a Building,
}

impl Building {
    pub fn growable_shell(&self) -> Option<ShellGeometry<'_>> {
        self.development()?;
        (!self.mesh.faces.is_empty()).then_some(ShellGeometry { building: self })
    }
}

impl ShellGeometry<'_> {
    pub fn base_z(&self) -> f32 {
        self.building.height
    }

    pub fn roof_z(&self) -> f32 {
        self.building.development().unwrap().roof_z
    }

    pub fn door(&self) -> Vec3 {
        self.building.door_pos
    }

    pub fn bounds(&self) -> AABB3 {
        self.faces()
            .flat_map(|(face, _)| face.iter().copied())
            .fold(AABB3::new(self.door(), self.door()), AABB3::union_vec)
    }

    pub fn faces(&self) -> impl Iterator<Item = (&[Vec3], LinearColor)> {
        self.building
            .mesh
            .faces
            .iter()
            .map(|(face, color)| (face.as_slice(), *color))
    }

    pub fn triangles(&self) -> impl Iterator<Item = ([Vec3; 3], LinearColor)> + '_ {
        self.faces().flat_map(|(face, color)| {
            face.get(1..)
                .unwrap_or_default()
                .windows(2)
                .map(move |edge| ([face[0], edge[0], edge[1]], color))
        })
    }

    /// Distance along a finite ray, with its direction normalized internally.
    pub fn ray_depth(&self, ray: Ray3, limit: f32) -> Option<f32> {
        if !ray.from.is_finite() || !ray.dir.is_finite() || !limit.is_finite() || limit < 0.0 {
            return None;
        }
        let direction = ray.dir.try_normalize()?;
        self.triangles()
            .filter_map(|([a, b, c], _)| {
                let ab = b - a;
                let ac = c - a;
                let p = direction.cross(ac);
                let det = ab.dot(p);
                if det.abs() < 1e-6 {
                    return None;
                }
                let from = ray.from - a;
                let u = from.dot(p) / det;
                let q = from.cross(ab);
                let v = direction.dot(q) / det;
                let depth = ac.dot(q) / det;
                ((0.0..=1.0).contains(&u)
                    && v >= 0.0
                    && u + v <= 1.0
                    && (0.0..=limit).contains(&depth))
                .then_some(depth)
            })
            .min_by(f32::total_cmp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone(kind: ZoneKind, density: Density) -> ZoneCell {
        ZoneCell { kind, density }
    }

    #[test]
    fn zones2_z1_seed_fixtures_include_negative_decimeters() {
        for (center, cell, expected) in [
            (
                Vec2::ZERO,
                zone(ZoneKind::Residential, Density::Low),
                GrowableForm::BroadBase,
            ),
            (
                vec2(-18.2, 45.6),
                zone(ZoneKind::MixedUse, Density::High),
                GrowableForm::BroadBase,
            ),
            (
                vec2(170.5, -25.1),
                zone(ZoneKind::Industrial, Density::Medium),
                GrowableForm::FlatCap,
            ),
        ] {
            assert_eq!(GrowableForm::seeded(center, cell), expected);
            assert_eq!(
                GrowableForm::seeded(center + vec2(0.01, -0.01), cell),
                expected
            );
        }
    }

    #[test]
    fn zones2_z1_forms_and_value_stay_distinct_on_all_stock_parcel_sizes() {
        for size in [20.0, 30.0, 40.0] {
            for (density, style) in [
                (Density::Low, GrowableMassingStyle::House),
                (Density::Medium, GrowableMassingStyle::Block),
                (Density::High, GrowableMassingStyle::Tower),
            ] {
                let cell = zone(ZoneKind::Residential, density);
                let mut dimensions = [None; 3];
                for x in 100..108 {
                    let parcel = OBB::new(vec2(x as f32, 100.0), Vec2::Y, size, size);
                    let form = GrowableFormFit::check_grade(parcel, cell, style, 0, |_| Some(0.0))
                        .unwrap();
                    let better =
                        GrowableFormFit::check_grade(parcel, cell, style, 2, |_| Some(0.0))
                            .unwrap();
                    assert_eq!(form.form(), better.form());
                    let dims = form.shaft().axis().map(Vec2::mag);
                    let bigger = better.shaft().axis().map(Vec2::mag);
                    assert!(bigger[0] > dims[0] && bigger[1] > dims[1]);
                    assert!(better
                        .footprint()
                        .corners
                        .into_iter()
                        .all(|p| parcel.expand(-0.99).contains(p)));
                    if density == Density::High {
                        assert!(dims.into_iter().all(|axis| axis >= 8.999));
                    }
                    dimensions[form.form().index()] = Some(dims.map(|v| (10.0 * v).round() as u16));
                }
                let [a, b, c] = dimensions.map(Option::unwrap);
                assert!(a != b && a != c && b != c, "{size} m {density:?}");
            }
        }
    }

    #[test]
    fn zones2_z1_grade_retries_forms_and_samples_bilinear_peaks() {
        let shape = OBB::new(vec2(101.0, 100.0), Vec2::Y, 30.0, 30.0);
        let zone = zone(ZoneKind::Residential, Density::High);
        assert_eq!(
            GrowableForm::seeded(shape.center(), zone),
            GrowableForm::Stepped
        );
        let slope = |grade: f32| {
            GrowableFormFit::check_grade(shape, zone, GrowableMassingStyle::Tower, 0, |p| {
                Some((p.y - 85.0) * grade)
            })
        };
        let fit = slope(0.063).unwrap();
        assert_eq!(fit.form(), GrowableForm::BroadBase);
        assert!(fit
            .footprint()
            .corners
            .iter()
            .all(|p| fit.base_z() >= (p.y - 85.0) * 0.063));
        assert!(slope(0.20).is_none());

        // Along the diagonal of one terrain cell, the bilinear surface
        // reaches its maximum at t = 0.75, away from the ends and midpoint.
        let (_, peak) = segment_grade(Vec2::ZERO, vec2(12.0, 12.0), &|p| {
            let v = p / 12.0;
            Some(6.0 * v.x + 6.0 * v.y - 8.0 * v.x * v.y)
        })
        .unwrap();
        assert_eq!(peak, 4.5);
    }

    #[test]
    fn zones2_z1_fix_connector_rejects_bilinear_ridge() {
        let mut env = Environment::new(1, 1);
        let center = vec2(112.6, 126.0);
        let parcel = OBB::new(center, Vec2::Y, 40.0, 40.0);
        env.terrain_apply(parcel.bbox().expand(2.0 * CELL_SIZE), |p| {
            if p.xy() == vec2(112.0, 112.0) {
                3.0
            } else {
                0.0
            }
        });
        assert!(env.true_height(vec2(112.0, 112.0)).unwrap() > 2.98);
        let fit = GrowableFormFit::check(
            &env,
            parcel,
            zone(ZoneKind::Residential, Density::High),
            GrowableMassingStyle::Tower,
            0,
        );
        assert!(
            fit.is_none(),
            "the connector crosses the ridge for all three forms: {fit:?}"
        );
    }

    #[test]
    fn zones2_z1_fix_one_floor_industrial_panels_stay_on_the_ground_facade() {
        let mut env = Environment::new(1, 1);
        let parcel = OBB::new(vec2(112.0, 126.0), Vec2::Y, 30.0, 30.0);
        env.terrain_apply(parcel.bbox().expand(2.0 * CELL_SIZE), |_| 0.0);
        let cell = zone(ZoneKind::Industrial, Density::Low);
        let style = GrowableMassingStyle::Industrial;
        let mut fit = GrowableFormFit::check(&env, parcel, cell, style, 0).unwrap();
        fit.form = GrowableForm::Stepped;
        let (mesh, _) = fit.freeze(&env, style).unwrap().mesh(cell, 1, 1).unwrap();
        let facade_y = fit.door().y;
        let panels = mesh.faces.iter().filter(|(vertices, _)| {
            vertices
                .iter()
                .all(|p| (p.y - (facade_y - 0.025)).abs() < 0.001)
        });
        let mut count = 0;
        for (vertices, _) in panels {
            count += 1;
            assert!(
                vertices.iter().all(|p| p.z <= fit.base_z() + 2.5),
                "panel extends above the first setback: {vertices:?}"
            );
        }
        assert_eq!(count, 4);
    }

    #[test]
    fn zones2_z1_capacity_arithmetic_rounds_and_caps() {
        assert_eq!(housing_slots(5, 6, 4, false), Some(8));
        assert_eq!(housing_slots(4, 5, 4, true), Some(5));
        assert_eq!(housing_slots(5, 1, 1, true), None);
        assert_eq!(
            housing_slots(MAX_PROFILED_SLOTS, 28, 12, false),
            Some(MAX_PROFILED_SLOTS)
        );
        assert_eq!(job_slots(ZoneKind::Commercial, 12, 28, 4), Some(24));
        assert_eq!(job_slots(ZoneKind::Office, 9, 28, 4), Some(22));
        assert_eq!(job_slots(ZoneKind::MixedUse, 12, 28, 4), Some(12));
    }
}
