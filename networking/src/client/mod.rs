use std::net::{IpAddr, SocketAddr};

use serde::de::DeserializeOwned;
use serde::Serialize;

use client_playout::ClientPlayoutBuffer;

use crate::connection_client::ConnectionClient;
use crate::connections::ConnectionsError;
use crate::packets::{
    AuthentResponse, ClientReliablePacket, ClientUnreliablePacket, ServerReliablePacket,
    ServerUnreliablePacket,
};
use crate::worldsend::WorldReceive;
use crate::{
    decode, decode_merged, encode, AuthentID, Frame, PhantomSendSync, PlayerInput, DEFAULT_PORT,
};
use common::timestep::Timestep;

mod client_playout;

#[derive(Debug)]
pub struct FrameInputs<I> {
    pub inputs: Vec<ServerInput<I>>,
    pub frame: Frame,
}

#[derive(Debug)]
pub struct ServerInput<I> {
    pub sender: u32,
    pub sent_by_me: bool,
    pub inp: I,
}

pub enum PollResult<W, I> {
    Wait(I),
    Input(Vec<FrameInputs<I>>),
    GameWorld(I, W),
    Disconnect(String),
}

#[allow(clippy::large_enum_variant)]
enum ClientState<W, I> {
    Connecting,
    Downloading {
        id: AuthentID,
        wr: WorldReceive<W>,
    },
    CatchingUp {
        id: AuthentID,
        consumed_frame: Frame,
        next_inputs: Option<Vec<FrameInputs<I>>>,
    },
    Playing {
        id: AuthentID,
        buffer: ClientPlayoutBuffer,
        final_inputs: Option<Vec<FrameInputs<I>>>,
    },
    Disconnected {
        reason: String,
    },
}

pub struct Client<WORLD: DeserializeOwned, INPUT: Serialize + DeserializeOwned + Default> {
    net: ConnectionClient,

    name: String,
    version: String,

    state: ClientState<WORLD, INPUT>,

    pub step: Timestep,
    lag_compensate: u64,

    _phantom: PhantomSendSync<(INPUT, WORLD)>,
}

pub struct ConnectConf {
    pub name: String,
    pub addr: IpAddr,
    pub port: Option<u16>,
    pub frame_buffer_advance: u64,
    pub version: String,
}

impl<W: DeserializeOwned, I: Serialize + DeserializeOwned + Default> Client<W, I> {
    pub fn connect(conf: ConnectConf) -> Result<Self, ConnectionsError> {
        let addr = conf.addr;
        let port = conf.port.unwrap_or(DEFAULT_PORT);
        let saddr = SocketAddr::new(addr, port);

        let net = ConnectionClient::new(saddr)?;

        Ok(Self {
            net,
            state: ClientState::Connecting,
            name: conf.name,
            lag_compensate: conf.frame_buffer_advance,
            step: Timestep::default(),
            _phantom: Default::default(),
            version: crate::wire_version(&conf.version),
        })
    }

    #[allow(clippy::collapsible_if)]
    pub fn poll(&mut self, input: I) -> PollResult<W, I> {
        //log::info!("{:?}", &self.state);
        if self.net.is_disconnected() {
            if !matches!(self.state, ClientState::Disconnected { .. }) {
                self.state = ClientState::Disconnected {
                    reason: "connection lost".to_string(),
                };
            }
        }

        loop {
            let v = self.net.recv_tcp();
            if v.is_empty() {
                break;
            }
            for data in v {
                if let Some(packet) = decode(&data) {
                    let _ = self.message_reliable(packet);
                } else {
                    log::error!("could not decode reliable packet from server");
                }
            }
        }

        while let Some(data) = self.net.recv_udp() {
            if let Some(packet) = decode(&data) {
                self.message_unreliable(packet);
            } else {
                log::error!("could not decode unreliable packet from server");
            }
        }

        match self.state {
            ClientState::Disconnected { ref reason } => {
                return PollResult::Disconnect(reason.clone());
            }
            ClientState::Connecting => {
                return PollResult::Wait(input);
            }
            ClientState::Downloading {
                wr: WorldReceive::Errored,
                ..
            } => {
                let reason = "could not decode world packet".to_string();
                self.state = ClientState::Disconnected {
                    reason: reason.clone(),
                };
                return PollResult::Disconnect(reason);
            }
            ClientState::Downloading {
                wr: WorldReceive::Finished { frame, .. },
                id,
            } => {
                let s = std::mem::replace(
                    &mut self.state,
                    ClientState::CatchingUp {
                        next_inputs: None,
                        id,
                        consumed_frame: frame,
                    },
                );

                if let ClientState::Downloading {
                    wr: WorldReceive::Finished { world, .. },
                    ..
                } = s
                {
                    self.net
                        .send_tcp(encode(&ClientReliablePacket::BeginCatchUp));
                    return PollResult::GameWorld(input, world);
                } else {
                    unreachable!()
                }
            }
            ClientState::Downloading { .. } => {}
            ClientState::CatchingUp {
                ref mut next_inputs,
                ..
            } => {
                if let Some(x) = next_inputs.take() {
                    log::info!("{} catching up consumed inputs, asking for more", self.name);
                    self.net.send_tcp(encode(&ClientReliablePacket::CatchUpAck));
                    return PollResult::Input(x);
                }
                return PollResult::Wait(input);
            }
            ClientState::Playing {
                ref mut buffer,
                ref mut final_inputs,
                id,
            } => {
                if let Some(inputs) = final_inputs.take() {
                    log::info!("{} catching up final inputs, ready to play", self.name);
                    return PollResult::Input(inputs);
                }

                self.step.prepare_frame(1);
                if !self.step.tick() {
                    return PollResult::Wait(input);
                }

                let mut inp = Some(&input);
                let mut mk_input = || {
                    let d = Default::default();
                    let v = inp.take().unwrap_or(&d);
                    let serialized = encode(&v);
                    PlayerInput(serialized)
                };

                let advance = buffer.advance();

                let fba = self.lag_compensate.max(1);
                let to_consume = match advance {
                    0 => 0,
                    _ if (1..=fba).contains(&advance) => 1,
                    _ if (fba + 1..=fba * 2).contains(&advance) => 2,
                    _ if (fba * 2 + 1..=fba * 3).contains(&advance) => 3,
                    _ => advance - fba * 3,
                };

                if to_consume > 0 {
                    assert!(to_consume <= advance);

                    let net = &mut self.net;

                    let multi: Vec<_> = (0..to_consume)
                        .map(move |_| {
                            // unwrap ok: to_consume must be less than advance
                            let (inp, pack) = buffer.try_consume(&mut mk_input).unwrap();
                            net.send_udp(encode(&ClientUnreliablePacket::Input { input: pack }));
                            decode_merged(id, inp, buffer.consumed_frame())
                        })
                        .collect();
                    //log::info!("consuming {:?} inputs from unreliable channel", multi.len());
                    return PollResult::Input(multi);
                }
            }
        }

        PollResult::Wait(input)
    }

    fn message_reliable(&mut self, p: ServerReliablePacket) -> Option<()> {
        match p {
            ServerReliablePacket::WorldSend(fragment) => {
                log::info!("{}: received world fragment", self.name);

                if let ClientState::Downloading { ref mut wr, .. } = self.state {
                    wr.handle(fragment, &self.net);
                } else {
                    log::error!("received world but was not downloading.. weird");
                }
            }
            ServerReliablePacket::Challenge(challenge) => {
                log::info!("{}: received challenge", self.name);
                self.net
                    .send_udp(encode(&ClientUnreliablePacket::Connection(challenge)));
            }
            ServerReliablePacket::AuthentResponse(r) => match r {
                AuthentResponse::Accepted { id, period: step } => {
                    log::info!(
                        "{}: authent response is accepted. asking for world",
                        self.name
                    );
                    self.state = ClientState::Downloading {
                        wr: WorldReceive::default(),
                        id,
                    };
                    self.step = Timestep::new(step);
                    self.net.send_tcp(encode(&ClientReliablePacket::WorldAck));
                }
                AuthentResponse::Refused { reason } => {
                    log::error!("authent refused :( reason: {}", reason);
                    self.state = ClientState::Disconnected { reason };
                }
            },
            ServerReliablePacket::CatchUp { inputs } => {
                log::info!("{}: received catch up inputs", self.name);

                if let ClientState::CatchingUp {
                    ref mut next_inputs,
                    ref mut consumed_frame,
                    id,
                } = self.state
                {
                    if !next_inputs.is_none() {
                        log::error!(
                            "some inputs were not catched up before receiving other ones!!!"
                        );
                    }
                    log::info!("{:?} + {:?}", *consumed_frame, inputs.len());
                    *next_inputs = Some(
                        inputs
                            .into_iter()
                            .map(|v| {
                                consumed_frame.incr();
                                decode_merged(id, v, *consumed_frame)
                            })
                            .collect(),
                    );
                } else {
                    log::error!("received catching up inputs but was not catching up.. weird");
                }
            }
            ServerReliablePacket::ReadyToPlay {
                final_consumed_frame,
                final_inputs,
            } => {
                log::info!(
                    "{}: received ready to play on {:?}",
                    self.name,
                    final_consumed_frame
                );
                if let ClientState::CatchingUp {
                    next_inputs: None,
                    id,
                    mut consumed_frame,
                } = self.state
                {
                    assert_eq!(
                        final_consumed_frame,
                        Frame(consumed_frame.0 + final_inputs.len() as u64)
                    );
                    self.state = ClientState::Playing {
                        id,
                        buffer: ClientPlayoutBuffer::new(final_consumed_frame, 3),
                        final_inputs: Some(
                            final_inputs
                                .into_iter()
                                .map(|v| {
                                    consumed_frame.incr();
                                    decode_merged(id, v, consumed_frame)
                                })
                                .collect(),
                        ),
                    };
                } else {
                    log::error!(
                        "received ready to play but was still catching up or connecting.. weird"
                    );
                }
            }
        }
        None
    }

    fn message_unreliable(&mut self, p: ServerUnreliablePacket) {
        match p {
            ServerUnreliablePacket::Input(inp) => {
                if let ClientState::Playing {
                    ref mut buffer,
                    final_inputs: None,
                    ..
                } = self.state
                {
                    for (frame, inp) in inp {
                        let _ = buffer.insert_serv_input(frame, inp);
                    }
                }
            }
            ServerUnreliablePacket::ReadyForAuth => {
                log::info!("{}: received ready for auth", self.name);
                let connect = ClientReliablePacket::Connect {
                    name: self.name.clone(),
                    version: self.version.clone(),
                };
                self.net.send_tcp(encode(&connect));
            }
        }
    }

    pub fn describe(&self) -> String {
        match self.state {
            ClientState::Connecting => "Connecting...".to_string(),
            ClientState::Downloading { ref wr, .. } => {
                if let Some((cur, total)) = wr.progress() {
                    format!(
                        "Downloading map... {:.1}M/{:.1}M",
                        (cur as f32) / 1000000.0,
                        (total as f32) / 1000000.0
                    )
                } else {
                    "Downloading map...".to_string()
                }
            }
            ClientState::CatchingUp { .. } => "Catching up...".to_string(),
            ClientState::Playing {
                buffer: ref buf, ..
            } => {
                format!("Playing! Buffer advance: {}", buf.advance())
            }
            ClientState::Disconnected { ref reason } => reason.clone(),
        }
    }
}

#[cfg(test)]
#[test]
fn p7_ingress_rejects_released_p6_handshake() {
    assert_legacy_handshake_rejected("0.11.1");
}

#[cfg(test)]
#[test]
fn pathpark_w3_rejects_commands_4_handshake() {
    assert_legacy_handshake_rejected("0.12.0+commands.4");
}

#[cfg(test)]
fn assert_legacy_handshake_rejected(floor: &str) {
    use crate::{Server, ServerConfiguration, ServerPollResult};
    use std::net::{Ipv4Addr, TcpListener};
    use std::time::{Duration, Instant};

    let release = include_str!("../../../VERSION");
    let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut server = Server::<u64, Vec<u8>>::start(ServerConfiguration {
        start_frame: Frame(0),
        period: Duration::from_millis(20),
        port: Some(port),
        virtual_client: None,
        version: release.into(),
        always_run: false,
    })
    .unwrap();
    let mut peer = Client::<u64, Vec<u8>>::connect(ConnectConf {
        name: "floor".into(),
        addr: Ipv4Addr::LOCALHOST.into(),
        port: Some(port),
        frame_buffer_advance: 3,
        version: floor.into(),
    })
    .unwrap();
    // Reproduce the older value sent verbatim in the Connect packet.
    peer.version = floor.into();

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "floor peer got no verdict");
        assert!(matches!(
            server.poll(&19, Frame(0), None),
            ServerPollResult::Wait(None)
        ));
        match peer.poll(Vec::new()) {
            PollResult::Disconnect(reason) => {
                assert!(reason.contains("Incompatible versions"), "{reason}");
                break;
            }
            PollResult::Wait(_) => {}
            _ => panic!("floor peer passed authentication"),
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
