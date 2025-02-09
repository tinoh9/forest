// Copyright 2019-2023 ChainSafe Systems
// SPDX-License-Identifier: Apache-2.0, MIT

#![recursion_limit = "1024"]

mod behaviour;
pub mod chain_exchange;
mod config;
mod discovery;
mod gossip_params;
pub mod hello;
mod metrics;
mod peer_manager;
pub mod rpc;
mod service;

// Re-export some libp2p types
pub use libp2p::{
    identity::{ed25519, Keypair, PeerId},
    multiaddr::{Multiaddr, Protocol},
};
pub use multihash::Multihash;

pub(crate) use self::behaviour::*;
pub use self::{config::*, peer_manager::*, service::*};
