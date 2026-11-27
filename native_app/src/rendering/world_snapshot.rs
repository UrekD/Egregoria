use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use engine::MeshInstance;
use geom::Camera;
use geom::{LinearColor, Vec3};
use prototypes::{GameDuration, GameTime, Money, RollingStockID};
use simulation::economy::Government;
use simulation::map::{Map, MapSubscriber, RenderMap, SubscriberChunkID, UpdateType};
use simulation::map_dynamic::{Itinerary, Transit};
use simulation::multiplayer::chat::Message;
use simulation::multiplayer::MultiplayerState;
use simulation::transportation::{Location, VehicleKind};
use simulation::{AnyEntity, HumanID, Simulation, TrainID, VehicleID, WagonID};

use crate::gui::{capture_problem_markers, DemandSnapshot, ProblemMarker, ToolboxSnapshot};
use crate::sim_thread::{BatchId, Boundary};

const VIEW_INTERVAL: Duration = Duration::from_millis(16);
const HUD_INTERVAL: Duration = Duration::from_millis(100);
const MAX_DIRTY_CHUNKS: usize = 256;
const ACTOR_POOL_SIZE: usize = 4;

pub(crate) struct MapPrepInput {
    pub generation: u64,
    pub base_epoch: u64,
    pub target_epoch: u64,
    pub full_refresh: bool,
    pub captured_at: Instant,
    pub affected_chunks: BTreeMap<SubscriberChunkID, UpdateType>,
    // The chunk keys address cow-backed geometry in this pinned source.
    pub source: Arc<RenderMap>,
}

impl MapPrepInput {
    pub fn merge(&mut self, next: Self) {
        let gap = self.generation != next.generation || self.target_epoch != next.base_epoch;
        self.full_refresh |= gap || next.full_refresh;
        self.generation = next.generation;
        self.target_epoch = next.target_epoch;
        self.source = next.source;
        if !self.full_refresh {
            for (chunk, kinds) in next.affected_chunks {
                *self
                    .affected_chunks
                    .entry(chunk)
                    .or_insert(UpdateType::empty()) |= kinds;
            }
            self.full_refresh = self.affected_chunks.len() > MAX_DIRTY_CHUNKS;
        }
        if self.full_refresh {
            self.affected_chunks.clear();
        }
    }
}

pub(crate) struct MapCapture {
    generation: u64,
    epoch: u64,
    changes: [(UpdateType, MapSubscriber); 5],
    water_revision: u64,
    needs_full: bool,
}

impl MapCapture {
    pub fn new(generation: u64, map: &Map) -> Self {
        Self {
            generation,
            epoch: 0,
            changes: [
                UpdateType::Road | UpdateType::RoadSupport,
                UpdateType::Building,
                UpdateType::Terrain,
                UpdateType::Water,
                UpdateType::Land,
            ]
            .map(|kinds| {
                (
                    kinds,
                    map.subscribers.subscribe_limited(kinds, MAX_DIRTY_CHUNKS),
                )
            }),
            water_revision: map.environment().water_revision(),
            needs_full: true,
        }
    }

    pub fn capture(&mut self, map: &Map, render_map: impl FnOnce(&Map) -> RenderMap) -> Option<MapPrepInput> {
        let mut full_refresh = std::mem::take(&mut self.needs_full);
        let mut affected_chunks = BTreeMap::new();
        let water_revision = map.environment().water_revision();
        for (kinds, changes) in &mut self.changes {
            let (cleared, chunks) = changes.take_changes(MAX_DIRTY_CHUNKS);
            full_refresh |= cleared
                || (kinds.contains(UpdateType::Water)
                    && chunks.is_empty()
                    && water_revision != self.water_revision);
            for chunk in chunks {
                *affected_chunks.entry(chunk).or_insert(UpdateType::empty()) |= *kinds;
            }
        }
        self.water_revision = water_revision;
        full_refresh |= affected_chunks.len() > MAX_DIRTY_CHUNKS;
        if full_refresh {
            affected_chunks.clear();
        } else if affected_chunks.is_empty() {
            return None;
        }
        let base_epoch = self.epoch;
        self.epoch += 1;
        Some(MapPrepInput {
            generation: self.generation,
            base_epoch,
            target_epoch: self.epoch,
            full_refresh,
            captured_at: Instant::now(),
            affected_chunks,
            source: Arc::new(render_map(map)),
        })
    }

    pub fn replace_source(&mut self, map: &Map) {
        let epoch = self.epoch;
        *self = Self::new(self.generation, map);
        self.epoch = epoch;
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }
}

pub(crate) struct HudSnapshot {
    pub time: GameTime,
    pub treasury: Money,
    pub demand: DemandSnapshot,
    pub markers: Arc<[ProblemMarker]>,
    pub toolbox: ToolboxSnapshot,
    pub chat: Arc<[Message]>,
}

pub(crate) struct WorldView {
    pub stamp: ViewStamp,
    pub hud: HudSnapshot,
    pub entities: Arc<EntitySamples>,
    pub applied_batch: Option<BatchId>,
    pub command_sequence: u64,
    pub paused: bool,
    pub transit_stops: Arc<[Vec3]>,
    pub transit_revision: u64,
    tick: u64,
}

#[derive(Default)]
pub(crate) struct LatestViews {
    previous: Option<Arc<WorldView>>,
    latest: Option<Arc<WorldView>>,
}

impl LatestViews {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn latest(&self) -> Option<&Arc<WorldView>> {
        self.latest.as_ref()
    }

    pub fn publish(&mut self, view: Arc<WorldView>) {
        if let Some(latest) = self.latest.take() {
            if !Arc::ptr_eq(&latest.entities, &view.entities) {
                self.previous = Some(latest);
            }
        }
        self.latest = Some(view);
    }

    pub fn previous_for(&self, view: &WorldView) -> Option<&WorldView> {
        let previous = self.previous.as_deref()?;
        let latest = self.latest.as_deref()?;
        (view.stamp.view_seq == latest.stamp.view_seq
            && view.stamp.generation == latest.stamp.generation)
            .then_some(previous)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ViewRequest {
    pub camera: Option<Camera>,
}

pub(crate) struct WorldUpdate {
    pub view: Arc<WorldView>,
    pub map: Option<MapPrepInput>,
}

#[derive(Default)]
pub(crate) struct SnapshotSource {
    map: Option<MapCapture>,
    pending_map: Option<MapPrepInput>,
    latest: Option<Arc<WorldView>>,
    pending: bool,
    last_markers: Option<(Instant, Camera)>,
    last_hud: Option<Instant>,
    last_view: Option<Instant>,
    actor_pool: Vec<Arc<EntitySamples>>,
}

#[derive(Default)]
pub(crate) struct CaptureWork {
    pub actor_scan: bool,
    pub buffer_grows: u64,
    pub actor_pool_bytes: usize,
}

fn same_camera(a: &Camera, b: &Camera) -> bool {
    a.proj_cache.as_ref() == b.proj_cache.as_ref()
        && a.viewport_w == b.viewport_w
        && a.viewport_h == b.viewport_h
}

impl SnapshotSource {
    pub(crate) fn capture_map(&mut self, sim: &Simulation, generation: u64, replaced: bool) -> u64 {
        let map = sim.map();
        if self
            .map
            .as_ref()
            .is_none_or(|capture| capture.generation() != generation)
        {
            *self = Self::default();
            self.map = Some(MapCapture::new(generation, &map));
        }
        let capture = self.map.as_mut().unwrap();
        if replaced {
            capture.replace_source(&map);
        }
        if let Some(input) = capture.capture(&map, |map| {
            map.render_snapshot().with_parking_pavement(sim.parking_pavement_snapshot())
        }) {
            if let Some(pending) = &mut self.pending_map {
                pending.merge(input);
            } else {
                self.pending_map = Some(input);
            }
        }
        capture.epoch()
    }

    pub(crate) fn capture_view(
        &mut self,
        sim: &Simulation,
        boundary: Boundary,
        request: ViewRequest,
        paused: bool,
        force: bool,
    ) -> CaptureWork {
        let epoch = self.map.as_ref().unwrap().epoch();
        let now = Instant::now();
        let latest = self.latest.as_deref();
        let command_sequence = sim
            .read::<simulation::world_command::CommandOutcomes>()
            .sequence;
        let same_map = latest.is_some_and(|view| view.stamp.map_epoch == epoch);
        let same_commands = latest.is_some_and(|view| {
            view.command_sequence == command_sequence && view.applied_batch == boundary.last_batch
        });
        let same_boundary = same_map
            && same_commands
            && latest.is_some_and(|view| {
                view.stamp.completed_step == boundary.completed_step && view.tick == boundary.tick
            });

        let camera_changed = match (request.camera.as_ref(), self.last_markers.as_ref()) {
            (Some(camera), Some((_, old))) => !same_camera(camera, old),
            (None, None) => false,
            _ => true,
        };
        let markers_due = if request.camera.is_some() {
            !same_map
                || self.last_markers.is_none_or(|(at, _)| {
                    (camera_changed || !same_boundary) && (force || now - at >= HUD_INTERVAL)
                })
        } else {
            self.last_markers.is_some()
        };
        let hud_due = force
            || !same_map
            || !same_commands
            || (!same_boundary && self.last_hud.is_none_or(|at| now - at >= HUD_INTERVAL));
        if !force && same_map && same_commands && latest.is_some_and(|view| view.paused == paused) {
            let recent = self.last_view.is_some_and(|at| now - at < VIEW_INTERVAL);
            if recent || (same_boundary && !markers_due && !hud_due) {
                return CaptureWork::default();
            }
        }
        let mut work = CaptureWork::default();
        let transit = sim.read::<Transit>();
        let same_actors =
            same_boundary && latest.is_some_and(|view| view.transit_revision == transit.revision);
        let entities = if same_actors {
            latest.unwrap().entities.clone()
        } else {
            let (entities, buffer_grows) =
                Self::capture_actors(&mut self.actor_pool, sim, &transit);
            work.actor_scan = true;
            work.buffer_grows = buffer_grows;
            work.actor_pool_bytes = self
                .actor_pool
                .iter()
                .map(|slot| slot.capacity_bytes())
                .sum();
            entities
        };
        let transit_stops = latest
            .filter(|view| same_map && view.transit_revision == transit.revision)
            .map_or_else(
                || transit.stops.values().map(|stop| stop.walk_pos).collect(),
                |view| view.transit_stops.clone(),
            );

        let markers = if markers_due {
            self.last_markers = request.camera.map(|camera| (now, camera));
            request.camera.map_or_else(Arc::default, |camera| {
                capture_problem_markers(sim, &camera, boundary.generation, epoch).into()
            })
        } else {
            latest.map_or_else(Arc::default, |view| view.hud.markers.clone())
        };
        let time = *sim.read::<GameTime>();
        let (treasury, demand, toolbox, chat) = if hud_due {
            self.last_hud = Some(now);
            let since = time.instant() - GameDuration::from_minutes(5);
            let chat = sim.read::<MultiplayerState>();
            (
                sim.read::<Government>().money,
                DemandSnapshot::capture(sim),
                ToolboxSnapshot::capture(sim),
                chat.chat.messages_since(since).take(30).cloned().collect(),
            )
        } else {
            let hud = &latest.unwrap().hud;
            (hud.treasury, hud.demand, hud.toolbox, hud.chat.clone())
        };

        self.latest = Some(Arc::new(WorldView {
            stamp: ViewStamp {
                generation: boundary.generation,
                view_seq: latest.map_or(1, |previous| previous.stamp.view_seq + 1),
                completed_step: boundary.completed_step,
                sample_time: if same_actors {
                    latest.unwrap().stamp.sample_time
                } else {
                    now
                },
                map_epoch: epoch,
            },
            hud: HudSnapshot {
                time,
                treasury,
                demand,
                markers,
                toolbox,
                chat,
            },
            entities,
            applied_batch: boundary.last_batch,
            command_sequence,
            paused,
            transit_stops,
            transit_revision: transit.revision,
            tick: boundary.tick,
        }));
        self.last_view = Some(now);
        self.pending = true;
        work
    }

    fn capture_actors(
        pool: &mut Vec<Arc<EntitySamples>>,
        sim: &Simulation,
        transit: &Transit,
    ) -> (Arc<EntitySamples>, u64) {
        for slot in pool.iter_mut() {
            if let Some(samples) = Arc::get_mut(slot) {
                let grows = samples.capture(sim, transit);
                return (slot.clone(), grows);
            }
        }
        let mut samples = EntitySamples::default();
        let grows = samples.capture(sim, transit);
        let samples = Arc::new(samples);
        if pool.len() < ACTOR_POOL_SIZE {
            pool.push(samples.clone());
        }
        (samples, grows)
    }

    pub(crate) fn take(&mut self) -> Option<WorldUpdate> {
        let view = self.latest.as_ref()?;
        if !self.pending || self.map.as_ref()?.epoch() != view.stamp.map_epoch {
            return None;
        }
        self.pending = false;
        Some(WorldUpdate {
            view: view.clone(),
            map: self.pending_map.take(),
        })
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub(crate) enum ActorId {
    Human(HumanID),
    Vehicle(VehicleID),
    Wagon(WagonID),
}

impl ActorId {
    pub(crate) fn entity(self) -> AnyEntity {
        match self {
            Self::Human(id) => id.into(),
            Self::Vehicle(id) => id.into(),
            Self::Wagon(id) => id.into(),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct SelectionView {
    pub selected: Option<AnyEntity>,
    pub followed: Option<AnyEntity>,
}

impl SelectionView {
    pub fn contains(self, actor: &ActorSample) -> bool {
        let part = Some(actor.id.entity());
        let entity = Some(actor.entity());
        self.selected == part
            || self.followed == part
            || self.selected == entity
            || self.followed == entity
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum ActorKind {
    Pedestrian,
    Car,
    Bus,
    Truck,
    RollingStock(RollingStockID),
}

pub(crate) struct ActorSample {
    pub id: ActorId,
    pub kind: ActorKind,
    pub pose: MeshInstance,
    pub leader: Option<TrainID>,
    parked: bool,
}

impl ActorSample {
    pub(crate) fn entity(&self) -> AnyEntity {
        self.leader
            .map_or_else(|| self.id.entity(), AnyEntity::from)
    }

    pub(crate) fn pose_from(&self, previous: &Self, steps: u64, fraction: f32) -> MeshInstance {
        let max_step = if self.kind == ActorKind::Pedestrian {
            1.0
        } else {
            5.0
        };
        let max_distance = 1.0 + max_step * steps.min(12) as f32;
        let delta = self.pose.pos - previous.pose.pos;
        // A chord can cut the corner of a lane or rail. Require both headings
        // to align with it within 5 degrees, or 1 degree for wider sample gaps.
        let straight_dot = if steps > 8 { 0.999_847_7 } else { 0.996_194_7 };
        if self.id != previous.id
            || self.kind != previous.kind
            || self.parked != previous.parked
            || self.leader != previous.leader
            || delta.mag2() > max_distance * max_distance
        {
            return self.pose;
        }
        let delta = if self.kind == ActorKind::Pedestrian {
            delta.xy().z0()
        } else {
            delta
        };
        let (Some(travel), Some(old_heading), Some(new_heading)) = (
            delta.try_normalize(),
            previous.pose.dir.try_normalize(),
            self.pose.dir.try_normalize(),
        ) else {
            return self.pose;
        };
        if old_heading.dot(new_heading) < straight_dot
            || travel.dot(old_heading) < straight_dot
            || travel.dot(new_heading) < straight_dot
        {
            return self.pose;
        }
        MeshInstance {
            pos: previous.pose.pos.lerp(self.pose.pos, fraction),
            dir: previous.pose.dir.lerp(self.pose.dir, fraction),
            tint: self.pose.tint,
        }
    }
}

#[derive(Default)]
pub(crate) struct EntitySamples {
    pub actors: Vec<ActorSample>,
    pub reroutes: Vec<(AnyEntity, Vec3, u16)>,
}

impl EntitySamples {
    fn capacity_bytes(&self) -> usize {
        self.actors.capacity() * std::mem::size_of::<ActorSample>()
            + self.reroutes.capacity() * std::mem::size_of::<(AnyEntity, Vec3, u16)>()
    }

    fn record_reroute(&mut self, entity: AnyEntity, pos: Vec3, itinerary: &Itinerary) {
        if let Some(wait) = itinerary.is_wait_for_reroute().filter(|&wait| wait != 0) {
            self.reroutes.push((entity, pos, wait));
        }
    }

    fn capture(&mut self, sim: &Simulation, transit: &Transit) -> u64 {
        let world = sim.world();
        let actor_capacity = self.actors.capacity();
        let reroute_capacity = self.reroutes.capacity();
        self.actors.clear();
        self.reroutes.clear();
        self.actors
            .reserve(world.vehicles.len() + world.wagons.len());
        for (id, p) in world.humans.iter() {
            self.record_reroute(id.into(), p.trans.pos, &p.it);
            if matches!(p.location, Location::Outside) {
                self.actors.push(ActorSample {
                    id: ActorId::Human(id),
                    kind: ActorKind::Pedestrian,
                    pose: MeshInstance {
                        pos: p.trans.pos.up(0.5 + 0.4 * p.pedestrian.walk_anim.cos()),
                        dir: p.trans.dir.xy().z0(),
                        tint: LinearColor::WHITE,
                    },
                    leader: None,
                    parked: false,
                });
            }
        }
        for (id, v) in world.vehicles.iter() {
            if v.vehicle.kind == VehicleKind::Car
                && v.vehicle.state.is_parked()
                && v.it.is_property_stored()
                && sim.car_stored_inside_garage(id)
            {
                continue;
            }
            self.record_reroute(id.into(), v.trans.pos, &v.it);
            let (kind, tint) = match v.vehicle.kind {
                VehicleKind::Car => (ActorKind::Car, v.vehicle.tint.into()),
                VehicleKind::Truck => (ActorKind::Truck, v.vehicle.tint.into()),
                VehicleKind::Bus => (
                    ActorKind::Bus,
                    transit
                        .runs
                        .get(&id)
                        .and_then(|run| transit.lines.get(run.line))
                        .map_or_else(|| v.vehicle.tint.into(), |line| line.color.into()),
                ),
            };
            self.actors.push(ActorSample {
                id: ActorId::Vehicle(id),
                kind,
                pose: MeshInstance {
                    pos: v.trans.pos,
                    dir: v.trans.dir,
                    tint,
                },
                leader: None,
                parked: v.vehicle.state.is_parked(),
            });
        }
        for (id, wagon) in world.wagons.iter() {
            self.actors.push(ActorSample {
                id: ActorId::Wagon(id),
                kind: ActorKind::RollingStock(wagon.wagon.rolling_stock),
                pose: MeshInstance {
                    pos: wagon.trans.pos,
                    dir: wagon.trans.dir,
                    tint: LinearColor::WHITE,
                },
                leader: Some(wagon.itfollower.leader),
                parked: false,
            });
        }
        for (id, train) in world.trains.iter() {
            self.record_reroute(id.into(), train.trans.pos, &train.it);
        }
        u64::from(self.actors.capacity() > actor_capacity)
            + u64::from(self.reroutes.capacity() > reroute_capacity)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ViewStamp {
    pub generation: u64,
    pub view_seq: u64,
    pub completed_step: u64,
    pub sample_time: Instant,
    pub map_epoch: u64,
}

impl ViewStamp {
    pub(crate) fn interpolation_from(self, previous: Self, now: Instant, paused: bool) -> f32 {
        if paused
            || self.generation != previous.generation
            || self.map_epoch != previous.map_epoch
            || self.view_seq <= previous.view_seq
            || self.completed_step <= previous.completed_step
            || self.sample_time <= previous.sample_time
        {
            return 1.0;
        }

        let interval = self.sample_time.duration_since(previous.sample_time);
        let elapsed = now.saturating_duration_since(self.sample_time);
        (elapsed.as_secs_f32() / interval.as_secs_f32()).min(1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn scale_t2_map_journal_retains_repeated_edits_and_pins_a_full_source() {
        use simulation::map::Environment;

        let mut map = Map::empty();
        assert!(map.install_environment(Environment::new(1, 1)));
        let mut capture = MapCapture::new(4, &map);
        let initial = capture.capture(&map, Map::render_snapshot).unwrap();
        assert!(initial.full_refresh);
        assert_eq!(initial.source.environment().size(), (1, 1));
        assert!(capture.capture(&map, Map::render_snapshot).is_none());

        let bucket = SubscriberChunkID::new_i16(0, 0);
        map.subscribers.dispatch_chunk(UpdateType::Road, bucket);
        let mut pending = capture.capture(&map, Map::render_snapshot).unwrap();
        map.subscribers.dispatch_chunk(UpdateType::Building, bucket);
        pending.merge(capture.capture(&map, Map::render_snapshot).unwrap());
        assert!(!pending.full_refresh);
        assert_eq!((pending.base_epoch, pending.target_epoch), (1, 3));
        assert!(pending.affected_chunks[&bucket].contains(UpdateType::Road | UpdateType::Building));

        for x in 0..=MAX_DIRTY_CHUNKS {
            map.subscribers
                .dispatch_chunk(UpdateType::Terrain, SubscriberChunkID::new_i16(x as i16, 0));
        }
        pending.merge(capture.capture(&map, Map::render_snapshot).unwrap());
        assert!(pending.full_refresh);
        assert!(pending.affected_chunks.is_empty());

        let replacement = Map::empty();
        capture.replace_source(&replacement);
        let latest = capture.capture(&replacement).unwrap();
        assert!(latest.full_refresh);
        assert_eq!(
            (latest.generation, latest.base_epoch, latest.target_epoch),
            (4, 4, 5)
        );
        assert_eq!(latest.source.environment().size(), (0, 0));
        assert_eq!(initial.source.environment().size(), (1, 1));
    }

    #[test]
    fn scale_t2_pinned_geometry_survives_authoritative_replacement() {
        use geom::vec2;
        use scenarios::builders::grid_city;
        use scenarios::ScenarioCtx;
        use simulation::SimulationOptions;

        scenarios::ensure_repo_root_cwd();
        crate::init::init();
        let mut scenario = ScenarioCtx::new(SimulationOptions {
            terrain_size: 2,
            save_replay: false,
            ..SimulationOptions::LEGACY
        });
        let old = scenario.sim.map().render_snapshot();
        let old_roads = old.roads().len();
        let old_objects = old.spatial_map().objects().count();
        grid_city(&mut scenario, vec2(300.0, 300.0), 2, 2, 120.0);
        let new = scenario.sim.map().render_snapshot();
        assert_eq!(old.roads().len(), old_roads);
        assert_eq!(old.spatial_map().objects().count(), old_objects);
        assert!(new.roads().len() > old_roads);
        assert!(new.spatial_map().objects().count() > old_objects);
    }

    #[test]
    fn client_max_turn_with_skipped_ticks_snaps_to_the_lane() {
        let direction = geom::vec3(0.5, 0.8660254, 0.0);
        let sample = |pos, dir| ActorSample {
            id: ActorId::Vehicle(VehicleID::default()),
            kind: ActorKind::Car,
            pose: MeshInstance {
                pos,
                dir,
                tint: LinearColor::WHITE,
            },
            leader: None,
            parked: false,
        };
        let previous = sample(-Vec3::X * 20.0, Vec3::X);
        let latest = sample(direction * 20.0, direction);
        assert_eq!(latest.pose_from(&previous, 12, 0.5).pos, latest.pose.pos);
        let straight = sample(Vec3::X * 20.0, Vec3::X);
        assert_eq!(straight.pose_from(&previous, 12, 0.5).pos, Vec3::ZERO);
    }

    #[test]
    fn client_max_paused_redraw_reuses_actors_even_when_forced() {
        use geom::Transform;
        use scenarios::ScenarioCtx;
        use simulation::transportation::spawn_driving_vehicle;
        use simulation::SimulationOptions;

        scenarios::ensure_repo_root_cwd();
        crate::init::init();
        let mut scenario = ScenarioCtx::new(SimulationOptions {
            terrain_size: 1,
            ..SimulationOptions::LEGACY
        });
        let car = spawn_driving_vehicle(
            &mut scenario.sim,
            VehicleKind::Car,
            Transform::new(Vec3::X * 80.0),
        )
        .unwrap();
        let mut source = SnapshotSource::default();
        let boundary = Boundary {
            generation: 1,
            completed_step: 10,
            last_batch: None,
            tick: scenario.sim.get_tick(),
        };
        source.capture_map(&scenario.sim, boundary.generation, false);
        let mut request = ViewRequest { camera: None };
        source.capture_view(&scenario.sim, boundary, request, true, true);
        let first = source.take().unwrap().view;
        assert_eq!(first.entities.actors.len(), 1);

        request.camera = Some(Camera::new(Vec3::ZERO, 800.0, 600.0));
        source.capture_view(&scenario.sim, boundary, request, true, true);
        let second = source.take().unwrap().view;
        assert_eq!(
            first.entities.actors.as_ptr(),
            second.entities.actors.as_ptr()
        );

        scenario.sim.world_mut_unchecked().vehicles[car].trans.pos += Vec3::X;
        source.capture_view(
            &scenario.sim,
            Boundary {
                completed_step: boundary.completed_step + 1,
                ..boundary
            },
            request,
            true,
            true,
        );
        let advanced = source.take().unwrap().view;
        assert_ne!(
            second.entities.actors[0].pose.pos,
            advanced.entities.actors[0].pose.pos
        );
    }

    #[test]
    fn scale_t2_actor_uses_full_id_and_snaps_on_jump() {
        use slotmapd::SlotMap;

        let mut ids = SlotMap::<VehicleID, ()>::with_key();
        let old_id = ids.insert(());
        ids.remove(old_id);
        let new_id = ids.insert(());
        let sample = |id, x| ActorSample {
            id: ActorId::Vehicle(id),
            kind: ActorKind::Car,
            pose: MeshInstance {
                pos: Vec3::X * x,
                dir: Vec3::X,
                tint: LinearColor::WHITE,
            },
            leader: None,
            parked: false,
        };
        let previous = sample(old_id, 1.0);
        assert_eq!(
            sample(old_id, 3.0).pose_from(&previous, 1, 0.5).pos,
            Vec3::X * 2.0
        );
        assert_eq!(
            sample(new_id, 3.0).pose_from(&previous, 1, 0.5).pos,
            Vec3::X * 3.0
        );
        assert_eq!(
            sample(old_id, 50.0).pose_from(&previous, 1, 0.5).pos,
            Vec3::X * 50.0
        );
    }

    #[test]
    fn scale_t2_selected_train_covers_wagons() {
        let leader = TrainID::default();
        let wagon = ActorSample {
            id: ActorId::Wagon(WagonID::default()),
            kind: ActorKind::RollingStock(RollingStockID::new("freight-wagon")),
            pose: MeshInstance {
                pos: Vec3::ZERO,
                dir: Vec3::X,
                tint: LinearColor::WHITE,
            },
            leader: Some(leader),
            parked: false,
        };
        assert!(SelectionView {
            selected: Some(leader.into()),
            followed: None
        }
        .contains(&wagon));
        assert!(SelectionView {
            selected: None,
            followed: Some(leader.into())
        }
        .contains(&wagon));
    }

    #[test]
    fn scale_t2_interpolates_only_continuous_sample_boundaries() {
        let start = Instant::now();
        let previous = ViewStamp {
            generation: 1,
            view_seq: 7,
            completed_step: 10,
            sample_time: start,
            map_epoch: 3,
        };
        let latest = ViewStamp {
            view_seq: 8,
            completed_step: 14,
            sample_time: start + Duration::from_millis(40),
            ..previous
        };
        let between = latest.sample_time + Duration::from_millis(10);
        assert_eq!(latest.interpolation_from(previous, between, false), 0.25);
        for discontinuous in [
            ViewStamp {
                generation: 2,
                ..latest
            },
            ViewStamp {
                map_epoch: 4,
                ..latest
            },
            ViewStamp {
                completed_step: 10,
                ..latest
            },
            ViewStamp {
                view_seq: 7,
                ..latest
            },
        ] {
            assert_eq!(
                discontinuous.interpolation_from(previous, between, false),
                1.0
            );
        }
        assert_eq!(latest.interpolation_from(previous, between, true), 1.0);
        assert_eq!(
            latest.interpolation_from(previous, between + Duration::from_secs(1), false),
            1.0
        );
    }
}
