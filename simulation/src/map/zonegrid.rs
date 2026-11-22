use geom::{vec2, Shape, Vec2, AABB, OBB};
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::BTreeMap;

pub use prototypes::{Density, ZoneKind};

pub const ZONE_CELL_SIZE: f32 = 8.0;
const MAX_PAINT_CELLS: usize = 2_500;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZoneCell {
    pub kind: ZoneKind,
    pub density: Density,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct ZoneGrid {
    pub cells: BTreeMap<(i32, i32), ZoneCell>,
    /// Changes once for each paint or unzone that changes the grid.
    pub generation: u64,
}

impl ZoneGrid {
    pub const MAX_PAINT_SIDE: f32 = 400.0;

    pub(crate) fn valid_paint(area: &OBB, bounds: AABB) -> bool {
        area.corners.iter().all(|&corner| bounds.contains(corner))
            && paint_cells(area).is_some_and(|cells| cells.count() <= MAX_PAINT_CELLS)
    }

    pub fn paint(&mut self, area: OBB, kind: Option<ZoneKind>, density: Density) {
        let Some(cells) = paint_cells(&area) else {
            return;
        };
        let cells: Vec<_> = cells.collect();
        if cells.len() > MAX_PAINT_CELLS {
            return;
        }

        let mut changed = false;
        for key in cells {
            changed |= match kind {
                Some(kind) => {
                    let cell = ZoneCell { kind, density };
                    self.cells.insert(key, cell) != Some(cell)
                }
                None => self.cells.remove(&key).is_some(),
            };
        }
        if changed {
            self.generation = self.generation.wrapping_add(1);
        }
    }

    pub fn lot_zone(&self, area: &OBB) -> Option<ZoneCell> {
        if self.cells.is_empty() {
            return None;
        }
        lot_vote(area, |key| self.cells.get(&key).copied())
    }

    /// Vote as if the already clipped paint rectangles had been applied. The first return
    /// value says whether the stroke touched any of the lot's storage cell centres.
    pub fn lot_zone_after_paint(
        &self,
        lot: &OBB,
        paints: &[OBB],
        replacement: Option<ZoneCell>,
    ) -> (bool, Option<ZoneCell>) {
        let mut touched = false;
        let vote = lot_vote(lot, |key| {
            if paints.iter().any(|area| area.contains(cell_center(key))) {
                touched = true;
                replacement
            } else {
                self.cells.get(&key).copied()
            }
        });
        (touched, vote)
    }
}

fn lot_vote(
    area: &OBB,
    mut cell_at: impl FnMut((i32, i32)) -> Option<ZoneCell>,
) -> Option<ZoneCell> {
    const KINDS: [ZoneKind; 4] = [
        ZoneKind::Residential,
        ZoneKind::Commercial,
        ZoneKind::Industrial,
        ZoneKind::Office,
    ];
    let mut counts = [[0usize; 3]; KINDS.len()];
    let mut total = 0;
    let mut zoned = 0;
    for key in cells_under(area) {
        total += 1;
        if let Some(cell) = cell_at(key) {
            counts[cell.kind as usize][usize::from(cell.density.ordinal())] += 1;
            zoned += 1;
        }
    }
    if zoned == 0 || 2 * zoned < total {
        return None;
    }

    let (kind, densities) = KINDS
        .into_iter()
        .zip(counts)
        .max_by_key(|(kind, counts)| (counts.iter().sum::<usize>(), Reverse(*kind)))?;
    let density = [Density::Low, Density::Medium, Density::High]
        .into_iter()
        .max_by_key(|density| {
            let ordinal = density.ordinal();
            (densities[usize::from(ordinal)], Reverse(ordinal))
        })?;
    Some(ZoneCell { kind, density })
}

fn cell_center((x, y): (i32, i32)) -> Vec2 {
    (vec2(x as f32, y as f32) + Vec2::splat(0.5)) * ZONE_CELL_SIZE
}

fn paint_cells(area: &OBB) -> Option<impl Iterator<Item = (i32, i32)> + '_> {
    let [a, b] = area.axis();
    let axis_lengths = a.mag() * b.mag();
    // Allow rounding in rotated rectangles stored as world-space f32 coordinates.
    let corner_scale = area.corners.iter().fold(1.0_f32, |scale, corner| {
        scale.max(corner.x.abs()).max(corner.y.abs())
    });
    let corner_tolerance = 4.0 * f32::EPSILON * corner_scale;
    let valid_rectangle = axis_lengths > 0.0
        && a.dot(b).abs() <= 0.001 * axis_lengths
        && (area.corners[2] - area.corners[0]).is_close(a + b, corner_tolerance);
    let valid_size = area.segments().iter().all(|side| {
        let length_squared = (side.dst - side.src).mag2();
        length_squared <= ZoneGrid::MAX_PAINT_SIDE * ZoneGrid::MAX_PAINT_SIDE
    });
    (valid_rectangle && valid_size).then(|| cells_under(area).take(MAX_PAINT_CELLS + 1))
}

fn cells_under(area: &OBB) -> impl Iterator<Item = (i32, i32)> + '_ {
    let bbox = area.bbox();
    let index = |coord: f32| (coord / ZONE_CELL_SIZE).floor() as i32;
    let xs = index(bbox.ll.x)..=index(bbox.ur.x);
    let ys = index(bbox.ll.y)..=index(bbox.ur.y);
    xs.flat_map(move |x| ys.clone().map(move |y| (x, y)))
        .filter(|&key| area.contains(cell_center(key)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::economy::Government;
    use crate::map::Environment;
    use crate::tests::TestCtx;
    use crate::world_command::{CommandOutcomes, Rejection, WorldCommand};
    use geom::{vec3, Vec2};

    #[test]
    fn p1a_zone_paint_validation() {
        let mut ctx = TestCtx::new_fast();
        ctx.g.map_mut().environment = Environment::new(2, 2);
        let original_money = ctx.g.read::<Government>().money;
        let mut paint = |area| {
            ctx.apply(&[WorldCommand::ZonePaint {
                area,
                kind: Some(ZoneKind::Industrial),
                density: Density::High,
            }]);
            (
                ctx.g.read::<CommandOutcomes>().last.clone(),
                ctx.g.read::<ZoneGrid>().cells.len(),
            )
        };

        let rejected = vec![("ZonePaint", Rejection::InvalidZone)];
        for area in [
            OBB::new_corners([
                vec2(8.0, 8.0),
                vec2(40.0, 8.0),
                vec2(56.0, 40.0),
                vec2(24.0, 40.0),
            ]),
            OBB::new(vec2(12.0, 12.0), Vec2::X, 0.0, 8.0),
            OBB::new_corners([
                vec2(8.0, 8.0),
                vec2(40.0, 8.0),
                vec2(44.0, 40.0),
                vec2(8.0, 40.0),
            ]),
            OBB::new(vec2(f32::NAN, 252.0), Vec2::X, 8.0, 8.0),
            OBB::new(Vec2::splat(252.0), Vec2::X, f32::MAX, f32::MAX),
            OBB::new(Vec2::splat(252.0), Vec2::X, 401.0, 8.0),
            OBB::new(vec2(2.0, 252.0), Vec2::X, 8.0, 8.0),
            // The rotated square fits the dimensions but covers 2,521 centres.
            OBB::new(
                Vec2::splat(300.0),
                Vec2::splat(std::f32::consts::FRAC_1_SQRT_2),
                399.0,
                399.0,
            ),
        ] {
            assert_eq!(paint(area), (rejected.clone(), 0), "{area:?}");
        }
        assert_eq!(
            paint(OBB::new(Vec2::splat(256.0), Vec2::X, 400.0, 400.0)),
            (vec![], 2_500)
        );
        assert_eq!(ctx.g.read::<Government>().money, original_money);
    }

    #[test]
    fn p1a_zoning_and_demand() {
        let mut ctx = TestCtx::new_fast();
        let street = [vec3(100.0, 100.0, 0.0), vec3(300.0, 100.0, 0.0)];
        ctx.build_roads(&street);
        let (road, lot, shape) = {
            let map = ctx.g.map();
            let lot = map.lots().values().next().unwrap();
            (lot.parent, lot.id, lot.shape)
        };

        let expected = Some(ZoneCell {
            kind: ZoneKind::Residential,
            density: Density::High,
        });
        ctx.apply(&[WorldCommand::ZonePaint {
            area: shape,
            kind: Some(ZoneKind::Residential),
            density: Density::High,
        }]);
        let painted = ctx.g.read::<ZoneGrid>().cells.clone();
        assert_eq!(ctx.g.read::<ZoneGrid>().lot_zone(&shape), expected);

        ctx.apply(&[WorldCommand::MapRemoveRoad(road)]);
        assert!(ctx.g.map().get(lot).is_none());
        assert_eq!(ctx.g.read::<ZoneGrid>().cells, painted);
        ctx.build_roads(&street);
        let rebuilt = ctx
            .g
            .map()
            .lots()
            .values()
            .find(|lot| lot.shape.center() == shape.center())
            .expect("rebuilding the road should regenerate the lot")
            .id;
        assert_ne!(lot, rebuilt);
        let rebuilt_shape = ctx.g.map().get(rebuilt).unwrap().shape;
        assert_eq!(ctx.g.read::<ZoneGrid>().lot_zone(&rebuilt_shape), expected);
    }

    #[test]
    fn p1a_zone_cell_majorities_and_ties() {
        use Density::{High, Low};
        use ZoneKind::{Commercial, Industrial, Office, Residential};

        let mut grid = ZoneGrid::default();
        let lot = OBB::new(vec2(0.0, 4.0), Vec2::X, 64.0, 8.0);
        let cell = |x| OBB::new(vec2(x, 4.0), Vec2::X, 8.0, 8.0);
        for (x, kind, density) in [
            (-28.0, Office, High),
            (-20.0, Industrial, High),
            (-12.0, Commercial, High),
            (-4.0, Residential, Low),
        ] {
            grid.paint(cell(x), Some(kind), density);
        }
        assert!(grid.cells.contains_key(&(-1, 0)));
        assert_eq!(
            grid.lot_zone(&lot),
            Some(ZoneCell {
                kind: Residential,
                density: Low
            }),
        );

        grid.paint(cell(4.0), Some(Commercial), Low);
        assert_eq!(
            grid.lot_zone(&lot),
            Some(ZoneCell {
                kind: Commercial,
                density: Low
            }),
        );
        grid.paint(cell(12.0), Some(Commercial), High);
        assert_eq!(
            grid.lot_zone(&lot),
            Some(ZoneCell {
                kind: Commercial,
                density: High
            }),
        );
        grid.paint(cell(-20.0), None, Low);
        grid.paint(cell(-28.0), None, Low);
        grid.paint(cell(-4.0), None, Low);
        assert_eq!(grid.lot_zone(&lot), None);

        let misses_centres = OBB::new(vec2(-8.0, 4.0), Vec2::X, 7.0, 8.0);
        assert_eq!(grid.lot_zone(&misses_centres), None);
    }

    #[test]
    fn u1_zone_prospective_vote_matches_applied_stamps_and_erase() {
        let mut grid = ZoneGrid::default();
        let lot = OBB::new(Vec2::splat(24.0), Vec2::X, 40.0, 40.0);
        let stamps = [12.0, 28.0, 36.0].map(|x| OBB::new(vec2(x, 20.0), Vec2::X, 16.0, 24.0));
        let zone = ZoneCell {
            kind: ZoneKind::Office,
            density: Density::High,
        };
        assert_eq!(
            grid.lot_zone_after_paint(&lot, &stamps[..1], Some(zone)),
            (true, None)
        );
        assert_eq!(
            grid.lot_zone_after_paint(&lot, &stamps, Some(zone)),
            (true, Some(zone))
        );
        for stamp in stamps {
            grid.paint(stamp, Some(zone.kind), zone.density);
        }
        assert_eq!(grid.lot_zone(&lot), Some(zone));
        assert_eq!(
            grid.lot_zone_after_paint(&lot, &stamps[..2], None),
            (true, None)
        );
        for stamp in &stamps[..2] {
            grid.paint(*stamp, None, zone.density);
        }
        assert_eq!(grid.lot_zone(&lot), None);
    }

    #[test]
    fn u1_zone_density_majority_and_ordinal_ties() {
        use Density::{High, Low, Medium};

        let kind = ZoneKind::Residential;
        let lot = OBB::new(vec2(20.0, 4.0), Vec2::X, 40.0, 8.0);
        for (votes, expected) in [
            ([Medium, High, Medium, Low, Medium], Medium),
            ([Medium, High, Medium, Low, High], Medium),
            ([Low, High, Medium, Low, High], Low),
        ] {
            let mut grid = ZoneGrid::default();
            for (x, density) in votes.into_iter().enumerate() {
                grid.cells.insert((x as i32, 0), ZoneCell { kind, density });
            }
            assert_eq!(
                grid.lot_zone(&lot),
                Some(ZoneCell {
                    kind,
                    density: expected
                })
            );
        }
    }

    #[test]
    fn u1_zone_paint_and_erase_at_map_edge() {
        let mut ctx = TestCtx::new_fast();
        let edge = ctx.g.map().environment.bounds().ur;
        let area = OBB::new(edge - Vec2::splat(8.0), Vec2::X, 16.0, 16.0);
        let kind = Some(ZoneKind::Office);
        ctx.apply(&[WorldCommand::ZonePaint {
            area,
            kind,
            density: Density::Medium,
        }]);
        let grid = ctx.g.read::<ZoneGrid>();
        let last = edge / ZONE_CELL_SIZE - Vec2::splat(1.0);
        assert_eq!(grid.cells.len(), 4);
        for x in last.x as i32 - 1..=last.x as i32 {
            for y in last.y as i32 - 1..=last.y as i32 {
                assert_eq!(grid.cells[&(x, y)].density, Density::Medium);
            }
        }
        drop(grid);
        ctx.apply(&[WorldCommand::ZonePaint {
            area,
            kind: None,
            density: Density::Medium,
        }]);
        assert!(ctx.g.read::<ZoneGrid>().cells.is_empty());
        assert!(ctx.g.read::<CommandOutcomes>().last.is_empty());
    }

    #[test]
    fn u1_zone_guided_medium_paint_is_locked_but_erase_is_allowed() {
        use crate::economy::progression::{ProgressionMode, ProgressionState};

        let mut ctx = TestCtx::new_fast();
        let area = OBB::new(vec2(8.0, 8.0), Vec2::X, 16.0, 16.0);
        let paint = |kind, density| WorldCommand::ZonePaint {
            area,
            kind,
            density,
        };
        let kind = Some(ZoneKind::Residential);
        ctx.apply(&[paint(kind, Density::Medium)]);
        ctx.g.write::<ProgressionState>().mode = ProgressionMode::Guided;
        let generation = ctx.g.read::<ZoneGrid>().generation;
        assert_eq!(ctx.g.read::<ZoneGrid>().cells.len(), 4);

        ctx.apply(&[paint(Some(ZoneKind::Office), Density::Medium)]);
        assert_eq!(
            ctx.g.read::<CommandOutcomes>().last,
            vec![("ZonePaint", Rejection::Locked { milestone: 1 })]
        );
        assert_eq!(ctx.g.read::<ZoneGrid>().generation, generation);

        ctx.apply(&[paint(None, Density::Medium)]);
        assert!(ctx.g.read::<ZoneGrid>().cells.is_empty());
        assert!(ctx.g.read::<CommandOutcomes>().last.is_empty());
    }

    #[test]
    fn generation_changes_only_on_effective_paints() {
        let mut grid = ZoneGrid::default();
        let area = OBB::new(vec2(16.0, 16.0), Vec2::X, 16.0, 16.0);
        let zone = Some(ZoneKind::Residential);
        for (kind, density, generation) in [
            (None, Density::Low, 0),
            (zone, Density::Low, 1),
            (zone, Density::Low, 1),
            (zone, Density::High, 2),
            (Some(ZoneKind::Commercial), Density::High, 3),
            (None, Density::Low, 4),
            (None, Density::High, 4),
        ] {
            grid.paint(area, kind, density);
            assert_eq!(grid.generation, generation);
        }
        assert!(grid.cells.is_empty());
    }
}
