#[cfg(feature = "multiplayer")]
pub use self::inner::*;
use crate::uiworld::UiWorld;
#[cfg(any(feature = "multiplayer", test))]
use crate::uiworld::TilePurchaseReceipt;
#[cfg(any(feature = "multiplayer", test))]
use simulation::world_command::{CommandOutcomes, WorldCommand, WorldCommands};
use common::timestep::Timestep;

#[cfg(feature = "multiplayer")]
#[derive(Clone, Copy)]
pub(crate) enum NetworkTarget {
    Host,
    Connect,
}

impl Default for NetworkState {
    fn default() -> Self {
        Self::Singleplayer(Timestep::default())
    }
}

impl NetworkState {
    pub fn reset_timestep(&mut self) {
        match self {
            Self::Singleplayer(step) => *step = Timestep::default(),
            #[cfg(feature = "multiplayer")]
            Self::Client(_) | Self::Server(_) => {}
        }
    }
}

#[cfg(not(feature = "multiplayer"))]
pub enum NetworkState {
    Singleplayer(Timestep),
}

#[cfg(any(feature = "multiplayer", test))]
fn local_frame_commands<'a>(
    inputs: impl IntoIterator<Item = (bool, &'a WorldCommands)> + 'a,
) -> impl Iterator<Item = (usize, &'a WorldCommand)> + 'a {
    inputs
        .into_iter()
        .flat_map(|(local, commands)| commands.iter().map(move |command| (local, command)))
        .enumerate()
        .filter_map(|(index, (local, command))| local.then_some((index, command)))
}

#[cfg(any(feature = "multiplayer", test))]
fn tile_receipts<'a>(
    generation: u64,
    frame: u64,
    outcomes: &'a CommandOutcomes,
    commands: impl IntoIterator<Item = (usize, &'a WorldCommand)> + 'a,
) -> impl Iterator<Item = TilePurchaseReceipt> + 'a {
    commands
        .into_iter()
        .filter_map(move |(command_index, command)| {
            let WorldCommand::BuyTile { tile } = *command else {
                return None;
            };
            Some(TilePurchaseReceipt {
                generation,
                frame,
                command_index,
                tile,
                batch_sequence: outcomes.batch_seq(),
                result: outcomes.tile_purchase(command_index)?,
            })
        })
}

pub fn is_singleplayer(uiw: &UiWorld) -> bool {
    matches!(*uiw.read::<NetworkState>(), NetworkState::Singleplayer(_))
}

#[cfg(feature = "multiplayer")]
mod inner {
    use crate::game_loop::{State, Timings, VERSION};
    use crate::gui::windows::network::NetworkConnectionInfo;
    use crate::uiworld::ReceivedCommands;
    use common::timestep::Timestep;
    use networking::{
        ConnectConf, Frame, PollResult, ServerConfiguration, ServerPollResult, VirtualClientConf,
    };
    use prototypes::DELTA_F64;
    use super::{CommandOutcomes, WorldCommands};
    use simulation::Simulation;
    use std::net::ToSocketAddrs;
    use std::sync::Mutex;
    use std::time::Duration;

    pub type Client = Mutex<networking::Client<Simulation, WorldCommands>>;
    pub type Server = Mutex<networking::Server<Simulation, WorldCommands>>;

    #[allow(clippy::large_enum_variant)]
    pub enum NetworkState {
        Singleplayer(Timestep),
        Client(Client),
        Server(Server),
    }

    pub(crate) fn sim_update(state: &mut State) -> bool {
        if super::is_singleplayer(&state.uiw) {
            return false;
        }

        let Ok(mut sim) = state.sim.try_write() else {
            return false;
        };

        let commands = std::mem::take(&mut *state.uiw.write::<WorldCommands>());
        *state.uiw.write::<ReceivedCommands>() = ReceivedCommands::default();

        let mut net_state = state.uiw.write::<NetworkState>();

        let mut inputs_to_apply = None;
        match &mut *net_state {
            NetworkState::Singleplayer(_) => unreachable!(),
            NetworkState::Server(ref mut server) => {
                let polled =
                    server
                        .get_mut()
                        .unwrap()
                        .poll(&sim, Frame(sim.get_tick()), Some(commands));
                match polled {
                    ServerPollResult::Wait(commands) => {
                        if let Some(commands) = commands {
                            *state.uiw.write::<WorldCommands>() = commands;
                        }
                    }
                    ServerPollResult::Input(inputs) => {
                        inputs_to_apply = Some(inputs);
                    }
                }
            }
            NetworkState::Client(ref mut client) => {
                let polled = client.get_mut().unwrap().poll(commands);
                match polled {
                    PollResult::Wait(commands) => {
                        *state.uiw.write::<WorldCommands>() = commands;
                    }
                    PollResult::Input(inputs) => {
                        inputs_to_apply = Some(inputs);
                    }
                    PollResult::GameWorld(_, _) => {
                        state.uiw.write::<NetworkConnectionInfo>().error =
                            "The server replaced the active world.".into();
                        return true;
                    }
                    PollResult::Disconnect(reason) => {
                        state.uiw.write::<NetworkConnectionInfo>().error = reason;
                        return true;
                    }
                }
            }
        }

        if let Some(inputs) = inputs_to_apply {
            let mut merged = WorldCommands::default();
            let mut purchases = Vec::new();
            let generation = state.uiw.read::<crate::session::SessionState>().generation;
            for frame_commands in inputs {
                assert_eq!(frame_commands.frame.0, sim.get_tick() + 1);
                let inputs = &frame_commands.inputs;
                let t = sim.tick(
                    &mut state.game_schedule,
                    inputs.iter().flat_map(|input| input.inp.iter()),
                );
                state
                    .uiw
                    .write::<Timings>()
                    .world_update
                    .add_value(t.as_secs_f32());
                let outcomes = sim.read::<CommandOutcomes>();
                let local = || {
                    super::local_frame_commands(
                        inputs.iter().map(|input| (input.sent_by_me, &input.inp)),
                    )
                };
                purchases.extend(super::tile_receipts(
                    generation,
                    frame_commands.frame.0,
                    &outcomes,
                    local(),
                ));
                for (index, command) in local() {
                    if outcomes.accepted.contains(&index) {
                        merged.push(command.clone());
                    }
                }
            }
            let mut received = ReceivedCommands::new(merged);
            received.tile_purchases = purchases;
            *state.uiw.write::<ReceivedCommands>() = received;
        }
        false
    }

    pub fn start_server(info: &mut NetworkConnectionInfo, tick: u64) -> Option<Server> {
        let server = match networking::Server::start(ServerConfiguration {
            start_frame: Frame(tick),
            period: Duration::from_secs_f64(DELTA_F64),
            port: None,
            virtual_client: Some(VirtualClientConf {
                name: info.name.to_string(),
            }),
            version: VERSION.to_string(),
            always_run: true,
        }) {
            Ok(x) => x,
            Err(e) => {
                info.error = format!("{:?}", e);
                return None;
            }
        };

        Some(Mutex::new(server))
    }

    pub(crate) enum Bootstrap {
        Waiting,
        Ready(Simulation),
        Failed(String),
    }

    pub(crate) fn poll_bootstrap(client: &mut Client) -> Bootstrap {
        match client.get_mut().unwrap().poll(WorldCommands::default()) {
            PollResult::Wait(_) => Bootstrap::Waiting,
            PollResult::GameWorld(_, sim) => match crate::session::validate_native_world(&sim) {
                Ok(()) => Bootstrap::Ready(sim),
                Err(error) => Bootstrap::Failed(error),
            },
            PollResult::Disconnect(message) => Bootstrap::Failed(message),
            PollResult::Input(_) => {
                Bootstrap::Failed("Server sent input before the initial world.".into())
            }
        }
    }

    pub fn start_client(info: &mut NetworkConnectionInfo) -> Option<Client> {
        let mut s = info.ip.to_string();
        if !s.contains(':') {
            s += ":23019"
        }
        let parsed_addr = match s.to_socket_addrs() {
            Ok(x) => match x.into_iter().next() {
                Some(x) => x,
                None => {
                    info.error = "no ip found with given address".to_string();
                    return None;
                }
            },
            Err(e) => {
                info.error = e.to_string();
                return None;
            }
        };

        let port = parsed_addr.port();

        let client = match networking::Client::connect(ConnectConf {
            name: info.name.clone(),
            addr: parsed_addr.ip(),
            port: if port != 23019 { Some(port) } else { None },
            frame_buffer_advance: 8,
            version: VERSION.to_string(),
        }) {
            Ok(x) => x,
            Err(e) => {
                info.error = format!("{:?}", e);
                return None;
            }
        };

        Some(Mutex::new(client))
    }
}

#[cfg(test)]
#[test]
fn land_receipts_keep_local_command_indices_across_catch_up_frames() {
    use simulation::map::{LandMode, LandTileId};
    use simulation::world_command::Rejection;
    use simulation::{MapPreset, NewGameOptions, Simulation};
    scenarios::ensure_repo_root_cwd();
    simulation::init::init();
    let mut sim = Simulation::new_game(&NewGameOptions {
        preset: MapPreset::Flat,
        terrain_chunks: 16,
        land_mode: LandMode::Tiled,
        ..NewGameOptions::default()
    })
    .unwrap();
    let remote = WorldCommands::from(vec![
        WorldCommand::SetTaxRate {
            zone: simulation::map::ZoneKind::Residential,
            rate_permille: 120,
        };
        2
    ]);
    let denied = LandTileId { x: 10, y: 7 };
    let paid = LandTileId { x: 9, y: 7 };
    let mut receipts = Vec::new();
    for (frame, tile) in [(2, denied), (3, paid)] {
        let local = WorldCommands::from(vec![WorldCommand::BuyTile { tile }]);
        sim.tick(
            &mut Simulation::schedule(),
            remote.iter().chain(local.iter()),
        );
        receipts.extend(tile_receipts(
            4,
            frame,
            &sim.read::<CommandOutcomes>(),
            local_frame_commands([(false, &remote), (true, &local)]),
        ));
    }
    let oversized = WorldCommands::from(vec![
        WorldCommand::BuyTile { tile: paid };
        sim.command_batch_limit()
    ]);
    sim.tick(
        &mut Simulation::schedule(),
        remote.iter().chain(oversized.iter()),
    );
    let outcomes = sim.read::<CommandOutcomes>();
    let rejected = tile_receipts(
        4,
        4,
        &outcomes,
        local_frame_commands([(false, &remote), (true, &oversized)]),
    )
    .collect::<Vec<_>>();
    assert_eq!(rejected.len(), sim.command_batch_limit());
    assert_eq!(
        rejected.last().unwrap().command_index,
        sim.command_batch_limit() + 1
    );
    assert_eq!(
        rejected.last().unwrap().result,
        Err(Rejection::LandDenied(
            simulation::map::LandDenial::WorkLimit
        ))
    );
    assert_eq!(receipts.len(), 2);
    assert_eq!(receipts[0].result, Err(Rejection::TileNotAdjacent));
    assert_eq!(receipts[1].result.unwrap().tile, paid);
    assert!(receipts[0].batch_sequence < receipts[1].batch_sequence);
    assert_eq!(
        receipts
            .iter()
            .map(|receipt| (receipt.generation, receipt.frame, receipt.command_index))
            .collect::<Vec<_>>(),
        [(4, 2, 2), (4, 3, 2)]
    );
}
