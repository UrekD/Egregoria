use engine::{MeshBuilder, MeshVertex};
use geom::{LinearColor, Vec3};
use prototypes::{GROWABLE_FLOOR_HEIGHT_M, MIXED_USE_SHOP_FLOOR_HEIGHT_M};
use simulation::map::{
    Building, Density, GrowableBusinessState, GrowableDevelopment, GrowableSiteState, ZoneKind,
};
use std::ops::Range;

const MAX_VERTICES: usize = 1_536;
const MAX_INDICES: usize = 2_304;

/// Append a prepared world-space shell to the existing static building batch.
pub(super) fn append(builder: &mut MeshBuilder<false>, building: &Building) -> bool {
    let Some(site) = building.development() else {
        return false;
    };
    let mut emitted = false;
    builder.extend_with(None, |vertices, add_index| {
        let mut offset = 0;
        emitted = emit_shell(
            &building.mesh.faces,
            site,
            building.door_pos,
            building.height,
            |face, normal, color| {
                vertices.extend(face.iter().map(|&position| MeshVertex {
                    position: position.into(),
                    normal,
                    uv: [0.0; 2],
                    color: color.into(),
                    tangent: [0.0; 4],
                }));
                for i in 1..face.len() as u32 - 1 {
                    add_index(offset);
                    add_index(offset + i);
                    add_index(offset + i + 1);
                }
                offset += face.len() as u32;
            },
        );
    });
    emitted
}

fn emit_shell(
    faces: &[(Vec<Vec3>, LinearColor)],
    site: &GrowableDevelopment,
    door: Vec3,
    base: f32,
    mut emit: impl FnMut(&[Vec3], Vec3, LinearColor),
) -> bool {
    // Reserve the full shell before spending any vertices on facade details.
    let mut vertices = 0;
    let mut indices = 0;
    for (face, _) in faces {
        if !(3..=MAX_VERTICES).contains(&face.len()) {
            return false;
        }
        vertices += face.len();
        indices += 3 * (face.len() - 2);
        if vertices > MAX_VERTICES || indices > MAX_INDICES || !face.iter().all(|p| p.is_finite()) {
            return false;
        }
    }
    let mut panels = ((MAX_VERTICES - vertices) / 4).min((MAX_INDICES - indices) / 6);
    let abandoned = matches!(site.site, GrowableSiteState::Abandoned { .. });
    let mut emitted = false;
    for (face, color) in faces {
        let Some(normal) = face[1..].windows(2).find_map(|p| {
            (p[0] - face[0])
                .cross(p[1] - face[0])
                .try_normalize()
                .filter(|normal| normal.is_finite() && normal.mag2() > 0.0)
        }) else {
            continue;
        };
        emitted = true;
        emit(face, normal, if abandoned { 0.34 * *color } else { *color });
        let Some(wall) = Facade::from_face(face, normal, base) else {
            continue;
        };
        wall.details(site, door, base, abandoned, |panel, color| {
            if panels > 0 {
                panels -= 1;
                emit(&panel, normal, color);
            }
        });
    }
    emitted
}

struct Facade {
    first: Vec3,
    last: Vec3,
    top: f32,
    normal: Vec3,
}

impl Facade {
    fn from_face(face: &[Vec3], normal: Vec3, base: f32) -> Option<Self> {
        let &[first, last, upper_right, upper_left] = face else {
            return None;
        };
        // The prepared exterior wall quads start at the lower edge. Foundation
        // skirts, roof panels and sloped connectors need no facade detail.
        (first.z >= base - 0.01
            && first.z == last.z
            && upper_left.z == upper_right.z
            && first.xy() == upper_left.xy()
            && last.xy() == upper_right.xy()
            && upper_left.z - first.z >= 0.5)
            .then_some(Self {
                first,
                last,
                top: upper_left.z,
                normal,
            })
    }

    fn details(
        &self,
        site: &GrowableDevelopment,
        door: Vec3,
        base: f32,
        abandoned: bool,
        mut emit: impl FnMut([Vec3; 4], LinearColor),
    ) {
        let mixed = site.zone.kind == ZoneKind::MixedUse;
        let shop_closed = matches!(
            site.business,
            Some(GrowableBusinessState::ClosedUntil { .. })
        );
        let front =
            (door - self.first).dot(self.normal).abs() < 0.02 && self.first.z <= base + 0.01;
        if front && (abandoned || mixed && shop_closed) {
            let half_width = if mixed {
                0.46
            } else {
                (1.0 / self.first.distance(self.last)).min(0.42)
            };
            let span = (0.5 - half_width)..(0.5 + half_width);
            let top = if mixed { 3.05 } else { 2.35 };
            let shutter = LinearColor::new(0.17, 0.18, 0.18, 1.0);
            let mut bottom = 0.0;
            for height in [0.5, 1.1, 1.7, 2.3] {
                self.panel(
                    span.clone(),
                    (base + bottom)..(base + height),
                    shutter,
                    &mut emit,
                );
                bottom = height + 0.06;
                self.panel(
                    span.clone(),
                    (base + height)..(base + bottom),
                    1.7 * shutter,
                    &mut emit,
                );
            }
            self.panel(span, (base + bottom)..(base + top), shutter, &mut emit);
        }
        if mixed {
            let fascia = if abandoned {
                LinearColor::gray(0.12)
            } else {
                LinearColor::new(0.08, 0.49, 0.44, 1.0)
            };
            self.panel(0.0..1.0, (base + 3.58)..(base + 4.24), fascia, &mut emit);
        }
        if site.zone.density == Density::Low {
            return;
        }
        let first_z = if mixed {
            base + MIXED_USE_SHOP_FLOOR_HEIGHT_M
        } else {
            base
        };
        let glass = match site.zone.kind {
            ZoneKind::Residential | ZoneKind::MixedUse => LinearColor::new(0.16, 0.28, 0.28, 1.0),
            ZoneKind::Commercial | ZoneKind::Office => LinearColor::new(0.14, 0.26, 0.37, 1.0),
            ZoneKind::Industrial => LinearColor::new(0.33, 0.34, 0.33, 1.0),
        };
        let glass = if abandoned { 0.45 * glass } else { glass };
        let floor_at = |z: f32| ((z - first_z) / GROWABLE_FLOOR_HEIGHT_M).floor() as u8;
        let end = floor_at(self.top)
            .saturating_add(1)
            .min(site.floors.min(28).saturating_sub(u8::from(mixed)));
        for floor in floor_at(self.first.z)..end {
            let z = first_z + f32::from(floor) * GROWABLE_FLOOR_HEIGHT_M;
            self.panel(0.06..0.94, (z + 0.87)..(z + 2.45), glass, &mut emit);
        }
    }

    fn panel(
        &self,
        span: Range<f32>,
        height: Range<f32>,
        color: LinearColor,
        emit: &mut impl FnMut([Vec3; 4], LinearColor),
    ) {
        let bottom = height.start.max(self.first.z);
        let top = height.end.min(self.top);
        if top - bottom < 0.035 {
            return;
        }
        let at = |across, z| self.first.lerp(self.last, across).xy().z(z) + self.normal * 0.05;
        emit(
            [
                at(span.start, bottom),
                at(span.end, bottom),
                at(span.end, top),
                at(span.start, top),
            ],
            color,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::vec3;
    use simulation::map::ZoneCell;

    fn site(kind: ZoneKind, floors: u8) -> GrowableDevelopment {
        GrowableDevelopment {
            zone: ZoneCell {
                kind,
                density: Density::High,
            },
            level: 1,
            floors,
            roof_z: f32::from(floors) * GROWABLE_FLOOR_HEIGHT_M + 0.6,
            approved_value_band: 0,
            home: None,
            planned_jobs: 12,
            site: GrowableSiteState::Active,
            business: Some(GrowableBusinessState::Open),
        }
    }

    #[test]
    fn zones2_native_tower_mesh_is_bounded_and_banded() {
        let mut faces = Vec::new();
        let base = [
            vec3(0.0, 0.0, 0.0),
            vec3(12.0, 0.0, 0.0),
            vec3(12.0, 12.0, 0.0),
            vec3(0.0, 12.0, 0.0),
        ];
        let site = site(ZoneKind::Office, 28);
        let roof = base.map(|p| p.up(site.roof_z));
        for i in 0..4 {
            let j = (i + 1) % 4;
            faces.push((vec![base[i], base[j], roof[j], roof[i]], LinearColor::WHITE));
        }
        faces.push((roof.to_vec(), LinearColor::WHITE));
        let mut vertices = 0;
        let mut indices = 0;
        let mut highest_window = 0.0_f32;
        assert!(emit_shell(
            &faces,
            &site,
            base[0],
            0.0,
            |face, normal, color| {
                vertices += face.len();
                indices += 3 * (face.len() - 2);
                assert!((normal.mag2() - 1.0).abs() < 1e-5);
                assert!(face.iter().all(|v| v.is_finite()));
                if color.r != 1.0 {
                    highest_window = highest_window.max(face[2].z);
                }
            }
        ));
        assert_eq!(vertices, 20 + 28 * 4 * 4);
        assert!(vertices <= 1536);
        assert!(indices <= 2304);
        assert!(highest_window > 88.0);

        let over_budget = vec![faces[0].clone(); 385];
        assert!(!emit_shell(&over_budget, &site, base[0], 0.0, |_, _, _| {
            panic!("oversized shell emitted geometry")
        }));
    }

    #[test]
    fn zones2_native_closed_shop_shutters_only_the_ground_floor() {
        let faces = [(
            vec![
                Vec3::ZERO,
                Vec3::x(12.0),
                vec3(12.0, 0.0, 15.0),
                Vec3::z(15.0),
            ],
            LinearColor::WHITE,
        )];
        let mut site = site(ZoneKind::MixedUse, 4);
        let draw = |site: &GrowableDevelopment| {
            let mut faces_out = Vec::new();
            assert!(emit_shell(
                &faces,
                site,
                Vec3::x(6.0),
                0.0,
                |face, _, color| {
                    faces_out.push((face[0].z, <[f32; 4]>::from(color)));
                }
            ));
            faces_out
        };
        let open = draw(&site);
        site.business = Some(GrowableBusinessState::ClosedUntil {
            retry_elapsed_tick: 1,
        });
        let closed = draw(&site);
        assert!(closed.len() > open.len());
        assert_eq!(open[0], closed[0]);
        assert!(open.iter().any(|(z, _)| *z > MIXED_USE_SHOP_FLOOR_HEIGHT_M));
        assert!(open.iter().all(|face| closed.contains(face)));
        site.site = GrowableSiteState::Abandoned {
            since_elapsed_tick: 1,
        };
        let abandoned = draw(&site);
        assert!(abandoned[0].1[0] < closed[0].1[0]);
    }
}
