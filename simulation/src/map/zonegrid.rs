use geom::{vec2, Shape, AABB, OBB};
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::BTreeMap;

pub use prototypes::{Density, ZoneKind};

pub const ZONE_CELL_SIZE: f32 = 8.0;
const MAX_PAINT_SIDE: f32 = 400.0;
const MAX_PAINT_CELLS: usize = 2_500;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZoneCell {
    pub kind: ZoneKind,
    pub density: Density,
}

#[derive(Default, Serialize, Deserialize)]
pub struct ZoneGrid {
    pub cells: BTreeMap<(i32, i32), ZoneCell>,
}

impl ZoneGrid {
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

        for key in cells {
            match kind {
                Some(kind) => self.cells.insert(key, ZoneCell { kind, density }),
                None => self.cells.remove(&key),
            };
        }
    }

    pub fn lot_zone(&self, area: &OBB) -> Option<ZoneCell> {
        if self.cells.is_empty() {
            return None;
        }

        let mut counts = BTreeMap::<_, (usize, usize)>::new();
        let mut total = 0;
        let mut zoned = 0;
        for key in cells_under(area) {
            total += 1;
            if let Some(cell) = self.cells.get(&key) {
                let (low, high) = counts.entry(cell.kind).or_default();
                match cell.density {
                    Density::Low => *low += 1,
                    Density::High => *high += 1,
                }
                zoned += 1;
            }
        }
        if 2 * zoned < total {
            return None;
        }

        let (kind, (low, high)) = counts
            .into_iter()
            .max_by_key(|&(kind, (low, high))| (low + high, Reverse(kind)))?;
        let density = if high > low {
            Density::High
        } else {
            Density::Low
        };
        Some(ZoneCell { kind, density })
    }
}

fn paint_cells(area: &OBB) -> Option<impl Iterator<Item = (i32, i32)> + '_> {
    let valid_size = area.segments().iter().all(|side| {
        let length_squared = (side.dst - side.src).mag2();
        length_squared <= MAX_PAINT_SIDE * MAX_PAINT_SIDE
    });
    valid_size.then(|| cells_under(area).take(MAX_PAINT_CELLS + 1))
}

fn cells_under(area: &OBB) -> impl Iterator<Item = (i32, i32)> + '_ {
    let bbox = area.bbox();
    let index = |coord: f32| (coord / ZONE_CELL_SIZE).floor() as i32;
    let xs = index(bbox.ll.x)..=index(bbox.ur.x);
    let ys = index(bbox.ll.y)..=index(bbox.ur.y);
    xs.flat_map(move |x| ys.clone().map(move |y| (x, y)))
        .filter(|&(x, y)| {
            let centre = (vec2(x as f32, y as f32) + vec2(0.5, 0.5)) * ZONE_CELL_SIZE;
            area.contains(centre)
        })
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
}
