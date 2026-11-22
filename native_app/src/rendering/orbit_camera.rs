#![allow(clippy::redundant_closure_call)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use common::saveload::{Encoder, JSONPretty, SlotCamera, JSON};
use engine::{Context, Tesselator};
use geom::{Camera, Plane, Radians, Vec2, Vec3, AABB};
use simulation::map::pathfinding_crate::num_traits::Pow;
use simulation::map::{BuildingKind, Map};

use crate::gui::windows::settings::Settings;
use crate::inputmap::{InputAction, InputMap};

const CAMERA_SAVE_INTERVAL: Duration = Duration::from_secs(2);
const CAMERA_SETTLE_TIME: Duration = Duration::from_millis(250);
const CAMERA_MAX_RETRY_DELAY: Duration = Duration::from_secs(60);
const CAMERA_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(250);
static WARNED_CAMERA_SAVE: AtomicBool = AtomicBool::new(false);

/// How far past the map edge the orbit centre may go.
const EDGE_MARGIN: f32 = 100.0;

/// Area the orbit centre may move in: the whole map at every zoom level, so the
/// player can look at the map edge.
fn movement_bounds(map: AABB) -> AABB {
    map.expand(EDGE_MARGIN)
}

fn same_camera(a: &Camera, b: &Camera) -> bool {
    a.pos == b.pos
        && a.yaw == b.yaw
        && a.pitch == b.pitch
        && a.dist == b.dist
        && a.fovy == b.fovy
        && a.viewport_w == b.viewport_w
        && a.viewport_h == b.viewport_h
}

fn save_camera_file(camera: &Camera) -> bool {
    match JSONPretty::try_save(camera, "camera3D") {
        Ok(()) => true,
        Err(error) => {
            if WARNED_CAMERA_SAVE.swap(true, Ordering::Relaxed) {
                log::debug!("failed saving camera3D: {error}");
            } else {
                log::warn!("failed saving camera3D: {error}");
            }
            false
        }
    }
}

fn spawn_camera_save(camera: Camera) -> Receiver<bool> {
    let (done, result) = mpsc::channel();
    rayon::spawn(move || {
        let _ = done.send(save_camera_file(&camera));
    });
    result
}

#[derive(Default)]
struct CameraSave {
    enabled: bool,
    observed: Option<(Camera, Instant)>,
    saved: Option<Camera>,
    last_attempt: Option<Instant>,
    in_flight: Option<(Camera, Receiver<bool>)>,
    retry: Option<(Instant, Duration)>,
    shutdown_deadline: Option<Instant>,
}

impl CameraSave {
    fn update(
        &mut self,
        camera: Camera,
        now: Instant,
        spawn: impl FnOnce(Camera) -> Receiver<bool>,
    ) {
        if self.shutdown_deadline.is_some() {
            return;
        }
        self.poll_write(now);

        let changed_at = match self.observed {
            Some((previous, at)) if same_camera(&camera, &previous) => at,
            _ => now,
        };
        self.observed = Some((camera, changed_at));
        let settled_at = changed_at + CAMERA_SETTLE_TIME;
        let final_save_due =
            now >= settled_at && self.last_attempt.is_none_or(|at| at < settled_at);
        let interval_elapsed = self
            .last_attempt
            .is_none_or(|at| now.duration_since(at) >= CAMERA_SAVE_INTERVAL);
        if self.in_flight.is_none()
            && self.needs_save(&camera)
            && self.retry.is_none_or(|(retry_at, _)| now >= retry_at)
            && (final_save_due || interval_elapsed)
        {
            self.start_write(camera, now, spawn);
        }
    }

    fn poll_shutdown(
        &mut self,
        camera: Camera,
        now: Instant,
        spawn: impl FnOnce(Camera) -> Receiver<bool>,
    ) -> bool {
        if !self.enabled {
            return true;
        }
        let deadline = *self
            .shutdown_deadline
            .get_or_insert(now + CAMERA_SHUTDOWN_TIMEOUT);
        if now >= deadline {
            return true;
        }
        self.poll_write(now);
        if self.in_flight.is_some() {
            return false;
        }
        if !self.needs_save(&camera) || self.retry.is_some_and(|(retry_at, _)| now < retry_at) {
            return true;
        }
        self.start_write(camera, now, spawn);
        false
    }

    fn needs_save(&self, camera: &Camera) -> bool {
        self.saved.is_none_or(|saved| !same_camera(camera, &saved))
    }

    fn start_write(
        &mut self,
        camera: Camera,
        now: Instant,
        spawn: impl FnOnce(Camera) -> Receiver<bool>,
    ) {
        self.last_attempt = Some(now);
        self.in_flight = Some((camera, spawn(camera)));
    }

    fn poll_write(&mut self, now: Instant) {
        let Some((submitted, result)) = &self.in_flight else {
            return;
        };
        match result.try_recv() {
            Ok(true) => {
                self.saved = Some(*submitted);
                self.retry = None;
            }
            Ok(false) | Err(TryRecvError::Disconnected) => {
                let delay = self.retry.map_or(CAMERA_SAVE_INTERVAL, |(_, previous)| {
                    (previous * 2).min(CAMERA_MAX_RETRY_DELAY)
                });
                self.retry = Some((now + delay, delay));
            }
            Err(TryRecvError::Empty) => return,
        }
        self.in_flight = None;
    }
}

/// CameraHandler3D is the camera handler for the 3D view
/// It controls the camera using an orbit view
pub struct OrbitCamera {
    pub camera: Camera,
    pub lastscreenpos: Vec2,
    pub last_pos: Option<Vec2>,
    pub targetpos: Vec3,
    pub targetyaw: Radians,
    pub targetpitch: Radians,
    pub targetdist: f32,
    pub maxdist: f32,
    save: CameraSave,
}

impl OrbitCamera {
    pub fn update(&mut self, ctx: &mut Context) {
        self.camera.update();
        ctx.gfx.set_camera(self.camera);
        let params = ctx.gfx.render_params.value_mut();
        params.cam_pos = self.camera.eye();
        params.cam_dir = -self.camera.dir();
        if self.save.enabled {
            self.save
                .update(self.camera, Instant::now(), spawn_camera_save);
        }
    }

    pub(crate) fn poll_save_on_exit(&mut self, now: Instant) -> bool {
        self.save.poll_shutdown(self.camera, now, spawn_camera_save)
    }

    pub fn height(&self) -> f32 {
        self.camera.offset().z
    }

    pub fn cull_tess(&self, tess: &mut Tesselator) {
        let p = self.camera.pos;
        tess.cull_rect = Some(AABB::centered(p.xy(), Vec2::splat(4000.0)));
        tess.zoom = 1000.0 / self.height().max(1.0);
    }

    pub fn follow(&mut self, p: Vec3) {
        self.camera.pos = p;
        self.targetpos = p;
    }

    pub fn resize(&mut self, ctx: &mut Context, width: f32, height: f32) {
        self.camera.set_viewport(width, height);
        self.update(ctx);
    }

    pub fn unproject(&self, pos: Vec2, height: impl Fn(Vec2) -> Option<f32>) -> Option<Vec3> {
        let r = self.camera.unproj_ray(pos)?;

        let p = Plane { n: Vec3::Z, o: 0.0 };

        let p = r.intersection_plane(&p)?.xy();
        Some(p.z(height(p)?))
    }

    /// Project a 3D point to the screen
    /// Returns the screen position [0, 1] and the depth
    pub fn project(&self, pos: Vec3) -> (Vec2, f32) {
        self.camera.project(pos)
    }

    pub fn load(viewport: (u32, u32)) -> Self {
        let saved = JSON::load("camera3D").ok();
        let camera =
            saved.unwrap_or_else(|| Self::default_camera(viewport.0 as f32, viewport.1 as f32));
        let mut orbit = Self::from_camera(camera);
        orbit.save.saved = saved;
        orbit
    }

    pub(crate) fn slot_camera(&self) -> SlotCamera {
        SlotCamera {
            position: self.camera.pos.into(),
            yaw: self.camera.yaw.0,
            pitch: self.camera.pitch.0,
            distance: self.camera.dist,
        }
    }

    pub(crate) fn reset_for_world(&mut self, map: &Map, saved: Option<SlotCamera>) {
        let bounds = map.environment.bounds();
        let valid = saved.filter(|saved| {
            let pos = Vec3::from(saved.position);
            pos.is_finite()
                && movement_bounds(bounds).contains(pos.xy())
                && saved.yaw.is_finite()
                && (0.0..Radians::HALFPI.0).contains(&saved.pitch)
                && (5.0..=self.maxdist).contains(&saved.distance)
        });
        let default_position = || {
            let center = bounds.center();
            starter_view(map)
                .unwrap_or_else(|| center.z(map.environment.height(center).unwrap_or_default()))
        };
        let position = valid.map_or_else(default_position, |saved| Vec3::from(saved.position));
        let mut camera = Camera::new(position, self.camera.viewport_w, self.camera.viewport_h);
        if let Some(saved) = valid {
            camera.yaw = Radians(saved.yaw);
            camera.pitch = Radians(saved.pitch);
            camera.dist = saved.distance;
        }
        camera.update();
        *self = Self {
            save: std::mem::take(&mut self.save),
            ..Self::from_camera(camera)
        };
    }

    fn default_camera(width: f32, height: f32) -> Camera {
        Camera::new(Vec3::new(6511.0, 9590.0, 0.0), width, height)
    }

    pub(crate) fn from_camera(camera: Camera) -> Self {
        Self {
            camera,
            lastscreenpos: Default::default(),
            last_pos: Default::default(),
            targetpos: camera.pos,
            targetyaw: camera.yaw,
            targetpitch: camera.pitch,
            targetdist: camera.dist,
            maxdist: 1500.0,
            save: CameraSave::default(),
        }
    }

    pub fn camera_movement(
        &mut self,
        ctx: &mut Context,
        delta: f32,
        inps: &InputMap,
        settings: &Settings,
        map_bounds: AABB,
        height: impl Fn(Vec2) -> Option<f32>,
    ) {
        // edge cases (NaN, inf, etc)
        if !self.camera.pos.is_finite() {
            self.camera.pos = Vec3::ZERO;
        }
        if !self.camera.dist.is_finite() {
            self.camera.dist = self.maxdist;
        }
        if !self.camera.yaw.0.is_finite() {
            self.camera.yaw.0 = 0.3;
        }
        if !self.camera.pitch.0.is_finite() {
            self.camera.pitch.0 = 0.3;
        }

        // prepare useful variables
        let delta = delta.min(0.1);
        let off = self.camera.offset();
        let d = off.xy().try_normalize().unwrap_or(Vec2::ZERO) * self.camera.dist;
        let screenpos = ctx.input.mouse.screen;

        // handle inputs
        if inps.act.contains(&InputAction::GoRight) {
            self.targetpos += -delta * d.perpendicular().z0();
        }
        if inps.act.contains(&InputAction::GoLeft) {
            self.targetpos += delta * d.perpendicular().z0();
        }
        if inps.act.contains(&InputAction::GoForward) {
            self.targetpos += -delta * d.z0();
        }
        if inps.act.contains(&InputAction::GoBackward) {
            self.targetpos += delta * d.z0();
        }

        if inps.act.contains(&InputAction::Zoom) {
            self.targetdist *= (1.0f32 / 1.05).pow(0.5 + 0.1 * inps.wheel.abs());
        }

        if inps.act.contains(&InputAction::Dezoom) {
            self.targetdist *= 1.05f32.pow(0.5 + 0.1 * inps.wheel.abs());
        }

        if settings.camera_border_move {
            if screenpos.x < 2.0 {
                self.targetpos += delta * d.perpendicular().z0();
            }
            if screenpos.x > self.camera.viewport_w - 2.0 {
                self.targetpos += -delta * d.perpendicular().z0();
            }
            if screenpos.y < 2.0 {
                self.targetpos += -delta * d.z0();
            }
            if screenpos.y > self.camera.viewport_h - 2.0 {
                self.targetpos += delta * d.z0();
            }
        }

        let delta_mouse = screenpos - self.lastscreenpos;
        self.lastscreenpos = screenpos;

        let unprojected = self.unproject(screenpos, |_| Some(0.0));

        if inps.act.contains(&InputAction::CameraRotate) {
            self.targetyaw -= Radians(delta_mouse.x / 100.0);
            self.targetpitch += Radians(delta_mouse.y / 100.0);
            self.targetpitch = self
                .targetpitch
                .min(Radians::HALFPI - Radians(0.01))
                .max(Radians(0.01));
        } else if inps.act.contains(&InputAction::CameraMove) {
            if let Some((last_pos, unprojected)) = self.last_pos.zip(unprojected) {
                self.targetpos += (last_pos - unprojected.xy())
                    .cap_magnitude(50.0 * delta * self.camera.eye().z)
                    .z0();
            }
            self.last_pos = unprojected.map(Vec3::xy);
        }

        // make sure things are in reasonable bounds
        self.targetdist = self.targetdist.clamp(5.0, self.maxdist);
        self.camera.fovy = settings.camera_fov.clamp(1.0, 179.0);
        let bounds = movement_bounds(map_bounds);
        self.targetpos.x = self.targetpos.x.clamp(bounds.ll.x, bounds.ur.x);
        self.targetpos.y = self.targetpos.y.clamp(bounds.ll.y, bounds.ur.y);
        self.targetpos.z = self.targetpos.z.clamp(0.0, 100000.0);

        // smooth camera movement
        if settings.camera_smooth {
            macro_rules! lerpp {
                ($a:expr, $b:expr, $amt:expr, $c:expr) => {
                    let coeff = delta * settings.camera_smooth_tightness * $amt;
                    let diff = $b - $a;
                    if coeff.abs() > 1.0 || $c(diff) < 0.002 {
                        $a = $b;
                    } else {
                        $a += diff * coeff;
                    }
                };
            }

            lerpp!(self.camera.pos, self.targetpos, 8.0, |v: Vec3| v.mag2());
            lerpp!(self.camera.yaw, self.targetyaw, 16.0, |x: Radians| x
                .0
                .abs());
            lerpp!(self.camera.pitch, self.targetpitch, 8.0, |x: Radians| x
                .0
                .abs());
            lerpp!(self.camera.dist, self.targetdist, 8.0, |x: f32| x.abs());
            if (self.targetdist / self.camera.dist - 1.0).abs() < 0.002 {
                self.camera.dist = self.targetdist;
            }
        } else {
            self.camera.pos = self.targetpos;
            self.camera.yaw = self.targetyaw;
            self.camera.pitch = self.targetpitch;
            self.camera.dist = self.targetdist;
        }

        // update orbit center to be height aware
        self.camera.pos.z = height(self.camera.pos.xy())
            .unwrap_or(self.camera.pos.z)
            .clamp(0.0, 100000.0);

        // update camera
        self.save.enabled = true;
        self.update(ctx);
    }
}

fn starter_view(map: &Map) -> Option<Vec3> {
    let station = map
        .buildings()
        .values()
        .find(|building| matches!(building.kind, BuildingKind::RailFreightStation(_)))?;
    let center = station.obb.center();
    // Leave room between the station and the center of the opening view for construction.
    let preferred = center + Vec2::X * 256.0;
    (-12..=12)
        .flat_map(|x| (-12..=12).map(move |y| (x, y)))
        .filter(|&(x, y)| x * x + y * y <= 12 * 12)
        .filter_map(|(x, y)| {
            let point = center + Vec2::new(x as f32, y as f32) * 64.0;
            // Procedural land is at zero elevation and underwater terrain is negative.
            let height = map.environment.true_height(point).filter(|&z| z >= 0.0)?;
            // A patch of land gives the player more usable space than a narrow shoreline.
            [Vec2::X, -Vec2::X, Vec2::Y, -Vec2::Y]
                .into_iter()
                .all(|dir| {
                    map.environment
                        .true_height(point + dir * 32.0)
                        .is_some_and(|z| z >= 0.0)
                })
                .then_some(point.z(height))
        })
        .min_by(|a, b| {
            a.xy()
                .distance2(preferred)
                .total_cmp(&b.xy().distance2(preferred))
        })
}

#[cfg(test)]
mod qf1_tests {
    use super::*;

    fn pose(x: u32) -> Camera {
        Camera::new(Vec3::new(x as f32, 0.0, 0.0), 1280.0, 720.0)
    }

    #[test]
    fn qf1_camera_update_throttles_motion_and_flushes_when_still() {
        let start = Instant::now();
        let mut save = CameraSave::default();
        let mut writes = Vec::new();
        let mut writer = |camera: Camera| {
            writes.push(camera.pos.x as u32);
            let (done, result) = mpsc::channel();
            done.send(true).unwrap();
            result
        };
        for step in 0..=20 {
            save.update(pose(step), start + CAMERA_SETTLE_TIME * step, &mut writer);
        }
        save.update(pose(20), start + CAMERA_SETTLE_TIME * 21, &mut writer);
        save.update(pose(20), start + CAMERA_SETTLE_TIME * 40, &mut writer);
        assert_eq!(writes, [0, 8, 16, 20]);
    }

    #[test]
    fn qf1_camera_update_coalesces_while_a_write_is_in_flight() {
        let start = Instant::now();
        let mut save = CameraSave::default();
        let (done, result) = mpsc::channel();
        save.update(pose(1), start, |_| result);
        save.update(pose(2), start + CAMERA_SAVE_INTERVAL, |_| {
            panic!("second writer")
        });
        assert!(save.saved.is_none());
        done.send(true).unwrap();
        save.update(pose(3), start + CAMERA_SAVE_INTERVAL * 2, |camera| {
            assert!(same_camera(&camera, &pose(3)));
            mpsc::channel().1
        });
        assert!(same_camera(&save.saved.unwrap(), &pose(1)));
    }

    #[test]
    fn qf1_camera_failure_backs_off_even_after_the_camera_settles() {
        let start = Instant::now();
        let mut save = CameraSave::default();
        let fail = |_| {
            let (done, result) = mpsc::channel();
            done.send(false).unwrap();
            result
        };
        save.update(pose(1), start, fail);
        save.update(pose(1), start, |_| panic!("immediate retry"));
        save.update(pose(1), start + CAMERA_SETTLE_TIME, |_| {
            panic!("settling bypassed backoff")
        });
        assert!(save.saved.is_none());
        let retry = start + CAMERA_SAVE_INTERVAL;
        save.update(pose(1), retry, fail);
        save.update(pose(1), retry, |_| panic!("immediate retry"));
        save.update(pose(1), retry + CAMERA_SAVE_INTERVAL, |_| {
            panic!("backoff did not grow")
        });
        save.update(pose(1), retry + CAMERA_SAVE_INTERVAL * 2, fail);
        assert!(save.in_flight.is_some());
    }

    #[test]
    fn qf1_camera_shutdown_flushes_the_latest_pose_in_order() {
        let start = Instant::now();
        let mut save = CameraSave {
            enabled: true,
            ..CameraSave::default()
        };
        let (done, result) = mpsc::channel();
        save.update(pose(1), start, |_| result);

        assert!(!save.poll_shutdown(pose(2), start, |_| panic!("second writer")));
        done.send(true).unwrap();
        let (done, result) = mpsc::channel();
        let next_poll = start + Duration::from_millis(100);
        assert!(!save.poll_shutdown(pose(2), next_poll, |camera| {
            assert!(same_camera(&camera, &pose(2)));
            result
        }));
        done.send(true).unwrap();
        assert!(save.poll_shutdown(pose(2), next_poll, |_| panic!("pose already saved")));
        assert!(same_camera(&save.saved.unwrap(), &pose(2)));
    }

    #[test]
    fn qf1_camera_shutdown_stops_waiting_at_the_deadline() {
        let start = Instant::now();
        let mut save = CameraSave {
            enabled: true,
            ..CameraSave::default()
        };
        let (_stalled, result) = mpsc::channel();
        save.update(pose(1), start, |_| result);
        assert!(!save.poll_shutdown(pose(2), start, |_| panic!("second writer")));

        let deadline = start + CAMERA_SHUTDOWN_TIMEOUT;
        assert!(
            !save.poll_shutdown(pose(2), deadline - Duration::from_millis(1), |_| {
                panic!("second writer")
            })
        );
        assert!(save.poll_shutdown(pose(2), deadline, |_| panic!("second writer")));
    }
}

#[cfg(test)]
mod ux2_tests {
    use super::*;
    use geom::vec2;

    fn clamp(point: Vec2, world: AABB) -> Vec2 {
        let bounds = movement_bounds(world);
        vec2(
            point.x.clamp(bounds.ll.x, bounds.ur.x),
            point.y.clamp(bounds.ll.y, bounds.ur.y),
        )
    }

    #[test]
    fn ux2_orbit_centre_reaches_every_map_edge_plus_a_margin() {
        for (width, height) in [
            (0.0, 0.0),
            (512.0, 512.0),
            (512.0, 8192.0),
            (8192.0, 8192.0),
            (25000.0, 25000.0),
        ] {
            let world = AABB::new_ll_ur(vec2(0.0, 0.0), vec2(width, height));
            let reach = |point| clamp(point, world);
            let far = 1.0e6;
            assert_eq!(reach(vec2(-far, -far)), vec2(-EDGE_MARGIN, -EDGE_MARGIN));
            assert_eq!(
                reach(vec2(far, far)),
                vec2(width + EDGE_MARGIN, height + EDGE_MARGIN)
            );
            for edge in [vec2(0.0, height * 0.5), vec2(width, height * 0.5)] {
                assert_eq!(reach(edge), edge, "{width}x{height}");
            }
            // Clamping a clamped position is stable, so the camera cannot jitter at the edge.
            let edge = reach(vec2(width + 37.0, -far));
            assert_eq!(reach(edge), edge);
        }
    }
}

#[cfg(test)]
mod af2_tests {
    use super::*;
    use simulation::{NewGameOptions, Simulation};

    #[test]
    fn af2_new_game_frames_land_near_the_starter_station() {
        scenarios::ensure_repo_root_cwd();
        crate::init::init();
        let sim = Simulation::new_game(&NewGameOptions {
            seed: 5831623300802791913,
            ..NewGameOptions::default()
        })
        .unwrap();
        let map = sim.map();
        let station = map
            .buildings()
            .values()
            .find(|building| matches!(building.kind, BuildingKind::RailFreightStation(_)))
            .unwrap();
        let mut orbit = OrbitCamera::from_camera(Camera::new(Vec3::ZERO, 1280.0, 720.0));
        orbit.reset_for_world(&map, None);

        let point = orbit.camera.pos.xy();
        let center = station.obb.center();
        assert!(
            point.distance(center) <= 800.0,
            "{point:?}, station {center:?}"
        );
        assert!(
            map.environment.true_height(point).unwrap() >= 0.0,
            "{point:?}"
        );
        let (screen, depth) = orbit.camera.project(station.obb.center().z(station.height));
        assert!(
            depth > 0.0 && (0.0..1280.0).contains(&screen.x) && (0.0..720.0).contains(&screen.y)
        );
    }
}
