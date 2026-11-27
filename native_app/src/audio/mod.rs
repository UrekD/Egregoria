use crate::audio::ambient::Ambient;
use crate::audio::car_sounds::CarSounds;
use crate::audio::music::Music;
use crate::uiworld::UiWorld;
use engine::AudioContext;
use geom::Camera;
use simulation::map::RenderMap;
use simulation::Simulation;

mod ambient;
mod car_sounds;
mod music;

pub static SOUNDS_LIST: include_dir::Dir = include_dir::include_dir!("assets/sounds");

pub(crate) struct AudioInputs(car_sounds::CarAudio);

pub struct GameAudio {
    music: Music,
    ambiant: Ambient,
    carsounds: CarSounds,
}

impl GameAudio {
    pub fn new(ctx: &mut AudioContext) -> Self {
        defer!(log::info!("finished init of game audio"));

        ctx.preload(
            SOUNDS_LIST
                .files()
                .flat_map(|x| x.path().file_name())
                .flat_map(|x| x.to_str())
                .map(|x| x.trim_end_matches(".ogg")),
        );

        Self {
            music: Music::new(),
            ambiant: Ambient::new(ctx),
            carsounds: CarSounds::new(ctx),
        }
    }

    pub(crate) fn reset_world(&mut self) {
        self.carsounds.reset_world();
    }

    pub(crate) fn capture(&self, sim: &Simulation, uiworld: &UiWorld) -> AudioInputs {
        AudioInputs(self.carsounds.capture(sim, uiworld.read::<Camera>().eye()))
    }

    pub(crate) fn update(
        &mut self,
        input: Option<AudioInputs>,
        map: &RenderMap,
        uiworld: &UiWorld,
        ctx: &mut AudioContext,
    ) {
        self.music.update(ctx);
        self.ambiant.update(map, uiworld);
        if let Some(AudioInputs(cars)) = input {
            self.carsounds.update(cars, ctx);
        }
    }
}
