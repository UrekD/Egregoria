use crate::gui::selectable::{face_fan, PreparedShell};
use engine::{MeshBuilder, MeshVertex};
use geom::{LinearColor, Vec3};
use prototypes::{GROWABLE_FLOOR_HEIGHT_M, MIXED_USE_SHOP_FLOOR_HEIGHT_M};
use simulation::map::{
    Density, GrowableBusinessState, GrowableDevelopment, GrowableSiteState, ZoneKind,
};
use std::ops::Range;

/// Append a validated world-space shell to the existing static building batch.
pub(super) fn append(
    builder: &mut MeshBuilder<false>,
    shell: &PreparedShell<'_>,
    site: &GrowableDevelopment,
) {
    builder.extend_with(None, |vertices, add_index| {
        let mut offset = 0;
        emit_shell(shell, site, |face, normal, color| {
            vertices.extend(face.iter().map(|&position| MeshVertex {
                position: position.into(),
                normal,
                uv: [0.0; 2],
                color: color.into(),
                tangent: [0.0; 4],
            }));
            for triangle in face_fan(face.len()) {
                for index in triangle {
                    add_index(offset + index as u32);
                }
            }
            offset += face.len() as u32;
        });
    });
}

fn emit_shell(
    shell: &PreparedShell<'_>,
    site: &GrowableDevelopment,
    mut emit: impl FnMut(&[Vec3], Vec3, LinearColor),
) {
    // The full shell has already reserved vertices and indices.
    let mut panels = shell.panel_budget;
    let abandoned = matches!(site.site, GrowableSiteState::Abandoned { .. });
    for (face, normal, color) in shell.faces() {
        emit(face, normal, if abandoned { 0.34 * color } else { color });
        let Some(wall) = Facade::from_face(face, normal, shell.base_z(), shell.door()) else {
            continue;
        };
        wall.details(site, shell.base_z(), abandoned, |panel, color| {
            if panels > 0 {
                panels -= 1;
                emit(&panel, normal, color);
            }
        });
    }
}

struct Facade {
    first: Vec3,
    last: Vec3,
    top: f32,
    normal: Vec3,
    entrance: Option<f32>,
}

impl Facade {
    fn from_face(face: &[Vec3], normal: Vec3, base: f32, door: Vec3) -> Option<Self> {
        let &[first, last, upper_right, upper_left] = face else {
            return None;
        };
        const PLANE_TOLERANCE_M: f32 = 0.011;
        const ENTRY_PANEL_OFFSET_M: f32 = 0.025;
        let plane = (first - door).dot(normal);
        // The prepared entry and storefront panels sit proud of the street wall.
        // Detail only the underlying wall quads, excluding those panels and the
        // foundation skirts, roof panels and sloped connectors.
        if first.z < base - PLANE_TOLERANCE_M
            || first.z != last.z
            || upper_left.z != upper_right.z
            || first.xy() != upper_left.xy()
            || last.xy() != upper_right.xy()
            || upper_left.z - first.z < 0.5
            || ((plane - ENTRY_PANEL_OFFSET_M).abs() <= PLANE_TOLERANCE_M
                && upper_left.z <= base + 3.41)
        {
            return None;
        }
        let along = last - first;
        let door_fraction = (door - first).dot(along) / along.mag2();
        let entrance = (plane.abs() <= PLANE_TOLERANCE_M
            && first.z <= base + PLANE_TOLERANCE_M
            && (0.0..=1.0).contains(&door_fraction))
        .then_some(door_fraction);
        Some(Self {
            first,
            last,
            top: upper_left.z,
            normal,
            entrance,
        })
    }

    fn details(
        &self,
        site: &GrowableDevelopment,
        base: f32,
        abandoned: bool,
        mut emit: impl FnMut([Vec3; 4], LinearColor),
    ) {
        let mixed = site.zone.kind == ZoneKind::MixedUse;
        let shop_closed = matches!(
            site.business,
            Some(GrowableBusinessState::ClosedUntil { .. })
        );
        let half_door = 1.0 / self.first.distance(self.last);
        if let Some(entrance) = self.entrance.filter(|_| abandoned || mixed && shop_closed) {
            let half_width = if mixed { 0.46 } else { half_door.min(0.42) };
            let span = (entrance - half_width).max(0.0)..(entrance + half_width).min(1.0);
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
            let height = (z + 0.87)..(z + 2.45);
            if let Some(entrance) = self.entrance.filter(|_| floor == 0 && !mixed) {
                if site.zone.kind == ZoneKind::Industrial {
                    // Z1 already prepares panes alongside the industrial entry.
                    continue;
                }
                let clearance = 1.05 * half_door;
                self.panel(
                    0.06..(entrance - clearance).min(0.94),
                    height.clone(),
                    glass,
                    &mut emit,
                );
                self.panel(
                    (entrance + clearance).max(0.06)..0.94,
                    height,
                    glass,
                    &mut emit,
                );
            } else {
                self.panel(0.06..0.94, height, glass, &mut emit);
            }
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
        if span.end <= span.start || top - bottom < 0.035 {
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
    use crate::gui::selectable::tests::profiled_house;
    use geom::{vec3, Vec2, AABB, OBB};
    use simulation::map::{procgen::ColoredMesh, Building, ZoneCell};

    fn building(faces: Vec<(Vec<Vec3>, LinearColor)>, door: Vec3, roof: f32) -> Building {
        let footprint = OBB::from(&AABB::new_ll_ur(Vec2::ZERO, Vec2::splat(12.0)));
        let mut building = profiled_house(
            Default::default(),
            footprint,
            ColoredMesh { faces },
            0.0,
            roof,
        );
        building.door_pos = door;
        building
    }

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
            frozen_shell: None,
            home: None,
            planned_jobs: 12,
            site: GrowableSiteState::Active,
            business: Some(GrowableBusinessState::Open),
        }
    }

    #[test]
    fn zones2_native_render_hook_consumes_rejected_profiled_houses() {
        let footprint = OBB::from(&AABB::new_ll_ur(Vec2::ZERO, Vec2::splat(12.0)));
        let valid = (
            footprint.corners.map(|p| p.z(30.0)).to_vec(),
            LinearColor::WHITE,
        );
        let cases = [
            vec![(vec![Vec3::ZERO], LinearColor::WHITE)],
            vec![valid.clone(); 385],
        ];
        for faces in cases {
            let building = building(faces, Vec3::x(6.0), 30.0);
            let mut mesh = MeshBuilder::<false>::new_without_mat();
            assert!(super::super::append_profiled_building(&mut mesh, &building));
            assert!(mesh.mk_tess().vertices.is_empty());
            assert!(mesh.mk_tess().indices.is_empty());
        }
    }

    #[test]
    fn zones2_native_frontage_keeps_the_door_and_shutter_clear_of_glazing() {
        let wall = vec![
            Vec3::ZERO,
            Vec3::x(12.0),
            vec3(12.0, 0.0, 6.4),
            Vec3::z(6.4),
        ];
        let door = vec![
            vec3(5.0, -0.025, 0.0),
            vec3(7.0, -0.025, 0.0),
            vec3(7.0, -0.025, 2.35),
            vec3(5.0, -0.025, 2.35),
        ];
        let building = building(
            vec![(wall, LinearColor::WHITE), (door, LinearColor::gray(0.22))],
            Vec3::x(6.0),
            6.4,
        );
        let shell = PreparedShell::new(&building).unwrap();
        let mut site = site(ZoneKind::Commercial, 2);
        let collect_glazing = |site: &GrowableDevelopment| {
            let mut glazing = Vec::new();
            emit_shell(&shell, site, |face, _, color| {
                let is_glass = color.g == 0.26 || color.g == (0.45 * 0.26);
                if is_glass && face[0].z < GROWABLE_FLOOR_HEIGHT_M {
                    glazing.push((face[0].x, face[1].x, face[0].y));
                }
            });
            glazing
        };
        for abandoned in [false, true] {
            if abandoned {
                site.site = GrowableSiteState::Abandoned {
                    since_elapsed_tick: 1,
                };
            }
            let glazing = collect_glazing(&site);
            assert_eq!(glazing.len(), 2);
            assert!(glazing
                .iter()
                .all(|&(lo, hi, y)| (hi <= 5.0 || lo >= 7.0) && y == -0.05));
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
        let building = building(faces, Vec3::x(6.0), site.roof_z);
        let shell = PreparedShell::new(&building).unwrap();
        emit_shell(&shell, &site, |face, normal, color| {
            vertices += face.len();
            indices += 3 * (face.len() - 2);
            assert!((normal.mag2() - 1.0).abs() < 1e-5);
            assert!(face.iter().all(|v| v.is_finite()));
            if color.r != 1.0 {
                highest_window = highest_window.max(face[2].z);
            }
        });
        assert_eq!(vertices, 20 + (28 * 4 + 1) * 4);
        assert!(vertices <= 1536);
        assert!(indices <= 2304);
        assert!(highest_window > 88.0);
    }

    #[test]
    fn zones2_native_closed_shop_shutters_only_the_ground_floor() {
        let faces = vec![(
            vec![
                Vec3::ZERO,
                Vec3::x(12.0),
                vec3(12.0, 0.0, 15.0),
                Vec3::z(15.0),
            ],
            LinearColor::WHITE,
        )];
        let building = building(faces, Vec3::x(6.0), 15.0);
        let shell = PreparedShell::new(&building).unwrap();
        let mut site = site(ZoneKind::MixedUse, 4);
        let draw = |site: &GrowableDevelopment| {
            let mut faces_out = Vec::new();
            emit_shell(&shell, site, |face, _, color| {
                faces_out.push((face[0].z, <[f32; 4]>::from(color)));
            });
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
