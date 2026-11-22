use crate::utils::scheduler::SeqSchedule;
use crate::world_command::WorldCommand;
use crate::{Simulation, SimulationOptions, INITIAL_TICK};
use common::saveload::{Encoder, JSON};
use prototypes::Tick;
use serde::{Deserialize, Serialize};
use std::io;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Replay {
    #[serde(default)]
    pub version: String,
    pub enabled: bool,
    commands: Vec<(Tick, WorldCommand)>,
    pub last_tick_recorded: Tick,
}

impl Default for Replay {
    fn default() -> Self {
        Self {
            version: crate::VERSION.to_owned(),
            enabled: false,
            commands: Vec::new(),
            last_tick_recorded: Tick::default(),
        }
    }
}

impl Replay {
    pub(crate) fn bootstrap_options(&self) -> io::Result<SimulationOptions> {
        match self.commands.first() {
            Some((Tick(INITIAL_TICK), WorldCommand::Init(options))) => Ok(**options),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Replay must begin with Init at the initial tick",
            )),
        }
    }

    pub fn push(&mut self, tick: Tick, command: WorldCommand) {
        self.commands.push((tick, command));
    }
}

pub(crate) struct ReplayJson;

impl Encoder for ReplayJson {
    const EXTENSION: &'static str = JSON::EXTENSION;

    fn encode(replay: &impl Serialize) -> io::Result<Vec<u8>> {
        JSON::encode(replay)
    }

    fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> io::Result<T> {
        #[derive(Deserialize)]
        struct ReplayHeader {
            #[serde(default)]
            version: String,
        }

        // Decode only the version on the first pass, regardless of JSON field order.
        let ReplayHeader { version } = JSON::decode(bytes)?;
        if version != crate::VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Replay has version {version:?}, expected {:?}",
                    crate::VERSION
                ),
            ));
        }
        JSON::decode(bytes)
    }
}

pub struct SimulationReplayLoader {
    pub replay: Replay,
    pub pastt: Tick,
    pub idx: usize,
    pub speed: usize,
    pub advance_n_ticks: usize,
}

impl SimulationReplayLoader {
    /// Returns true if the replay is finished
    pub fn advance_tick(&mut self, sim: &mut Simulation, schedule: &mut SeqSchedule) -> bool {
        // iterate through tick grouped commands
        let mut ticks_left = if self.speed == 0 {
            let v = self.advance_n_ticks;
            self.advance_n_ticks = 0;
            v
        } else {
            self.speed
        };
        while self.idx < self.replay.commands.len() && ticks_left > 0 {
            let curt = self.replay.commands[self.idx].0;
            while self.pastt < curt {
                sim.tick(schedule, &[]);
                self.pastt.0 += 1;
                ticks_left -= 1;
                if ticks_left == 0 {
                    return false;
                }
            }

            let idx_start = self.idx;
            while self.idx < self.replay.commands.len() && self.replay.commands[self.idx].0 == curt
            {
                self.idx += 1;
            }
            let command_slice = &self.replay.commands[idx_start..self.idx];

            log::info!(
                "[replay] acttick {:?} ({})",
                self.pastt,
                command_slice.len()
            );
            sim.tick(schedule, command_slice.iter().map(|(_, c)| c));
            self.pastt.0 += 1;
            ticks_left -= 1;
            if ticks_left == 0 {
                return false;
            }
        }
        if self.idx < self.replay.commands.len() {
            return false;
        }
        while ticks_left > 0 && self.pastt < self.replay.last_tick_recorded {
            sim.tick(schedule, &[]);
            self.pastt.0 += 1;
            ticks_left -= 1;
            if ticks_left == 0 {
                return false;
            }
        }
        true
    }
}
