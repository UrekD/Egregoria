use goryak::{
    button_primary, button_secondary, error, minrow, on_secondary_container,
    selectable_label_primary, text_edit, textc,
};
use simulation::economy::progression::ProgressionMode;
use simulation::{MapPreset, NewGameOptions, Simulation};

use super::{self as session, PreparedWorld, SessionUi, WorldJobKind, WorldReplacement};
use crate::uiworld::UiWorld;

#[derive(Default)]
pub(super) struct NewGameForm {
    name: String,
    seed: String,
    size: String,
    money: String,
    preset: MapPreset,
    progression: ProgressionMode,
    advanced: bool,
    record_replay: bool,
    error: Option<&'static str>,
}

impl NewGameForm {
    pub(super) fn fresh() -> Self {
        Self {
            name: "New City".into(),
            seed: session::random_seed().to_string(),
            size: "50".into(),
            money: "150000".into(),
            progression: ProgressionMode::Guided,
            ..Self::default()
        }
    }

    fn options(&self) -> Result<NewGameOptions, &'static str> {
        let seed = self
            .seed
            .trim()
            .parse()
            .map_err(|_| "Enter a whole seed from 0 to 18446744073709551615.")?;
        let terrain_chunks = self
            .size
            .trim()
            .parse()
            .ok()
            .filter(|size| {
                (NewGameOptions::MIN_TERRAIN_CHUNKS..=NewGameOptions::MAX_TERRAIN_CHUNKS)
                    .contains(size)
            })
            .ok_or("Enter a map size from 16 to 80 chunks.")?;
        let starting_money_bucks = self
            .money
            .trim()
            .parse()
            .ok()
            .filter(|money| {
                (NewGameOptions::MIN_STARTING_MONEY_BUCKS
                    ..=NewGameOptions::MAX_STARTING_MONEY_BUCKS)
                    .contains(money)
            })
            .ok_or("Enter starting money from 10,000 to 10,000,000 bucks.")?;
        Ok(NewGameOptions {
            preset: self.preset,
            seed,
            terrain_chunks,
            starting_money_bucks,
            progression: self.progression,
        })
    }
}

pub(super) fn render(uiw: &UiWorld) {
    let mut ui = uiw.write::<SessionUi>();
    let form = &mut ui.new_game;
    textc(on_secondary_container(), "City name");
    text_edit(275.0, &mut form.name, "City name");

    textc(on_secondary_container(), "Terrain preset");
    minrow(8.0, || {
        for (preset, label) in [
            (MapPreset::Procedural, "Procedural"),
            (MapPreset::Flat, "Flat"),
        ] {
            if selectable_label_primary(form.preset == preset, label).clicked {
                form.preset = preset;
            }
        }
    });

    textc(on_secondary_container(), "City mode");
    minrow(8.0, || {
        for (mode, label) in [
            (ProgressionMode::Guided, "Guided"),
            (ProgressionMode::FreeBuild, "Free build"),
        ] {
            if selectable_label_primary(form.progression == mode, label).clicked {
                form.progression = mode;
            }
        }
    });

    textc(on_secondary_container(), "Seed (0 uses legacy generation)");
    minrow(8.0, || {
        text_edit(230.0, &mut form.seed, "Seed");
        if button_secondary("Randomize").show().clicked {
            form.seed = session::random_seed().to_string();
        }
    });

    textc(on_secondary_container(), "Map size (16–80 chunks)");
    minrow(8.0, || {
        text_edit(65.0, &mut form.size, "Size");
        for (size, label) in [("16", "Small"), ("50", "Standard"), ("80", "Large")] {
            if selectable_label_primary(form.size == size, label).clicked {
                form.size = size.into();
            }
        }
    });

    textc(on_secondary_container(), "Starting money (bucks)");
    text_edit(175.0, &mut form.money, "Starting money");

    if button_secondary("Advanced").show().clicked {
        form.advanced ^= true;
    }
    if form.advanced && selectable_label_primary(form.record_replay, "Record debug replay").clicked
    {
        form.record_replay ^= true;
    }
    if let Some(message) = form.error {
        textc(error(), message);
    }

    let mut generate = false;
    let mut back = false;
    minrow(8.0, || {
        generate = button_primary("Create City").show().clicked;
        back = button_secondary("Back").show().clicked;
    });

    if back {
        drop(ui);
        session::back(uiw);
    } else if generate {
        let name = form.name.trim().to_owned();
        if name.is_empty() {
            form.error = Some("Enter a city name.");
            return;
        }
        let options = match form.options() {
            Ok(options) => options,
            Err(message) => {
                form.error = Some(message);
                return;
            }
        };
        let record = form.record_replay;
        form.error = None;
        drop(ui);
        let existing = session::known_slot_ids(uiw);
        session::start_world_job(uiw, WorldJobKind::NewGame, move || {
            let slot = session::choose_city_slot(common::saveload::save_dir(), &name, existing)
                .map_err(|error| format!("Could not choose a save slot. {error}"))?;
            let sim = if record {
                Simulation::new_game_with_replay(&options)
            } else {
                Simulation::new_game(&options)
            }
            .map_err(|error| error.to_string())?;
            Ok(PreparedWorld {
                replacement: WorldReplacement::NewGame(sim, slot),
                name,
            })
        });
    }
}
