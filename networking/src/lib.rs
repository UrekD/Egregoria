#![allow(clippy::uninlined_format_args)]

use crate::authent::AuthentID;
use common::saveload::{CompressedBincode, Encoder, COMMAND_PROTOCOL_REVISION};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::ops::Add;

mod authent;
mod catchup;
mod client;
mod connection_client;
mod connections;
mod packets;
mod ring;
mod server;
mod worldsend;

use crate::client::FrameInputs;
pub use client::{Client, ConnectConf, PollResult, ServerInput};
pub use server::{Server, ServerConfiguration, ServerPollResult, VirtualClientConf};

pub(crate) const MAX_WORLDSEND_PACKET_SIZE: usize = 262144; //32 ko at least 1.3Mo per s at 50FPS
pub(crate) const DEFAULT_PORT: u16 = 23019;
const MAX_PLAYER_INPUT_BYTES: usize = 512 * 1024;

pub(crate) fn wire_version(release: &str) -> String {
    format!("{release}+{COMMAND_PROTOCOL_REVISION}")
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Copy, Clone, Hash, Debug, Serialize, Deserialize)]
#[repr(transparent)]
pub struct Frame(pub u64);

#[derive(PartialEq, Eq, PartialOrd, Ord, Copy, Clone, Hash, Serialize, Deserialize)]
#[repr(transparent)]
pub(crate) struct UserID(pub u32);

#[derive(Clone, Serialize, Deserialize, Debug)]
#[repr(transparent)]
pub(crate) struct PlayerInput(pub Vec<u8>);

pub(crate) type MergedInputs = Vec<(AuthentID, PlayerInput)>;

impl Add for Frame {
    type Output = Self;
    #[inline]
    fn add(self, rhs: Self) -> Self::Output {
        Self(self.0 + rhs.0)
    }
}

impl Frame {
    pub fn incr(&mut self) {
        self.0 += 1
    }
    pub fn decr(&mut self) {
        self.0 -= 1
    }
    pub fn incred(self) -> Self {
        Self(self.0 + 1)
    }
    pub fn decred(self) -> Self {
        Self(self.0 - 1)
    }
}

type Enc = CompressedBincode;

pub(crate) fn try_encode<T: Serialize>(x: &T) -> Option<Vec<u8>> {
    Enc::encode(x).ok()
}

pub(crate) fn encode<T: Serialize>(x: &T) -> Vec<u8> {
    try_encode(x).expect("failed serializing")
}

pub(crate) fn decode<T: DeserializeOwned>(x: &[u8]) -> Option<T> {
    Enc::decode(x).ok()
}

pub(crate) fn decode_player_input<T: DeserializeOwned>(input: &PlayerInput) -> Option<T> {
    let bytes = &input.0;
    if bytes.len() > MAX_PLAYER_INPUT_BYTES {
        return None;
    }
    Enc::decode_limited(bytes, MAX_PLAYER_INPUT_BYTES).ok()
}

pub(crate) struct PhantomSendSync<T>(PhantomData<T>);

unsafe impl<T> Send for PhantomSendSync<T> {}
unsafe impl<T> Sync for PhantomSendSync<T> {}

impl<T> Default for PhantomSendSync<T> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

pub(crate) fn hash_str(s: &str) -> u32 {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish() as u32
}

fn decode_merged<I: DeserializeOwned>(
    me: AuthentID,
    x: MergedInputs,
    frame: Frame,
) -> FrameInputs<I> {
    FrameInputs {
        frame,
        inputs: x
            .into_iter()
            .flat_map(|(id, x)| {
                Some(ServerInput {
                    sender: id.0,
                    sent_by_me: id == me,
                    inp: decode_player_input(&x)?,
                })
            })
            .collect(),
    }
}

#[cfg(test)]
#[test]
fn p7_ingress_limits_inflated_player_input() {
    let permitted = vec![0u8; MAX_PLAYER_INPUT_BYTES / 2];
    assert_eq!(
        decode_player_input::<Vec<u8>>(&PlayerInput(encode(&permitted))),
        Some(permitted),
    );
    let excessive = PlayerInput(encode(&vec![0u8; MAX_PLAYER_INPUT_BYTES]));
    assert!(excessive.0.len() < MAX_PLAYER_INPUT_BYTES);
    assert!(decode_player_input::<Vec<u8>>(&excessive).is_none());
}
