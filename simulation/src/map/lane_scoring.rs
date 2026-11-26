use crate::map::{LaneID, LaneKind, Map, TraverseDirection, TraverseKind};
use crate::map_dynamic::transit::elapsed_ticks;
use crate::utils::resources::Resources;
use crate::world_command::bounded_command_vec;
use crate::World;
use prototypes::{game_secs_to_ticks, SECONDS_PER_REALTIME_SECOND};
use serde::{Deserialize, Serialize};

const MAX_LANES: usize = 4;
const MAX_WAIT: u8 = 30;
const QUEUED_VEHICLE_DELAY: u8 = 3;
const SAMPLE_TICKS: u64 = game_secs_to_ticks(30);
const MAX_VEHICLES: usize = 65_536;
const MAX_PROJECTION_WORK: usize = 16_384;
const APPROACH_SAMPLE_LENGTH: f32 = 30.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneScoringError {
    LaneCount,
    InvalidLaneGroup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct LaneWait {
    lane: LaneID,
    seconds: u8,
}

/// Opt-in route-cost samples for a single pinned group of directed parallel lanes.
/// Ordinary cities have no pilot state and keep the stock random lane tie-break.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneScoringPilot {
    #[serde(deserialize_with = "bounded_command_vec::<_, _, MAX_LANES>")]
    lanes: Vec<LaneWait>,
    sampled_at: Option<u64>,
}

impl LaneScoringPilot {
    pub fn sampled_at(&self) -> Option<u64> {
        self.sampled_at
    }

    pub fn wait_game_seconds(&self, lane: LaneID) -> Option<u8> {
        let index = self
            .lanes
            .binary_search_by_key(&lane, |item| item.lane)
            .ok()?;
        Some(self.lanes[index].seconds)
    }

    pub(super) fn wait_real_seconds(&self, lane: LaneID) -> Option<f32> {
        self.wait_game_seconds(lane)
            .map(|seconds| f32::from(seconds) / SECONDS_PER_REALTIME_SECOND as f32)
    }

    fn validate(&self, map: &Map) -> Result<(), LaneScoringError> {
        if !(2..=MAX_LANES).contains(&self.lanes.len()) {
            return Err(LaneScoringError::LaneCount);
        }
        if !self
            .lanes
            .windows(2)
            .all(|pair| pair[0].lane < pair[1].lane)
            || self.sampled_at.is_some_and(|tick| tick % SAMPLE_TICKS != 0)
            || self.lanes.iter().any(|sample| {
                sample.seconds > MAX_WAIT
                    || sample.seconds % QUEUED_VEHICLE_DELAY != 0
                    || (self.sampled_at.is_none() && sample.seconds != 0)
            })
        {
            return Err(LaneScoringError::InvalidLaneGroup);
        }
        let first = map
            .get(self.lanes[0].lane)
            .ok_or(LaneScoringError::InvalidLaneGroup)?;
        if self.lanes.iter().any(|sample| {
            !map.get(sample.lane).is_some_and(|lane| {
                matches!(lane.kind, LaneKind::Driving | LaneKind::Bus)
                    && (lane.parent, lane.src, lane.dst) == (first.parent, first.src, first.dst)
            })
        }) {
            return Err(LaneScoringError::InvalidLaneGroup);
        }
        Ok(())
    }
}

impl Map {
    pub fn lane_scoring_pilot(&self) -> Option<&LaneScoringPilot> {
        self.lane_scoring.as_ref()
    }

    /// Configure an isolated experimental run. Set the same pinned directed
    /// lanes on each replay/comparison arm before scoring the run.
    pub fn set_lane_scoring_pilot(
        &mut self,
        lanes: Option<&[LaneID]>,
    ) -> Result<(), LaneScoringError> {
        let Some(lanes) = lanes else {
            self.lane_scoring = None;
            return Ok(());
        };
        if !(2..=MAX_LANES).contains(&lanes.len()) {
            return Err(LaneScoringError::LaneCount);
        }
        let mut lanes: Vec<_> = lanes
            .iter()
            .map(|&lane| LaneWait { lane, seconds: 0 })
            .collect();
        lanes.sort_unstable_by_key(|sample| sample.lane);
        let pilot = LaneScoringPilot {
            lanes,
            sampled_at: None,
        };
        pilot.validate(self)?;
        self.lane_scoring = Some(pilot);
        Ok(())
    }

    pub(crate) fn load_lane_scoring_pilot(
        &mut self,
        pilot: Option<LaneScoringPilot>,
    ) -> std::io::Result<()> {
        if let Some(pilot) = &pilot {
            pilot.validate(self).map_err(|error| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid lane scoring pilot: {error:?}"),
                )
            })?;
        }
        self.lane_scoring = pilot;
        Ok(())
    }
}

pub(crate) fn lane_scoring_update(world: &mut World, resources: &mut Resources) {
    if resources.read::<Map>().lane_scoring.is_none() {
        return;
    }
    let tick = elapsed_ticks(resources).0;
    if tick % SAMPLE_TICKS != 0 || world.vehicles.len() > MAX_VEHICLES {
        return;
    }
    let mut map = resources.write::<Map>();
    let Some(pilot) = map.lane_scoring.as_ref() else {
        return;
    };
    if pilot.sampled_at == Some(tick) {
        return;
    }
    let mut sampled = pilot.clone();
    for sample in &mut sampled.lanes {
        sample.seconds = 0;
    }
    let mut projection_work = MAX_PROJECTION_WORK;
    for vehicle in world.vehicles.values() {
        if vehicle.collider.is_none() || vehicle.speed.0.abs() > 0.2 {
            continue;
        }
        let Some(traversal) = vehicle
            .it
            .get_travers()
            .filter(|path| path.dir == TraverseDirection::Forward)
        else {
            continue;
        };
        let TraverseKind::Lane(id) = traversal.kind else {
            continue;
        };
        let Ok(index) = sampled
            .lanes
            .binary_search_by_key(&id, |sample| sample.lane)
        else {
            continue;
        };
        let Some(lane) = map.get(id) else { continue };
        if lane.points.last().distance(vehicle.trans.pos) > APPROACH_SAMPLE_LENGTH {
            continue;
        }
        let Some(remaining) = projection_work.checked_sub(lane.points.len().saturating_mul(2))
        else {
            return;
        };
        projection_work = remaining;
        let (projected, segment_end) = lane.points.project_segment(vehicle.trans.pos);
        if projected.distance(vehicle.trans.pos) > lane.kind.width() * 0.5 {
            continue;
        }
        let rest = lane.points.as_slice()[segment_end..]
            .windows(2)
            .fold(projected.distance(lane.points[segment_end]), |sum, pair| {
                sum + pair[0].distance(pair[1])
            });
        if rest <= APPROACH_SAMPLE_LENGTH {
            sampled.lanes[index].seconds =
                (sampled.lanes[index].seconds + QUEUED_VEHICLE_DELAY).min(MAX_WAIT);
        }
    }
    sampled.sampled_at = Some(tick);
    map.lane_scoring = Some(sampled);
}

#[cfg(test)]
#[test]
fn p7_lane_scoring_opt_in_scores_parallel_lanes_and_restores_epoch() {
    use crate::map::{LanePatternBuilder, PathKind, Pathfinder, ProjectFilter, Traversable};
    use crate::map_dynamic::Itinerary;
    use crate::tests::TestCtx;
    use crate::transportation::{spawn_driving_vehicle, VehicleKind};
    use crate::Simulation;
    use common::saveload::{Bincode, Encoder};
    use geom::{vec3, Transform, Vec3};
    use prototypes::{GameTime, Tick};

    let mut ctx = TestCtx::new();
    let a = vec3(75.0, 200.0, 0.0);
    let b = vec3(250.0, 200.0, 0.0);
    let c = vec3(490.0, 200.0, 0.0);
    let d = vec3(680.0, 200.0, 0.0);
    ctx.build_roads(&[a, b]);
    ctx.build_roads(&[c, d]);
    let (start, finish, lanes) = {
        let mut map = ctx.g.map_mut();
        let from = map.project(b, 0.0, ProjectFilter::ALL);
        let to = map.project(c, 0.0, ProjectFilter::ALL);
        let pattern = LanePatternBuilder::new().n_lanes(2).build();
        let (end, road) = map.make_connection(from, to, None, &pattern).unwrap();
        let source = map.get(road).unwrap().other_end(end).unwrap();
        let lanes = map
            .get(road)
            .unwrap()
            .outgoing_lanes_from(source)
            .iter()
            .filter(|(_, kind)| *kind == LaneKind::Driving)
            .map(|&(lane, _)| lane)
            .collect::<Vec<_>>();
        let start = map
            .lanes()
            .values()
            .find(|lane| {
                lane.kind == LaneKind::Driving && lane.dst == source && lane.parent != road
            })
            .unwrap()
            .id;
        let finish = map
            .lanes()
            .values()
            .find(|lane| lane.kind == LaneKind::Driving && lane.src == end && lane.parent != road)
            .unwrap()
            .id;
        (start, finish, lanes)
    };
    let route = |map: &Map| {
        PathKind::Vehicle
            .path(
                map,
                Tick(2),
                Traversable::new(TraverseKind::Lane(start), TraverseDirection::Forward),
                finish,
            )
            .unwrap()
    };
    let on_group = |route: &[Traversable]| {
        route
            .iter()
            .find_map(|step| match step.kind {
                TraverseKind::Lane(lane) if lanes.contains(&lane) => Some(lane),
                _ => None,
            })
            .unwrap()
    };
    let stock = route(&ctx.g.map());
    assert!(ctx.g.map().lane_scoring_pilot().is_none());
    assert_eq!(
        ctx.g.map_mut().set_lane_scoring_pilot(Some(&[lanes[0]; 5])),
        Err(LaneScoringError::LaneCount)
    );
    assert_eq!(
        ctx.g
            .map_mut()
            .set_lane_scoring_pilot(Some(&[lanes[0], start])),
        Err(LaneScoringError::InvalidLaneGroup)
    );
    ctx.g
        .map_mut()
        .set_lane_scoring_pilot(Some(&lanes))
        .unwrap();
    let unqueued = on_group(&route(&ctx.g.map()));
    let (position, stop_at) = {
        let map = ctx.g.map();
        let lane = map.get(unqueued).unwrap();
        (
            lane.points.point_along(lane.points.length() - 16.0),
            lane.points.length() - 2.0,
        )
    };
    let vehicle = spawn_driving_vehicle(
        &mut ctx.g,
        VehicleKind::Car,
        Transform::new_dir(position, Vec3::X),
    )
    .unwrap();
    let itinerary =
        Itinerary::route_to_lane(Tick(2), position, &ctx.g.map(), unqueued, stop_at).unwrap();
    ctx.g.world.vehicles[vehicle].it = itinerary;
    let now = ctx.g.get_tick();
    *ctx.g.write::<GameTime>() = GameTime::new(Tick(now + SAMPLE_TICKS));
    lane_scoring_update(&mut ctx.g.world, &mut ctx.g.resources);
    let saved = ctx.g.map().lane_scoring_pilot().cloned().unwrap();
    assert_eq!(saved.sampled_at(), Some(SAMPLE_TICKS));
    assert_eq!(
        saved.wait_game_seconds(unqueued),
        Some(QUEUED_VEHICLE_DELAY)
    );
    let scored = route(&ctx.g.map());
    assert_ne!(on_group(&scored), unqueued);
    let resumed = Bincode::decode::<Simulation>(&Bincode::encode(&ctx.g).unwrap()).unwrap();
    assert_eq!(resumed.map().lane_scoring_pilot(), Some(&saved));
    assert_eq!(route(&resumed.map()), scored);
    ctx.g.map_mut().set_lane_scoring_pilot(None).unwrap();
    assert_eq!(route(&ctx.g.map()), stock);
}
