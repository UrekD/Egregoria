//! Public surface parking uses the same held-space and driveway state machine as garages.
use super::{BuildingParkingKind, ParkingLayout, StallPath, CAR_RADIUS};
use crate::map::{BuildingKind, CurbAnchor, Map, ProjectFilter, RoadID, RoadSide};
use crate::map::{PathEndpoint, ProjectKind, ROAD_Z_OFFSET};
use crate::map_dynamic::DirectedEndpoint;
use crate::transportation::{prepare_driveway_mouth, Sweep};
use crate::world_command::Rejection;
use geom::{Intersect, LinearColor, Segment, Shape, Transform, Vec2, Vec3, OBB};
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
    Ground { front_block: [i32; 2], facing: u8 },
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
    pub fn daily_upkeep(self) -> Money {
        Money::new_bucks(i64::from(self.width_blocks) * 4)
    }

    pub fn cost(self) -> Money {
        Money::new_bucks(
            i64::from(self.width_blocks) * 350 + self.mouths.iter().flatten().count() as i64 * 300,
        )
    }

    pub(super) fn protected_roads(self) -> impl Iterator<Item = RoadID> {
        let frame = match self.frame {
            LotFrame::Curb(at) => Some(at.road),
            LotFrame::Ground { .. } => None,
        };
        frame
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
}

impl LotFootprint {
    fn at(self, x: f32, y: f32) -> Vec3 {
        self.front + (self.along * x + self.outward * y).z0()
    }

    fn local(self, point: Vec3) -> Vec2 {
        let offset = point.xy() - self.front.xy();
        Vec2::new(offset.dot(self.along), offset.dot(self.outward))
    }

    pub fn stall_centers(self) -> impl Iterator<Item = Vec3> {
        let stalls = ((self.width - 5.0) / STALL_WIDTH).floor() as u8;
        (0..stalls).map(move |index| {
            self.at(
                (f32::from(index) - (f32::from(stalls) - 1.0) * 0.5) * STALL_WIDTH,
                STALL_CENTER,
            )
        })
    }

    pub fn blocks(self) -> impl Iterator<Item = OBB> {
        let blocks = (self.width / BLOCK_SIZE) as i8;
        (0..blocks).flat_map(move |x| {
            (0..2_i8).map(move |y| {
                OBB::new(
                    self.at(
                        (f32::from(x) + 0.5) * BLOCK_SIZE - self.width * 0.5,
                        (f32::from(y) + 0.5) * BLOCK_SIZE,
                    )
                    .xy(),
                    self.outward,
                    BLOCK_SIZE,
                    BLOCK_SIZE,
                )
            })
        })
    }
}

pub fn lot_footprint(map: &Map, frame: LotFrame, width_blocks: u8) -> Option<LotFootprint> {
    if !(2..=MAX_WIDTH_BLOCKS).contains(&width_blocks) {
        return None;
    }
    let (front, outward) = match frame {
        LotFrame::Curb(anchor) => curb_edge(map, anchor)?,
        LotFrame::Ground {
            front_block: [x, y],
            facing,
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
    Some(LotFootprint {
        shape: OBB::new(front.xy() + outward * (DEPTH * 0.5), outward, DEPTH, width),
        front,
        outward,
        along: outward.perpendicular(),
        width,
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

pub fn lot_mouth_position(map: &Map, footprint: LotFootprint, anchor: CurbAnchor) -> Option<Vec3> {
    let (edge, outward) = curb_edge(map, anchor)?;
    let at = footprint.local(edge);
    if outward.dot(footprint.outward) < 0.98
        || at.x.abs() > footprint.width * 0.5 - 3.0
        || !(-6.0..=0.12).contains(&at.y)
    {
        return None;
    }
    prepare_driveway_mouth(map, anchor).ok()?;
    Some(edge)
}

pub fn lot_gate_position(
    map: &Map,
    footprint: LotFootprint,
    endpoint: PathEndpoint,
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
        at.y.clamp(0.0, DEPTH),
    );
    if nearest.distance2(at) > 6.0_f32.powi(2) {
        return None;
    }
    let edge = footprint.at(nearest.x, nearest.y);
    let walk = Segment::new(point.xy(), edge.xy());
    let direction = walk.vec().try_normalize().unwrap_or(footprint.outward);
    let corridor = OBB::new(
        (walk.src + walk.dst) * 0.5,
        direction,
        walk.src.distance(walk.dst).max(0.1),
        0.3,
    );
    if !map.authorize_driveway_pad(&corridor.corners)
        || map
            .environment()
            .intersects_water(&geom::Polygon(corridor.corners.to_vec()))
    {
        return None;
    }
    let corridor = geom::Polygon::from(corridor.corners.as_slice());
    let skip = endpoint.road();
    if map
        .spatial_map()
        .query(walk.bbox(), ProjectFilter::ROAD | ProjectFilter::BUILDING)
        .any(|object| match object {
            ProjectKind::Road(id) => {
                Some(id) != skip
                    && map
                        .get(id)
                        .is_some_and(|road| road.boldline().intersects(&corridor))
            }
            ProjectKind::Building(id) => map.get(id).is_some_and(|building| {
                (building.obb.corners != footprint.shape.corners
                    || !matches!(building.kind, BuildingKind::ParkingLot { .. }))
                    && building.obb.intersects(&walk)
            }),
            _ => false,
        })
    {
        return None;
    }
    Some(point)
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct SurfaceAccess {
    pub(super) slots: Vec<(DirectedEndpoint, DirectedEndpoint)>,
    pub(super) apron: Vec<[Vec3; 2]>,
}

#[derive(Clone, Copy)]
struct Mouth {
    spec: LotMouth,
    edge: Vec3,
    approach: DirectedEndpoint,
    release: DirectedEndpoint,
    from: Transform,
    to: Transform,
}

fn surface_sweep(pad: LotFootprint, mouth: Mouth, stall: Vec3, outbound: bool) -> Sweep {
    let x = pad.local(mouth.edge).x;
    let sx = pad.local(stall).x;
    let aisle = AISLE + 0.75;
    let road = if outbound { mouth.to } else { mouth.from };
    let across = if sx > x { pad.along } else { -pad.along };
    let inward = pad.outward.z0();
    // The staggered heading changes stay in the aisle. Perpendicular motion
    // at the stall represents reversing out of a nose-in space.
    let turn = (sx - x).abs().min(3.0) * 0.48;
    let apron = Transform::new_dir(pad.at(x, 2.4), inward);
    let mut poses = vec![road, apron];
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
    if let LotFrame::Curb(at) = draft.frame {
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
            let want = at - pad.outward * road.width * 0.5;
            let actual = road.interfaced_points().project_2d(want);
            if actual.xy().distance2(want) > 0.10_f32.powi(2) {
                return Err(Rejection::InvalidBuilding);
            }
        }
    }
    let env = map.environment();
    for x in 0..=u32::from(draft.width_blocks) * 4 {
        for y in 0..=8 {
            let point = pad.at(x as f32 * 2.0 - pad.width * 0.5, y as f32 * 2.0);
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
) -> Result<ParkingLayout, Rejection> {
    validate_surface_pad(map, draft, pad)?;
    let mut mouths = Vec::new();
    for spec in draft.mouths.into_iter().flatten() {
        if map.pick_curb(spec.at.road, spec.at.side, spec.at.station_cm) != Some(spec.at) {
            return Err(Rejection::StaleRoadEdit);
        }
        let edge = lot_mouth_position(map, pad, spec.at).ok_or(Rejection::InvalidBuilding)?;
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
            approach,
            release,
            from,
            to,
        });
    }
    let primary = *mouths.first().ok_or(Rejection::InvalidBuilding)?;
    let gate = draft
        .foot_gate
        .unwrap_or(PathEndpoint::Sidewalk(primary.spec.at));
    // Validate the revision even when the selected sidewalk's point still looks the same.
    map.path_endpoint_position(gate)
        .ok_or(Rejection::StaleRoadEdit)?;
    let door = lot_gate_position(map, pad, gate).ok_or(Rejection::InvalidBuilding)?;
    let mut paths = Vec::new();
    let mut slots = Vec::new();
    let mut masks = BTreeSet::new();
    let mut bounds = pad.shape.bbox();
    let environment = map.environment();
    for stall in pad.stall_centers() {
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
        for (mouth, sweep) in [(inbound, &into), (outbound, &out)] {
            for point in sweep.points {
                let xy = point.xy();
                let clearance = OBB::new(xy, pad.outward, 2.5, 2.5);
                if environment.is_water(xy)
                    || environment
                        .height(xy)
                        .is_none_or(|z| (z + ROAD_Z_OFFSET - point.z).abs() > 0.6)
                    || !map.authorize_driveway_pad(&clearance.corners)
                    || (pad.local(point).y > 1.5 && !pad.shape.expand(-0.5).contains(xy))
                    || (pad.local(point).y < 1.25
                        && map
                            .spatial_map()
                            .query(clearance, ProjectFilter::BUILDING)
                            .filter_map(|kind| kind.as_building())
                            .any(|id| {
                                map.get(id).is_some_and(|building| {
                                    building.obb.corners != pad.shape.corners
                                        || !matches!(building.kind, BuildingKind::ParkingLot { .. })
                                })
                            }))
                {
                    return Err(Rejection::InvalidBuilding);
                }
            }
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
        depth: DEPTH,
        surface: Some(Box::new(SurfaceAccess {
            slots,
            apron: mouths
                .iter()
                .map(|mouth| [mouth.edge, pad.at(pad.local(mouth.edge).x, 2.4)])
                .collect(),
        })),
    })
}

pub(in crate::map) fn surface_mesh(pad: LotFootprint) -> crate::map::procgen::ColoredMesh {
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
    rectangle(0.0, DEPTH * 0.5, pad.width, DEPTH, asphalt, 0.015);
    let mut edge = None;
    for stall in pad.stall_centers() {
        let x = pad.local(stall).x;
        rectangle(
            x - STALL_WIDTH * 0.5,
            STALL_CENTER,
            0.12,
            STALL_DEPTH,
            stripe,
            0.035,
        );
        edge = Some(x + STALL_WIDTH * 0.5);
    }
    if let Some(x) = edge {
        rectangle(x, STALL_CENTER, 0.12, STALL_DEPTH, stripe, 0.035);
    }
    mesh
}
