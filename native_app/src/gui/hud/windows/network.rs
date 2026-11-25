use std::borrow::Cow;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use yakui::divider;
use yakui::widgets::Pad;

use goryak::{
    button_primary, checkbox_value, error, on_secondary_container, outline, text_edit, textc,
    Window,
};
use simulation::Simulation;

use crate::network::NetworkState;
use crate::uiworld::UiWorld;

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct NetworkConnectionInfo {
    pub name: String,
    pub ip: String,
    #[serde(skip)]
    pub error: String,
    #[serde(skip)]
    show_hashes: bool,
    #[serde(skip)]
    hashes: BTreeMap<String, u64>,
    #[serde(skip)]
    hashes_tick: u64,
}

fn label(x: impl Into<Cow<'static, str>>) {
    textc(on_secondary_container(), x);
}

/// Network window
/// Allows to connect to a server or start a server
pub fn network(uiworld: &UiWorld, sim: &Simulation, opened: &mut bool) {
    let mut target = None;
    Window {
        title: "Network".into(),
        opened,
        pad: Pad::all(10.0),
        radius: 10.0,
        child_spacing: 10.0,
    }
    .show(|| {
        let state = uiworld.read::<NetworkState>();
        let mut info = uiworld.write::<NetworkConnectionInfo>();

        match *state {
            NetworkState::Singleplayer(_) => {
                if !info.error.is_empty() {
                    textc(error(), info.error.clone());
                    divider(outline(), 5.0, 1.0);
                }

                text_edit(200.0, &mut info.name, "Name");

                if info.name.is_empty() {
                    label("please enter your name");
                    return;
                }

                if button_primary("Start server").show().clicked {
                    target = Some(crate::network::NetworkTarget::Host);
                }

                divider(outline(), 5.0, 1.0);

                text_edit(200.0, &mut info.ip, "IP");

                if button_primary("Connect").show().clicked {
                    target = Some(crate::network::NetworkTarget::Connect);
                }
            }
            NetworkState::Client(ref client) => {
                label(client.lock().unwrap().describe());
                show_hashes(sim, &mut info);
                if button_primary("Disconnect").show().clicked {
                    uiworld
                        .write::<crate::uiworld::SaveLoadState>()
                        .network_cancel = true;
                }
            }
            NetworkState::Server(ref server) => {
                label("Running server");
                label(server.lock().unwrap().describe());
                show_hashes(sim, &mut info);
                if button_primary("Stop hosting").show().clicked {
                    uiworld
                        .write::<crate::uiworld::SaveLoadState>()
                        .network_cancel = true;
                }
            }
        }
    });
    if let Some(target) = target {
        crate::session::request_departure(uiworld, sim, crate::session::Departure::Network(target));
    }
}

fn show_hashes(sim: &Simulation, info: &mut NetworkConnectionInfo) {
    checkbox_value(
        &mut info.show_hashes,
        on_secondary_container(),
        "show hashes",
    );
    if !info.show_hashes {
        return;
    }

    if sim.get_tick() % 100 == 0 || info.hashes.is_empty() {
        info.hashes = sim.hashes();
        info.hashes_tick = sim.get_tick();
    }

    label(format!("hashes for tick {}", info.hashes_tick));
    for (name, hash) in &info.hashes {
        label(format!("{name}: {hash}"));
    }
}
