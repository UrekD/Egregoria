pub use self::inner::*;
use crate::game_loop::{State, Timings};
use crate::gui::windows::settings::Settings;
use crate::uiworld::{ReceivedCommands, UiWorld};
use common::timestep::Timestep;
use simulation::world_command::{CommandOutcomes, WorldCommand, WorldCommands};
use simulation::Simulation;

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
mod inner {
    use crate::network::{State, Timestep};
    use crate::session::WorldReplacement;

    #[allow(clippy::large_enum_variant)]
    pub enum NetworkState {
        Singleplayer(Timestep),
    }

    pub fn sim_update(state: &mut State) -> Option<WorldReplacement> {
        super::handle_singleplayer(state);
        None
    }
}

#[allow(dead_code)]
fn handle_singleplayer(state: &mut State) {
    let mut sim = unwrap_orr!(state.sim.try_write(), return); // mut for tick

    let timewarp = state.uiw.read::<Settings>().time_warp;
    let mut commands = std::mem::take(&mut *state.uiw.write::<WorldCommands>());
    *state.uiw.write::<ReceivedCommands>() = ReceivedCommands::default();

    let sched = &mut state.game_schedule;
    let mut timings = state.uiw.write::<Timings>();

    let mut has_commands = !commands.is_empty();

    if has_commands && commands.iter().all(WorldCommand::is_instant) {
        sim.apply_commands(commands.as_ref());
        publish_accepted_commands(&state.uiw, &sim, &commands);
        commands = WorldCommands::default();
        has_commands = false;
    }

    let mut net_state = state.uiw.write::<NetworkState>();

    #[allow(irrefutable_let_patterns)]
    let NetworkState::Singleplayer(ref mut step) = *net_state
    else {
        return;
    };

    let mut commands_once = Some(commands.clone());
    step.prepare_frame(timewarp);
    while step.tick() || (has_commands && commands_once.is_some()) {
        let t = sim.tick(sched, commands_once.take().unwrap_or_default().as_ref());
        step.record_tick();
        timings.world_update.add_value(t.as_secs_f32());
    }

    if commands_once.is_none() {
        if has_commands {
            publish_accepted_commands(&state.uiw, &sim, &commands);
        }
    } else {
        *state.uiw.write::<WorldCommands>() = commands;
    }
}

fn publish_accepted_commands(uiw: &UiWorld, sim: &Simulation, commands: &WorldCommands) {
    let mut accepted = WorldCommands::default();
    accepted.extend(
        sim.read::<CommandOutcomes>()
            .accepted
            .iter()
            .map(|&index| commands.as_ref()[index].clone()),
    );
    *uiw.write::<ReceivedCommands>() = ReceivedCommands::new(accepted);
}

pub fn is_singleplayer(uiw: &UiWorld) -> bool {
    matches!(*uiw.read::<NetworkState>(), NetworkState::Singleplayer(_))
}

#[cfg(feature = "multiplayer")]
mod inner {
    use crate::game_loop::{State, Timings, VERSION};
    use crate::gui::windows::network::NetworkConnectionInfo;
    use crate::session::WorldReplacement;
    use crate::uiworld::ReceivedCommands;
    use common::timestep::Timestep;
    use networking::{
        ConnectConf, Frame, PollResult, ServerConfiguration, ServerPollResult, VirtualClientConf,
    };
    use prototypes::DELTA_F64;
    use simulation::world_command::WorldCommands;
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

    pub fn sim_update(state: &mut State) -> Option<WorldReplacement> {
        if super::is_singleplayer(&state.uiw) {
            super::handle_singleplayer(state);
            return None;
        }

        let mut sim = state.sim.try_write().ok()?;

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
                    PollResult::GameWorld(_, prepared_sim) => {
                        // The returned batch is local input from the old world. The
                        // client's catch-up state holds the actual protocol inputs.
                        return Some(WorldReplacement::Network(prepared_sim));
                    }
                    PollResult::Disconnect(reason) => {
                        log::error!(
                            "got disconnected :-( continuing with server world but it's sad"
                        );
                        *net_state = NetworkState::Singleplayer(Timestep::default());
                        state.uiw.write::<NetworkConnectionInfo>().error = reason;
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
                merged.merge(
                    &frame_commands
                        .inputs
                        .into_iter()
                        .filter(|x| x.sent_by_me)
                        .map(|x| x.inp)
                        .collect::<WorldCommands>(),
                );
            }
            *state.uiw.write::<ReceivedCommands>() = ReceivedCommands::new(merged);
        }
        None
    }

    pub fn start_server(info: &mut NetworkConnectionInfo, sim: &Simulation) -> Option<Server> {
        let server = match networking::Server::start(ServerConfiguration {
            start_frame: Frame(sim.get_tick()),
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
