#[cfg(feature = "multiplayer")]
pub use self::inner::*;
#[cfg(any(feature = "multiplayer", test))]
use crate::uiworld::TilePurchaseReceipt;
#[cfg(any(feature = "multiplayer", test))]
use crate::gui::traffic_repair::TrafficEditReceipt;
use crate::uiworld::UiWorld;
use common::timestep::Timestep;
#[cfg(any(feature = "multiplayer", test))]
use simulation::world_command::{CommandOutcomes, WorldCommand, WorldCommands};

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
fn frame_traffic_edits<'a>(
    generation: u64,
    frame: u64,
    outcomes: &'a CommandOutcomes,
    inputs: impl IntoIterator<Item = (u32, bool, &'a WorldCommands)> + 'a,
) -> impl Iterator<Item = TrafficEditReceipt> + 'a {
    inputs.into_iter()
        .flat_map(|(sender, local, commands)| {
            commands.iter().enumerate().map(move |(index, command)| (sender, local, index, command))
        })
        .enumerate()
        .filter_map(move |(flattened, (sender, local, index, command))| {
            TrafficEditReceipt::from_outcome(
                generation, frame, Some(sender), index, local, flattened, command, outcomes,
            )
        })
}

pub fn is_singleplayer(uiw: &UiWorld) -> bool {
    matches!(*uiw.read::<NetworkState>(), NetworkState::Singleplayer(_))
}

#[cfg(feature = "multiplayer")]
mod inner {
    use super::{CommandOutcomes, TilePurchaseReceipt, WorldCommands};
    use crate::game_loop::{State, Timings, VERSION};
    use crate::gui::windows::network::NetworkConnectionInfo;
    use crate::uiworld::ReceivedCommands;
    use common::timestep::Timestep;
    use networking::{
        ConnectConf, Frame, PollResult, ServerConfiguration, ServerPollResult, VirtualClientConf,
    };
    use prototypes::DELTA_F64;
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
                        discard_traffic_edits(&state.uiw);
                        state.uiw.write::<NetworkConnectionInfo>().error =
                            "The server replaced the active world.".into();
                        return true;
                    }
                    PollResult::Disconnect(reason) => {
                        discard_traffic_edits(&state.uiw);
                        state.uiw.write::<NetworkConnectionInfo>().error = reason;
                        return true;
                    }
                }
            }
        }

        if let Some(inputs) = inputs_to_apply {
            let mut merged = WorldCommands::default();
            let mut purchases = Vec::new();
            let mut traffic_edits = Vec::new();
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
                purchases.extend(TilePurchaseReceipt::from_outcomes(
                    generation,
                    frame_commands.frame.0,
                    &outcomes,
                    local(),
                ));
                traffic_edits.extend(super::frame_traffic_edits(
                    generation,
                    frame_commands.frame.0,
                    &outcomes,
                    inputs.iter().map(|input| (input.sender, input.sent_by_me, &input.inp)),
                ));
                for (index, command) in local() {
                    if outcomes.accepted.contains(&index) {
                        merged.push(command.clone());
                    }
                }
            }
            crate::gui::overlays::land_tiles::receive_network(&state.uiw, &purchases);
            crate::gui::traffic_repair::receive_network(&state.uiw, &traffic_edits);
            let mut received = ReceivedCommands::new(merged);
            received.traffic_edits = traffic_edits;
            received.tile_purchases = purchases;
            *state.uiw.write::<ReceivedCommands>() = received;
        }
        false
    }

    fn discard_traffic_edits(uiw: &crate::uiworld::UiWorld) {
        let generation = uiw.read::<crate::session::SessionState>().generation;
        crate::gui::traffic_repair::network_reset(uiw, generation);
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
fn population_scale_native_protocol_matches_world_snapshot() {
    use common::saveload::{Bincode, Encoder};

    scenarios::ensure_repo_root_cwd();
    simulation::init::init();
    let sim = simulation::Simulation::new_with_options(simulation::SimulationOptions {
        terrain_size: 1,
        persons_per_agent: 4,
        ..simulation::SimulationOptions::LEGACY
    });
    let bytes = Bincode::encode(&sim).unwrap();
    let snapshot_version: String = Bincode::decode_prefix(&bytes).unwrap();

    assert_eq!(crate::game_loop::VERSION, snapshot_version);
    assert_ne!(snapshot_version, "0.11.0");
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
        receipts.extend(TilePurchaseReceipt::from_outcomes(
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
    let rejected = TilePurchaseReceipt::from_outcomes(
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

#[cfg(all(test, feature = "multiplayer"))]
#[test]
fn p7_ingress_carries_real_road_commands_and_rejects_oversized_frames() {
    use common::saveload::{Bincode, Encoder};
    use networking::{
        Client, ConnectConf, Frame, PollResult, Server, ServerConfiguration, ServerPollResult,
    };
    use simulation::map::road_edit::{RoadEditStamp, RoadQuote, RoadUpgrade};
    use simulation::map::{LaneKind, LanePatternBuilder, RoadID};
    use std::net::{Ipv4Addr, TcpListener};
    use std::time::{Duration, Instant};

    let zero = Bincode::encode(&0u64).unwrap();
    let road_upgrade = |change| WorldCommand::MapUpgradeRoad {
        road: RoadID::default(),
        expected: Bincode::decode::<RoadEditStamp>(&zero).unwrap(),
        change,
        quote: Bincode::decode::<RoadQuote>(&zero).unwrap(),
    };
    let pattern = LanePatternBuilder::new().speed_limit(13.0).build();
    let command = road_upgrade(RoadUpgrade::Replace(pattern.clone()));
    let check_wire = |commands| {
        let bytes = Bincode::encode(&WorldCommands::from(commands)).unwrap();
        Bincode::decode::<WorldCommands>(&bytes)
    };
    let excessive = (0..257).map(|_| command.clone()).collect::<Vec<_>>();
    let mut overwide = pattern;
    overwide.lanes_forward = vec![(LaneKind::Driving, 13.0); 33];
    assert!(check_wire(vec![road_upgrade(RoadUpgrade::Replace(overwide))]).is_err());

    let release = include_str!("../../VERSION");
    let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut frame = Frame(0);
    let mut server = Server::<u64, WorldCommands>::start(ServerConfiguration {
        start_frame: frame,
        period: Duration::from_millis(20),
        port: Some(port),
        virtual_client: None,
        version: release.into(),
        always_run: false,
    })
    .unwrap();
    let mut client = Client::<u64, WorldCommands>::connect(ConnectConf {
        name: "road-editor".into(),
        addr: Ipv4Addr::LOCALHOST.into(),
        port: Some(port),
        frame_buffer_advance: 3,
        version: release.into(),
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut got_world = false;
    loop {
        assert!(
            Instant::now() < deadline,
            "the server did not consume the road edit"
        );
        if let ServerPollResult::Input(frames) = server.poll(&19, frame, None) {
            for input in frames {
                frame = input.frame;
                if let Some(received) = input
                    .inputs
                    .iter()
                    .find(|input| !input.inp.as_ref().is_empty())
                {
                    assert!(got_world);
                    assert_eq!(
                        Bincode::encode(&received.inp).unwrap(),
                        Bincode::encode(&WorldCommands::from(vec![command.clone()])).unwrap(),
                    );
                    assert!(check_wire(excessive).is_err());
                    return;
                }
            }
        }
        match client.poll(WorldCommands::from(vec![command.clone()])) {
            PollResult::GameWorld(_, value) => {
                assert_eq!(value, 19);
                got_world = true;
            }
            PollResult::Disconnect(reason) => panic!("{reason}"),
            PollResult::Wait(_) | PollResult::Input(_) => {}
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
#[test]
fn p7_receipt_keeps_local_rejection_and_remote_success_in_separate_frames() {
    use crate::gui::traffic_repair::{test_road_upgrade, TrafficEditResult};
    use common::saveload::{Bincode, Encoder};
    use simulation::map::road_edit::{RoadEditStatus, RoadQuote};
    use simulation::world_command::Rejection;
    use simulation::{MapPreset, NewGameOptions, Simulation};

    scenarios::ensure_repo_root_cwd();
    crate::init::init();
    let mut sim = Simulation::new_game(&NewGameOptions { preset: MapPreset::Flat, ..Default::default() })
        .unwrap();
    let (road, repair) = test_road_upgrade(&sim);
    let WorldCommand::MapUpgradeRoad { expected, change, .. } = repair.clone() else { unreachable!() };
    let tax = WorldCommand::SetTaxRate { zone: simulation::map::ZoneKind::Residential, rate_permille: 120 };
    let local = WorldCommands::from(vec![tax.clone(), WorldCommand::MapUpgradeRoad {
        road, expected, change, quote: Bincode::decode::<RoadQuote>(&Bincode::encode(&0u64).unwrap()).unwrap(),
    }]);
    let other = WorldCommands::from(vec![tax]);
    sim.apply_commands(other.iter().chain(local.iter()));
    let mut receipts: Vec<_> = frame_traffic_edits(
        4, 2, &sim.read::<CommandOutcomes>(), [(12, false, &other), (24, true, &local)],
    ).collect();
    let remote = WorldCommands::from(vec![repair]);
    sim.apply_commands(remote.iter());
    receipts.extend(frame_traffic_edits(4, 3, &sim.read::<CommandOutcomes>(), [(12, false, &remote)]));
    assert_eq!(receipts.len(), 2);
    assert_eq!((receipts[0].frame, receipts[0].sender, receipts[0].command_index, receipts[0].local), (2, Some(24), 1, true));
    assert_eq!(receipts[0].result, Err(Rejection::StaleRoadQuote));
    assert_eq!((receipts[1].frame, receipts[1].sender, receipts[1].command_index, receipts[1].local), (3, Some(12), 0, false));
    assert!(matches!(&receipts[1].result, Ok(TrafficEditResult::Road(receipt)) if receipt.road == road && receipt.status == RoadEditStatus::Applied));
    assert!(receipts[0].sequence < receipts[1].sequence);
}
