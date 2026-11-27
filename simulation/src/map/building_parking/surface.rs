//! Public surface parking uses the same held-space and driveway state machine as garages.
use super::{BuildingParkingKind, ParkingLayout, StallPath, CAR_RADIUS};
use crate::map::{BuildingID, CurbAnchor, Map, ProjectFilter, RoadID, RoadSide};
use crate::map::{PathEndpoint, ProjectKind, ROAD_Z_OFFSET};
use crate::map_dynamic::DirectedEndpoint;
use crate::transportation::{prepare_driveway_mouth, Sweep};
use crate::world_command::Rejection;
use geom::{
    BoldLine, Intersect, LinearColor, PolyLine, Polygon, Segment, Shape, Transform, Vec2, Vec3, OBB,
};
use prototypes::Money;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const BLOCK_SIZE: f32 = 8.0;
pub const MAX_WIDTH_BLOCKS: u8 = 8;
const DEPTH: f32 = 2.0 * BLOCK_SIZE;
const AISLE: f32 = 6.0;
const STALL_WIDTH: f32 = 2.5;
const STALL_DEPTH: f32 = 5.0;
const STALL_CENTER: f32 = DEPTH - STALL_DEPTH * 0.5 - 0.5;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LotFrame {
    Curb(CurbAnchor),
    Ground {
        front_block: [i32; 2],
        facing: u8,
    },
    CurbGrid {
        at: CurbAnchor,
        depth_blocks: u8,
    },
    GroundGrid {
        front_block: [i32; 2],
        facing: u8,
        depth_blocks: u8,
    },
    CurbPaint {
        at: CurbAnchor,
        rows: [u8; 8],
    },
    GroundPaint {
        front_block: [i32; 2],
        facing: u8,
        rows: [u8; 8],
    },
}

impl LotFrame {
    pub fn depth_blocks(self) -> u8 {
        match self {
            Self::Curb(_) | Self::Ground { .. } => 2,
            Self::CurbGrid { depth_blocks, .. } | Self::GroundGrid { depth_blocks, .. } => {
                depth_blocks
            }
            Self::CurbPaint { rows, .. } | Self::GroundPaint { rows, .. } => rows
                .iter()
                .rposition(|&row| row != 0)
                .map_or(0, |index| index as u8 + 1),
        }
    }

    fn is_grid(self) -> bool {
        !matches!(self, Self::Curb(_) | Self::Ground { .. })
    }

    pub fn painted_rows(self) -> Option<[u8; 8]> {
        match self {
            Self::CurbPaint { rows, .. } | Self::GroundPaint { rows, .. } => Some(rows),
            _ => None,
        }
    }

    pub fn paint(self, rows: [u8; 8]) -> Self {
        match self {
            Self::Curb(at) | Self::CurbGrid { at, .. } | Self::CurbPaint { at, .. } => {
                Self::CurbPaint { at, rows }
            }
            Self::Ground {
                front_block,
                facing,
            }
            | Self::GroundGrid {
                front_block,
                facing,
                ..
            }
            | Self::GroundPaint {
                front_block,
                facing,
                ..
            } => Self::GroundPaint {
                front_block,
                facing,
                rows,
            },
        }
    }

    pub(in crate::map) fn curb(self) -> Option<CurbAnchor> {
        match self {
            Self::Curb(at) | Self::CurbGrid { at, .. } | Self::CurbPaint { at, .. } => Some(at),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouthUse {
    In,
    Out,
    Both,
}

impl MouthUse {
    pub fn label(self) -> &'static str {
        match self {
            Self::In => "In",
            Self::Out => "Out",
            Self::Both => "Both",
        }
    }

    fn inbound(self) -> bool {
        self != Self::Out
    }

    fn outbound(self) -> bool {
        self != Self::In
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LotMouth {
    pub at: CurbAnchor,
    pub mode: MouthUse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceLotDraft {
    pub frame: LotFrame,
    pub width_blocks: u8,
    pub mouths: [Option<LotMouth>; 2],
    pub foot_gate: Option<PathEndpoint>,
}

impl SurfaceLotDraft {
    pub fn block_count(self) -> u32 {
        self.frame.painted_rows().map_or(
            u32::from(self.width_blocks) * u32::from(self.frame.depth_blocks()),
            |rows| rows.into_iter().map(u8::count_ones).sum(),
        )
    }

    pub fn daily_upkeep(self) -> Money {
        Money::new_bucks(i64::from(self.block_count()) * 2)
    }

    pub fn cost(self) -> Money {
        Money::new_bucks(
            i64::from(self.block_count()) * 175 + self.mouths.iter().flatten().count() as i64 * 300,
        )
    }

    pub(super) fn protected_roads(self) -> impl Iterator<Item = RoadID> {
        self.frame
            .curb()
            .map(|at| at.road)
            .into_iter()
            .chain(self.foot_gate.and_then(PathEndpoint::road))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct LotFootprint {
    pub shape: OBB,
    pub front: Vec3,
    pub outward: Vec2,
    pub along: Vec2,
    width: f32,
    depth: f32,
    grid: bool,
    painted: Option<[u8; 8]>,
}

impl LotFootprint {
    fn at(self, x: f32, y: f32) -> Vec3 {
        self.front + (self.along * x + self.outward * y).z0()
    }

    fn local(self, point: Vec3) -> Vec2 {
        let offset = point.xy() - self.front.xy();
        Vec2::new(offset.dot(self.along), offset.dot(self.outward))
    }

    pub(in crate::map) fn same_paving(self, other: Self) -> bool {
        self.shape.corners == other.shape.corners && self.painted == other.painted
    }

    fn block_local(self, point: Vec2) -> Option<[usize; 2]> {
        let x = ((point.x + self.width * 0.5) / BLOCK_SIZE).floor() as i32;
        let y = (point.y / BLOCK_SIZE).floor() as i32;
        if x < 0 || x >= (self.width / BLOCK_SIZE) as i32 || !(0..8).contains(&y) {
            return None;
        }
        Some([x as usize, y as usize])
    }

    pub fn block_at(self, point: Vec3) -> Option<[usize; 2]> {
        self.block_local(self.local(point))
    }

    fn has_paving(self, local: Vec2) -> bool {
        self.painted.is_none_or(|rows| {
            self.block_local(local)
                .is_some_and(|[x, y]| rows[y] & (1 << x) != 0)
        })
    }

    pub fn contains(self, point: Vec3) -> bool {
        self.shape.contains(point.xy()) && self.has_paving(self.local(point))
    }

    fn stall_fits(self, x: f32, y: f32) -> bool {
        let aisle = (y / DEPTH).floor() * DEPTH + 8.0;
        [-1.25, 1.25].into_iter().all(|dx| {
            [y - 2.5, y + 2.5, aisle]
                .into_iter()
                .all(|ty| self.has_paving(Vec2::new(x + dx, ty)))
        })
    }

    pub fn stall_centers(self) -> impl Iterator<Item = Vec3> {
        let stalls = ((self.width - 5.0) / STALL_WIDTH).floor() as u8;
        let rows = if self.grid {
            (self.depth / BLOCK_SIZE) as u8
        } else {
            1
        };
        (0..rows).flat_map(move |row| {
            let y = if self.grid && row % 2 == 0 {
                f32::from(row) * BLOCK_SIZE + 3.0
            } else {
                f32::from(row / 2) * DEPTH + STALL_CENTER
            };
            (0..stalls).filter_map(move |index| {
                let x = (f32::from(index) - (f32::from(stalls) - 1.0) * 0.5) * STALL_WIDTH;
                ((self.depth <= DEPTH || x.abs() >= 3.3) && self.stall_fits(x, y))
                    .then(|| self.at(x, y))
            })
        })
    }

    fn stalls_for(self, mouths: &[Mouth]) -> impl Iterator<Item = Vec3> + '_ {
        self.stall_centers().filter(move |&stall| {
            !self.grid
                || !mouths.iter().any(|mouth| {
                    let at = self.local(stall);
                    let mouth_at = self.local(mouth.edge);
                    mouth.inward.dot(self.outward).abs() >= 0.98
                        && (at.y - mouth_at.y.clamp(0.0, self.depth)).abs() < STALL_DEPTH
                        && (at.x - mouth_at.x).abs() < 3.5
                })
        })
    }

    pub fn blocks(self) -> impl Iterator<Item = OBB> {
        let blocks = (self.width / BLOCK_SIZE) as i8;
        let rows = (self.depth / BLOCK_SIZE) as i8;
        (0..blocks).flat_map(move |x| {
            (0..rows).filter_map(move |y| {
                if self
                    .painted
                    .is_some_and(|cells| cells[y as usize] & (1 << x) == 0)
                {
                    return None;
                }
                Some(OBB::new(
                    self.at(
                        (f32::from(x) + 0.5) * BLOCK_SIZE - self.width * 0.5,
                        (f32::from(y) + 0.5) * BLOCK_SIZE,
                    )
                    .xy(),
                    self.outward,
                    BLOCK_SIZE,
                    BLOCK_SIZE,
                ))
            })
        })
    }
}

fn connected_paving(rows: [u8; 8], width: u8) -> bool {
    let row_mask = ((1_u16 << width) - 1) as u8;
    if rows[0] == 0 || rows.iter().any(|&row| row & !row_mask != 0) {
        return false;
    }
    let mask = u64::from_le_bytes(rows);
    let mut connected = 1_u64 << mask.trailing_zeros();
    const LEFT: u64 = 0x0101_0101_0101_0101;
    const RIGHT: u64 = LEFT << 7;
    loop {
        let next = mask
            & (connected
                | ((connected & !RIGHT) << 1)
                | ((connected & !LEFT) >> 1)
                | (connected << 8)
                | (connected >> 8));
        if next == connected {
            return connected == mask;
        }
        connected = next;
    }
}

pub fn lot_footprint(map: &Map, frame: LotFrame, width_blocks: u8) -> Option<LotFootprint> {
    if !(2..=MAX_WIDTH_BLOCKS).contains(&width_blocks) {
        return None;
    }
    let depth_blocks = frame.depth_blocks();
    let painted = frame.painted_rows();
    if painted.map_or(![2, 4, 6, 8].contains(&depth_blocks), |rows| {
        depth_blocks < 2 || !connected_paving(rows, width_blocks)
    }) {
        return None;
    }
    let (front, outward) = match frame {
        LotFrame::Curb(anchor)
        | LotFrame::CurbGrid { at: anchor, .. }
        | LotFrame::CurbPaint { at: anchor, .. } => curb_edge(map, anchor)?,
        LotFrame::Ground {
            front_block: [x, y],
            facing,
        }
        | LotFrame::GroundGrid {
            front_block: [x, y],
            facing,
            ..
        }
        | LotFrame::GroundPaint {
            front_block: [x, y],
            facing,
            ..
        } => {
            let outward = [Vec2::Y, Vec2::X, -Vec2::Y, -Vec2::X]
                .get(usize::from(facing))
                .copied()?;
            let stagger = f32::from(width_blocks % 2) * BLOCK_SIZE * 0.5;
            let xy = Vec2::new(x as f32, y as f32) * BLOCK_SIZE + outward.perpendicular() * stagger;
            let z = map.environment().height(xy)? + ROAD_Z_OFFSET;
            (xy.z(z), outward)
        }
    };
    let width = f32::from(width_blocks) * BLOCK_SIZE;
    let depth = f32::from(depth_blocks) * BLOCK_SIZE;
    Some(LotFootprint {
        shape: OBB::new(front.xy() + outward * (depth * 0.5), outward, depth, width),
        front,
        outward,
        along: outward.perpendicular(),
        width,
        depth,
        grid: frame.is_grid(),
        painted,
    })
}

pub fn curb_edge(map: &Map, anchor: CurbAnchor) -> Option<(Vec3, Vec2)> {
    let road = map.get(anchor.road)?;
    let (center, segment) = map.path_station(road, anchor.station_cm)?;
    let tangent = (road.points()[segment] - road.points()[segment - 1])
        .xy()
        .try_normalize()?;
    let outward = match anchor.side {
        RoadSide::Right => tangent.perpendicular(),
        RoadSide::Left => -tangent.perpendicular(),
    };
    Some((center + outward.z0() * (road.width * 0.5), outward))
}

fn mouth_edge(map: &Map, footprint: LotFootprint, anchor: CurbAnchor) -> Option<(Vec3, Vec2)> {
    let (edge, outward) = curb_edge(map, anchor)?;
    let at = footprint.local(edge);
    let half_width = footprint.width * 0.5;
    let fits = if outward.dot(footprint.outward) >= 0.98 {
        at.x.abs() <= half_width - 3.0 && (-6.0..=0.12).contains(&at.y)
    } else if outward.dot(-footprint.outward) >= 0.98 {
        at.x.abs() <= half_width - 3.0
            && (footprint.depth - 0.12..=footprint.depth + 6.0).contains(&at.y)
    } else if outward.dot(footprint.along).abs() >= 0.98 {
        let offset = -outward.dot(footprint.along).signum() * at.x - half_width;
        (-0.12..=6.0).contains(&offset) && (3.0..=footprint.depth - 3.0).contains(&at.y)
    } else {
        false
    };
    let inside = at + Vec2::new(outward.dot(footprint.along), outward.dot(footprint.outward)) * 2.4;
    (fits
        && footprint.has_paving(Vec2::new(
            inside.x.clamp(-half_width + 0.1, half_width - 0.1),
            inside.y.clamp(0.1, footprint.depth - 0.1),
        )))
    .then_some((edge, outward))
}

pub fn lot_mouth_position(map: &Map, footprint: LotFootprint, anchor: CurbAnchor) -> Option<Vec3> {
    let (edge, _) = mouth_edge(map, footprint, anchor)?;
    prepare_driveway_mouth(map, anchor).ok()?;
    Some(edge)
}

pub fn lot_gate_position(
    map: &Map,
    footprint: LotFootprint,
    endpoint: PathEndpoint,
) -> Option<Vec3> {
    checked_gate_position(map, footprint, endpoint, None)
}

fn checked_gate_position(
    map: &Map,
    footprint: LotFootprint,
    endpoint: PathEndpoint,
    own: Option<BuildingID>,
) -> Option<Vec3> {
    let point = match endpoint {
        PathEndpoint::Sidewalk(anchor) => {
            let (edge, outward) = curb_edge(map, anchor)?;
            edge - outward.z0() * 1.5
        }
        PathEndpoint::Ground { .. } => return None,
        _ => map.path_endpoint_position(endpoint)? + Vec3::z(ROAD_Z_OFFSET),
    };
    let at = footprint.local(point);
    if (point.z - footprint.front.z).abs() > 0.5 {
        return None;
    }
    let nearest = Vec2::new(
        at.x.clamp(-footprint.width * 0.5, footprint.width * 0.5),
        at.y.clamp(0.0, footprint.depth),
    );
    if nearest.distance2(at) > 6.0_f32.powi(2) {
        return None;
    }
    let corridor = footprint.pedestrian_corridor(point.xy());
    if corridor.axis()[1].mag() > 6.01
        || !map.authorize_driveway_pad(&corridor.corners)
        || map
            .environment()
            .intersects_water(&geom::Polygon(corridor.corners.to_vec()))
    {
        return None;
    }
    let corridor = Polygon::from(corridor.corners.as_slice());
    let skip = endpoint.road();
    if map
        .spatial_map()
        .construction_candidates(
            corridor.bbox(),
            ProjectFilter::ROAD | ProjectFilter::BUILDING,
        )
        .any(|object| match object {
            ProjectKind::Road(id) => {
                Some(id) != skip
                    && map
                        .get(id)
                        .is_some_and(|road| road.boldline().intersects(&corridor))
            }
            ProjectKind::Building(id) if Some(id) != own => map.get(id).is_some_and(|building| {
                map.surface_lot_footprint(id).map_or_else(
                    || building.obb.intersects(&corridor),
                    |pad| pad.blocks().any(|block| block.intersects(&corridor)),
                )
            }),
            _ => false,
        })
    {
        return None;
    }
    Some(point)
}

impl LotFootprint {
    pub(in crate::map) fn pedestrian_corridor(self, gate: Vec2) -> OBB {
        if self.painted.is_none() {
            return gate_corridor(self.shape, gate);
        }
        self.blocks()
            .map(|block| gate_corridor(block, gate))
            .min_by(|a, b| a.axis()[1].mag2().total_cmp(&b.axis()[1].mag2()))
            .expect("validated nonempty paving")
    }

    fn sweep_on_paving(self, sweep: &Sweep, mouth: Mouth) -> bool {
        let apron = apron_point(self, mouth);
        sweep.points.iter().all(|&point| {
            if mouth.inward.dot((point - apron).xy()) <= -0.9 {
                return true;
            }
            let point = self.local(point);
            [Vec2::ZERO, Vec2::X, -Vec2::X, Vec2::Y, -Vec2::Y]
                .into_iter()
                .all(|offset| self.has_paving(point + offset))
        })
    }
}

pub(in crate::map) fn gate_corridor(pad: OBB, gate: Vec2) -> OBB {
    let from = pad.corners[0];
    let edge = pad.axis().into_iter().fold(from, |edge, axis| {
        edge + axis * ((gate - from).dot(axis) / axis.mag2()).clamp(0.0, 1.0)
    });
    let direction = (edge - gate).try_normalize().unwrap_or(Vec2::X);
    OBB::new(
        (gate + edge) * 0.5,
        direction,
        gate.distance(edge).max(0.1),
        0.3,
    )
}

fn surface_sweep_clear(
    map: &Map,
    own: Option<BuildingID>,
    mouth: Mouth,
    sweep: &Sweep,
    work: &mut usize,
) -> Result<(), Rejection> {
    let footprint = BoldLine::new(
        PolyLine::new(sweep.points.iter().map(|point| point.xy()).collect()),
        CAR_RADIUS,
    );
    let bounds = footprint.bbox();
    let spend = |remaining: &mut usize, amount| {
        crate::map::spend_water_work(remaining, amount).map_err(|_| Rejection::WaterWorkLimit)
    };
    map.spatial_map()
        .reserve_construction_query(bounds, work)
        .map_err(|_| Rejection::WaterWorkLimit)?;
    for object in map.spatial_map().construction_candidates(
        bounds,
        ProjectFilter::BUILDING | ProjectFilter::ROAD | ProjectFilter::INTER,
    ) {
        let blocked = match object {
            ProjectKind::Building(id) if Some(id) != own => {
                let other = map.surface_lot_footprint(id);
                spend(
                    work,
                    sweep.points.len() * 4 * other.map_or(1, |pad| pad.blocks().count()),
                )?;
                let building = &map.buildings()[id];
                other.map_or_else(
                    || footprint.intersects(&building.obb),
                    |pad| pad.blocks().any(|block| footprint.intersects(&block)),
                )
            }
            ProjectKind::Road(id) if id != mouth.spec.at.road => {
                let road = &map.roads()[id];
                spend(work, sweep.points.len() * road.points().len())?;
                footprint.intersects(&road.boldline())
            }
            ProjectKind::Intersection(id) => {
                let joint = &map.intersections()[id];
                spend(work, sweep.points.len() * joint.roads.len().max(1))?;
                footprint.intersects(&geom::Circle {
                    center: joint.pos.xy(),
                    radius: joint.radius,
                })
            }
            _ => false,
        };
        if blocked {
            return Err(Rejection::BuildingOverlap);
        }
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct SurfaceAccess {
    pub(super) slots: Vec<(DirectedEndpoint, DirectedEndpoint)>,
    pub(super) apron: Vec<[Vec3; 2]>,
    pub(super) gate_bounds: geom::AABB,
}

#[derive(Clone, Copy)]
struct Mouth {
    spec: LotMouth,
    edge: Vec3,
    inward: Vec2,
    approach: DirectedEndpoint,
    release: DirectedEndpoint,
    from: Transform,
    to: Transform,
}

fn apron_point(pad: LotFootprint, mouth: Mouth) -> Vec3 {
    let local = pad.local(mouth.edge);
    let half_width = pad.width * 0.5;
    let edge = pad.at(
        local.x.clamp(-half_width, half_width),
        local.y.clamp(0.0, pad.depth),
    );
    edge + mouth.inward.z0() * 2.4
}

fn grid_sweep(pad: LotFootprint, mouth: Mouth, stall: Vec3, outbound: bool) -> Sweep {
    let road = if outbound { mouth.to } else { mouth.from };
    let apron = apron_point(pad, mouth);
    let local = pad.local(apron);
    let stall = pad.local(stall);
    let aisle_for = |y: f32| (y / DEPTH).floor() * DEPTH + 8.0;
    let aisle = aisle_for(stall.y);
    let entry_aisle = aisle_for(local.y);
    let cross = if stall.x >= local.x {
        pad.along
    } else {
        -pad.along
    };
    let down = if aisle > entry_aisle {
        pad.outward
    } else {
        -pad.outward
    };
    let forward = if stall.y > aisle {
        pad.outward
    } else {
        -pad.outward
    };
    let mut poses = vec![road, Transform::new_dir(apron, mouth.inward.z0())];
    let mut push = |x, y, direction: Vec2| {
        let point = pad.at(x, y);
        if poses.last().unwrap().pos.distance2(point) > 0.25 {
            poses.push(Transform::new_dir(point, direction.z0()));
        }
    };
    push(local.x, entry_aisle, cross);
    if aisle != entry_aisle {
        push(0.0, entry_aisle, down);
        push(
            0.0,
            aisle,
            if stall.x >= 0.0 {
                pad.along
            } else {
                -pad.along
            },
        );
    }
    push(stall.x, aisle, forward);
    push(stall.x, stall.y, forward);
    if outbound {
        poses.reverse();
        let exit = poses.len() - 1;
        for pose in &mut poses[..exit] {
            pose.dir = -pose.dir;
        }
    }
    Sweep::through(&poses, if outbound { road.dir } else { forward.z0() })
}

fn surface_sweep(pad: LotFootprint, mouth: Mouth, stall: Vec3, outbound: bool) -> Sweep {
    if pad.grid {
        return grid_sweep(pad, mouth, stall, outbound);
    }
    let x = pad.local(mouth.edge).x;
    let sx = pad.local(stall).x;
    let aisle = AISLE + 0.75;
    let road = if outbound { mouth.to } else { mouth.from };
    let across = if sx > x { pad.along } else { -pad.along };
    let inward = pad.outward.z0();
    // The staggered heading changes stay in the aisle. Perpendicular motion
    // at the stall represents reversing out of a nose-in space.
    let turn = (sx - x).abs().min(3.0) * 0.48;
    let apron = if mouth.inward.dot(pad.outward) >= 0.98 {
        Transform::new_dir(pad.at(x, 2.4), inward)
    } else {
        Transform::new_dir(apron_point(pad, mouth), mouth.inward.z0())
    };
    let mut poses = vec![road, apron];
    if mouth.inward.dot(pad.outward) < 0.98 {
        let local = pad.local(apron.pos);
        let approach = (stall - apron.pos)
            .xy()
            .try_normalize()
            .unwrap_or(pad.outward);
        poses.push(Transform::new_dir(
            pad.at(
                local.x.clamp(-pad.width * 0.5 + 3.0, pad.width * 0.5 - 3.0),
                aisle,
            ),
            approach.z0(),
        ));
    }
    if turn > 0.1 {
        let sign = (sx - x).signum();
        poses.push(Transform::new_dir(
            pad.at(x + turn * sign, aisle),
            across.z0(),
        ));
        if (sx - x).abs() > 2.0 * turn + 0.2 {
            poses.push(Transform::new_dir(
                pad.at(sx - turn * sign, aisle),
                across.z0(),
            ));
        }
    }
    poses.push(Transform::new_dir(stall, inward));
    if outbound {
        poses.reverse();
        let exit = poses.len() - 1;
        for pose in &mut poses[..exit] {
            pose.dir = -pose.dir;
        }
    }
    Sweep::through(&poses, if outbound { road.dir } else { inward })
}

pub(in crate::map) fn validate_surface_pad(
    map: &Map,
    draft: SurfaceLotDraft,
    pad: LotFootprint,
) -> Result<(), Rejection> {
    if let Some(at) = draft.frame.curb() {
        if map.pick_curb(at.road, at.side, at.station_cm) != Some(at) {
            return Err(Rejection::StaleRoadEdit);
        }
        let road = map.get(at.road).ok_or(Rejection::MissingReference)?;
        let front = Segment::new(
            pad.at(-pad.width * 0.5, 0.0).xy(),
            pad.at(pad.width * 0.5, 0.0).xy(),
        );
        for i in 0..=u32::from(draft.width_blocks) * 4 {
            let t = i as f32 / (f32::from(draft.width_blocks) * 4.0);
            let at = front.src.lerp(front.dst, t);
            if pad.painted.is_some()
                && !pad.has_paving(pad.local(at.z(pad.front.z)) + Vec2::new(-0.01, 0.01))
                && !pad.has_paving(pad.local(at.z(pad.front.z)) + Vec2::new(0.01, 0.01))
            {
                continue;
            }
            let want = at - pad.outward * road.width * 0.5;
            let actual = road.interfaced_points().project_2d(want);
            if actual.xy().distance2(want) > 0.10_f32.powi(2) {
                return Err(Rejection::InvalidBuilding);
            }
        }
    }
    let env = map.environment();
    for x in 0..=u32::from(draft.width_blocks) * 4 {
        for y in 0..=u32::from(draft.frame.depth_blocks()) * 4 {
            let point = pad.at(x as f32 * 2.0 - pad.width * 0.5, y as f32 * 2.0);
            if pad.painted.is_some()
                && ![-0.05, 0.05].into_iter().any(|dx| {
                    [-0.05, 0.05]
                        .into_iter()
                        .any(|dy| pad.has_paving(pad.local(point) + Vec2::new(dx, dy)))
                })
            {
                continue;
            }
            if env.is_water(point.xy())
                || env
                    .height(point.xy())
                    .is_none_or(|z| (z + ROAD_Z_OFFSET - point.z).abs() > 0.35)
            {
                return Err(Rejection::InvalidBuilding);
            }
        }
    }
    Ok(())
}

pub(in crate::map) fn prepare_surface_layout(
    map: &Map,
    draft: SurfaceLotDraft,
    pad: LotFootprint,
    own: Option<BuildingID>,
    work: &mut usize,
) -> Result<ParkingLayout, Rejection> {
    crate::map::spend_water_work(
        work,
        1_024 * usize::from(draft.width_blocks) * usize::from(draft.frame.depth_blocks()),
    )
    .map_err(|_| Rejection::WaterWorkLimit)?;
    for anchor in draft
        .mouths
        .iter()
        .flatten()
        .map(|mouth| mouth.at.road)
        .chain(draft.protected_roads())
    {
        let road = map.get(anchor).ok_or(Rejection::MissingReference)?;
        crate::map::spend_water_work(work, 48 * road.points().len())
            .map_err(|_| Rejection::WaterWorkLimit)?;
    }
    validate_surface_pad(map, draft, pad)?;
    let mut mouths = Vec::new();
    for spec in draft.mouths.into_iter().flatten() {
        if map.pick_curb(spec.at.road, spec.at.side, spec.at.station_cm) != Some(spec.at) {
            return Err(Rejection::StaleRoadEdit);
        }
        let (edge, inward) = mouth_edge(map, pad, spec.at).ok_or(Rejection::InvalidBuilding)?;
        if mouths
            .iter()
            .any(|other: &Mouth| other.edge.distance2(edge) < 9.0_f32.powi(2))
        {
            return Err(Rejection::InvalidBuilding);
        }
        let (_, approach, release, from, to, _) =
            prepare_driveway_mouth(map, spec.at).map_err(|_| Rejection::InvalidBuilding)?;
        mouths.push(Mouth {
            spec,
            edge,
            inward,
            approach,
            release,
            from,
            to,
        });
    }
    if pad.grid {
        mouths.sort_unstable_by_key(|mouth| {
            let at = mouth.spec.at;
            (at.road, at.side as u8, at.station_cm)
        });
    }
    let primary = *mouths.first().ok_or(Rejection::InvalidBuilding)?;
    let gate = draft
        .foot_gate
        .unwrap_or(PathEndpoint::Sidewalk(primary.spec.at));
    // Validate the revision even when the selected sidewalk's point still looks the same.
    map.path_endpoint_position(gate)
        .ok_or(Rejection::StaleRoadEdit)?;
    let gate_query = pad.shape.bbox().expand(6.5);
    map.spatial_map()
        .reserve_construction_query(gate_query, work)
        .map_err(|_| Rejection::WaterWorkLimit)?;
    for candidate in map
        .spatial_map()
        .construction_candidates(gate_query, ProjectFilter::ROAD)
    {
        if let ProjectKind::Road(id) = candidate {
            crate::map::spend_water_work(work, map.roads()[id].points().len() * 4)
                .map_err(|_| Rejection::WaterWorkLimit)?;
        }
    }
    let door = checked_gate_position(map, pad, gate, own).ok_or(Rejection::InvalidBuilding)?;
    let gate_bounds = pad.pedestrian_corridor(door.xy()).bbox();
    let mut paths = Vec::new();
    let mut slots = Vec::new();
    let mut masks = BTreeSet::new();
    let mut bounds = pad.shape.bbox();
    let environment = map.environment();
    for stall in pad.stalls_for(&mouths) {
        let closest = |allows: fn(MouthUse) -> bool| {
            mouths
                .iter()
                .copied()
                .filter(|mouth| allows(mouth.spec.mode))
                .min_by(|a, b| a.edge.distance2(stall).total_cmp(&b.edge.distance2(stall)))
                .ok_or(Rejection::InvalidBuilding)
        };
        let inbound = closest(MouthUse::inbound)?;
        let outbound = closest(MouthUse::outbound)?;
        let into = surface_sweep(pad, inbound, stall, false);
        let out = surface_sweep(pad, outbound, stall, true);
        if pad.painted.is_some()
            && (!pad.sweep_on_paving(&into, inbound) || !pad.sweep_on_paving(&out, outbound))
        {
            continue;
        }
        for (mouth, sweep) in [(inbound, &into), (outbound, &out)] {
            for point in sweep.points {
                let xy = point.xy();
                let clearance = OBB::new(xy, pad.outward, 2.5, 2.5);
                if environment.is_water(xy)
                    || environment
                        .height(xy)
                        .is_none_or(|z| (z + ROAD_Z_OFFSET - point.z).abs() > 0.6)
                    || !map.authorize_driveway_pad(&clearance.corners)
                    || (mouth.inward.dot((point - apron_point(pad, mouth)).xy()) > -0.9
                        && !pad.shape.expand(-0.5).contains(xy))
                {
                    return Err(Rejection::InvalidBuilding);
                }
            }
            surface_sweep_clear(map, own, mouth, sweep, work)?;
            let envelope = sweep.bounds();
            bounds.ll = bounds.ll.min(envelope.ll);
            bounds.ur = bounds.ur.max(envelope.ur);
            let road = map
                .get(mouth.spec.at.road)
                .ok_or(Rejection::MissingReference)?;
            let lane = map
                .get(mouth.approach.lane)
                .ok_or(Rejection::MissingReference)?;
            if let Some(curb) = road.parking_next_to(lane) {
                let bays = map
                    .parking
                    .closest_spots(curb, mouth.edge)
                    .ok_or(Rejection::InvalidBuilding)?;
                for spot in bays.take(12) {
                    let at = map
                        .parking
                        .get(spot)
                        .ok_or(Rejection::InvalidBuilding)?
                        .trans
                        .pos;
                    if sweep
                        .segments()
                        .any(|segment| segment.project(at).distance2(at) < 3.55_f32.powi(2))
                    {
                        masks.insert(spot);
                    }
                }
            }
        }
        slots.push((inbound.approach, outbound.release));
        paths.push(StallPath { into, out });
    }
    if slots.is_empty() {
        return Err(Rejection::InvalidBuilding);
    }
    Ok(ParkingLayout {
        kind: BuildingParkingKind::SurfaceLot(draft),
        anchor: primary.spec.at,
        entrance: primary.edge,
        approach: slots[0].0,
        release: slots[0].1,
        capacity: paths.len() as u16,
        paths,
        masks: masks.into_iter().collect(),
        bounds: bounds.expand(CAR_RADIUS),
        door,
        depth: pad.depth,
        surface: Some(Box::new(SurfaceAccess {
            slots,
            gate_bounds,
            apron: mouths
                .iter()
                .map(|&mouth| [mouth.edge, apron_point(pad, mouth)])
                .collect(),
        })),
    })
}

pub(in crate::map) fn surface_mesh(
    pad: LotFootprint,
    stalls: &[Vec3],
) -> crate::map::procgen::ColoredMesh {
    let asphalt = LinearColor::gray(0.22);
    let stripe = LinearColor::gray(0.76);
    let mut mesh = crate::map::procgen::ColoredMesh::default();
    let mut rectangle = |x: f32, y: f32, width: f32, depth: f32, color, lift| {
        let points = [
            pad.at(x - width * 0.5, y - depth * 0.5),
            pad.at(x + width * 0.5, y - depth * 0.5),
            pad.at(x + width * 0.5, y + depth * 0.5),
            pad.at(x - width * 0.5, y + depth * 0.5),
        ];
        mesh.faces.push((
            points.into_iter().rev().map(|p| p.up(lift)).collect(),
            color,
        ));
    };
    if pad.painted.is_some() {
        for block in pad.blocks() {
            let center = pad.local(block.center().z(pad.front.z));
            rectangle(center.x, center.y, BLOCK_SIZE, BLOCK_SIZE, asphalt, 0.015);
        }
    } else {
        rectangle(0.0, pad.depth * 0.5, pad.width, pad.depth, asphalt, 0.015);
    }
    for &stall in stalls {
        let point = pad.local(stall);
        for side in [-0.5, 0.5] {
            rectangle(
                point.x + STALL_WIDTH * side,
                point.y,
                0.08,
                STALL_DEPTH,
                stripe,
                0.035,
            );
        }
    }
    mesh
}
