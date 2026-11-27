use engine::{AudioContext, AudioKind, Gain, GainControl};
use flat_spatial::grid::GridHandle;
use geom::{Vec2, Vec3, AABB};
use oddio::{Cycle, Mixed, Seek, Speed, SpeedControl};
use simulation::transportation::{TransportGrid, TransportationGroup};
use simulation::Simulation;
use slotmapd::SecondaryMap;

/// CarSound is the sound of a single car
pub struct CarSound {
    road: Option<(SpeedControl, GainControl, Mixed)>,
    engine: Option<(SpeedControl, GainControl, Mixed)>,
}

impl CarSound {
    fn stop(self) {
        for (_, _, mut mixed) in [self.road, self.engine].into_iter().flatten() {
            mixed.stop();
        }
    }
}

const HEAR_RADIUS: f32 = 200.0;

#[cfg(not(debug_assertions))]
const MAX_SOUNDS: usize = 30;
#[cfg(debug_assertions)]
const MAX_SOUNDS: usize = 1;

pub(super) struct CarAudio {
    camera: Vec3,
    cars: Vec<(GridHandle, Vec2, Vec2, f32)>,
    nearby_count: usize,
}

/// CarSounds are sounds that are played when cars are near the player
/// They are tied to a car entity
pub struct CarSounds {
    sounds: SecondaryMap<GridHandle, CarSound>,
    generic_car_sound: Option<GainControl>,
}

impl CarSounds {
    pub fn new(ctx: &mut AudioContext) -> Self {
        Self {
            sounds: SecondaryMap::new(),
            generic_car_sound: ctx
                .play_with_control(
                    "car_loop",
                    |x| Gain::new(Cycle::new(x), 0.0),
                    AudioKind::Effect,
                )
                .map(|x| x.0),
        }
    }

    pub(crate) fn reset_world(&mut self) {
        for (_, sound) in self.sounds.drain() {
            sound.stop();
        }
        if let Some(gain) = &mut self.generic_car_sound {
            gain.set_amplitude_ratio(0.0);
        }
    }

    pub(super) fn capture(&self, sim: &Simulation, camera: Vec3) -> CarAudio {
        let grid = sim.read::<TransportGrid>();
        let mut cars = Vec::with_capacity(MAX_SOUNDS);

        for (handle, _) in &self.sounds {
            if let Some((pos, car)) = grid.get(handle) {
                if pos.z0().is_close(camera, HEAR_RADIUS) {
                    cars.push((handle, pos, car.dir, car.speed));
                }
            }
        }
        let search = (HEAR_RADIUS * HEAR_RADIUS - camera.z * camera.z)
            .max(0.0)
            .sqrt();
        for (handle, _) in grid.query_around(camera.xy(), search) {
            if cars.len() == MAX_SOUNDS {
                break;
            }
            if self.sounds.contains_key(handle) {
                continue;
            }
            if let Some((pos, car)) = grid.get(handle) {
                if matches!(car.group, TransportationGroup::Vehicles) {
                    cars.push((handle, pos, car.dir, car.speed));
                }
            }
        }
        let nearby_count = if camera.z < 1000.0 {
            let bounds = AABB::centered(camera.xy(), Vec2::splat(200.0));
            grid.query_aabb(bounds.ll, bounds.ur)
                .filter_map(|(handle, _)| grid.get(handle))
                .filter(|(_, car)| matches!(car.group, TransportationGroup::Vehicles))
                .take(100)
                .count()
        } else {
            0
        };
        CarAudio {
            camera,
            cars,
            nearby_count,
        }
    }

    pub(super) fn update(&mut self, input: CarAudio, ctx: &mut AudioContext) {
        self.sounds.retain(|handle, sound| {
            if input.cars.iter().any(|&(id, ..)| id == handle) {
                return true;
            }
            for (_, _, mixed) in [&mut sound.road, &mut sound.engine].into_iter().flatten() {
                mixed.stop();
            }
            false
        });

        let campos = input.camera;
        for (handle, pos, heading, velocity) in input.cars {
            if !self.sounds.contains_key(handle) {
                let engine = ctx
                    .play_with_control(
                        "car_engine",
                        |x| {
                            let mut cycle = Cycle::new(x);
                            cycle.seek(common::rand::rand2(pos.x, pos.y));
                            let (g_control, signal) = Gain::new(cycle, 0.0);
                            let (speed_control, signal) = Speed::new(signal);
                            ((speed_control, g_control), signal)
                        },
                        AudioKind::Effect,
                    )
                    .map(|((a, b), c)| (a, b, c));

                let road = ctx
                    .play_with_control(
                        "car_loop",
                        |x| {
                            let mut cycle = Cycle::new(x);
                            cycle.seek(common::rand::rand2(pos.x, pos.y));
                            let (g_control, signal) = Gain::new(cycle, 0.0);
                            let (speed_control, signal) = Speed::new(signal);
                            ((speed_control, g_control), signal)
                        },
                        AudioKind::Effect,
                    )
                    .map(|((a, b), c)| (a, b, c));

                self.sounds.insert(handle, CarSound { road, engine });
            }
            let cs = &mut self.sounds[handle];
            let his_speed = (velocity * heading).z0();
            let dir_to_me = (campos - pos.z(campos.z * 0.5)).normalize();
            let speed_to_me = his_speed.dot(dir_to_me);
            let boost = 300.0 / (300.0 - speed_to_me);

            if let Some((ref mut speed, ref mut gain, _)) = cs.road {
                gain.set_amplitude_ratio(velocity.sqrt() * 3.0 / pos.z0().distance(campos));
                speed.set_speed(boost)
            }
            if let Some((ref mut speed, ref mut gain, _)) = cs.engine {
                gain.set_amplitude_ratio(velocity.sqrt() / pos.z0().distance(campos));
                speed.set_speed(boost)
            }
        }

        if let Some(ref mut sound) = self.generic_car_sound {
            let gain = input.nearby_count as f32 / 100.0 * (1.0 - campos.z / 1000.0);
            sound.set_amplitude_ratio(gain.clamp(0.0, 0.03));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oddio::{Constant, Mixer, Signal};

    #[test]
    fn p4_car_sound_reset_stops_both_vehicle_loops() {
        let (mut control, mut mixer) = Mixer::<f32>::new();
        let mut start_loop = || {
            let (gain, signal) = Gain::new(Constant::new(1.0), 1.0);
            let (speed, signal) = Speed::new(signal);
            (speed, gain, control.play(signal))
        };
        let mut grid = TransportGrid::new(100);
        let handle = grid.insert(Vec2::ZERO, Default::default());
        let mut cars = CarSounds {
            sounds: SecondaryMap::new(),
            generic_car_sound: None,
        };
        cars.sounds.insert(
            handle,
            CarSound {
                road: Some(start_loop()),
                engine: Some(start_loop()),
            },
        );
        let mut samples = [0.0; 16];
        mixer.sample(1.0 / 48_000.0, &mut samples);
        assert_eq!(samples, [2.0; 16]);

        cars.reset_world();
        assert!(cars.sounds.is_empty());
        mixer.sample(1.0 / 48_000.0, &mut samples);
        assert_eq!(samples, [0.0; 16]);
    }
}
