use std::fmt;

use crate::NewGameError;

/// Converts simulated agents to represented people at display and reporting boundaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PopulationScale(u16);

impl PopulationScale {
    pub const REALISTIC: Self = Self(1);
    pub const PRESETS: [u16; 5] = common::population::PERSONS_PER_AGENT_PRESETS;

    pub fn try_new(n: u16) -> Result<Self, NewGameError> {
        if Self::PRESETS.contains(&n) {
            Ok(Self(n))
        } else {
            Err(NewGameError::PersonsPerAgent(n))
        }
    }

    pub const fn persons_per_agent(self) -> u16 {
        self.0
    }

    pub fn persons(self, agents: usize) -> u64 {
        u64::try_from(agents)
            .unwrap_or(u64::MAX)
            .saturating_mul(u64::from(self.0))
    }
}

impl Default for PopulationScale {
    fn default() -> Self {
        Self::REALISTIC
    }
}

impl fmt::Display for PopulationScale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if *self == Self::REALISTIC {
            f.write_str("Realistic (1:1)")
        } else {
            write!(f, "Scaled (1:{})", self.0)
        }
    }
}
