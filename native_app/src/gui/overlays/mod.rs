use goryak::combo_box;
use prototypes::ServiceKind;
use serde::{Deserialize, Deserializer};
use simulation::Simulation;

use crate::uiworld::UiWorld;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
pub(crate) enum OverlayMode {
    #[default]
    Off,
    Power,
    Water,
    Sewage,
    Coverage(#[serde(deserialize_with = "deserialize_coverage")] ServiceKind),
    Pollution,
    LandValue,
    Happiness,
}

fn deserialize_coverage<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<ServiceKind, D::Error> {
    let kind = ServiceKind::deserialize(deserializer)?;
    match kind {
        ServiceKind::WaterPump | ServiceKind::SewageOutlet => Err(serde::de::Error::custom(
            "utility services have no coverage overlay",
        )),
        _ => Ok(kind),
    }
}

pub(crate) fn draw(_sim: &Simulation, _uiworld: &UiWorld) {}

pub(crate) fn selector(uiworld: &UiWorld) {
    use OverlayMode::*;

    const MODES: [(OverlayMode, &str); 13] = [
        (Off, "Overlay off"),
        (Power, "Power"),
        (Water, "Water"),
        (Sewage, "Sewage"),
        (Coverage(ServiceKind::Health), "Health coverage"),
        (Coverage(ServiceKind::Fire), "Fire coverage"),
        (Coverage(ServiceKind::Police), "Police coverage"),
        (Coverage(ServiceKind::School), "School coverage"),
        (Coverage(ServiceKind::Garbage), "Garbage coverage"),
        (Coverage(ServiceKind::Park), "Park coverage"),
        (Pollution, "Pollution"),
        (LandValue, "Land value"),
        (Happiness, "Happiness"),
    ];

    let mut mode = uiworld.write::<OverlayMode>();
    let mut selected = MODES
        .iter()
        .position(|(option, _)| *option == *mode)
        .unwrap_or(0);
    if combo_box(&mut selected, &MODES.map(|(_, label)| label), 175.0) {
        *mode = MODES[selected].0;
    }
}
