#[cfg(feature = "multiplayer")]
pub use self::inner::*;
use crate::uiworld::UiWorld;
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
    use simulation::world_command::{CommandOutcomes, WorldCommands};
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
            for frame_commands in inputs {
                assert_eq!(frame_commands.frame.0, sim.get_tick() + 1);
                let commands: WorldCommands = frame_commands
                    .inputs
                    .iter()
                    .map(|x| x.inp.clone())
                    .collect();
                let t = sim.tick(&mut state.game_schedule, commands.as_ref());
                state
                    .uiw
                    .write::<Timings>()
                    .world_update
                    .add_value(t.as_secs_f32());
                let accepted = sim.read::<CommandOutcomes>();
                let mut offset = 0;
                for input in frame_commands.inputs {
                    for (index, command) in input.inp.as_ref().iter().enumerate() {
                        if input.sent_by_me && accepted.accepted.contains(&(offset + index)) {
                            merged.push(command.clone());
                        }
                    }
                    offset += input.inp.as_ref().len();
                }
            }
            *state.uiw.write::<ReceivedCommands>() = ReceivedCommands::new(merged);
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
            PollResult::GameWorld(_, sim) => Bootstrap::Ready(sim),
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
