// Copyright 2019-2023 ChainSafe Systems
// SPDX-License-Identifier: Apache-2.0, MIT

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use cid::{multihash::Code::Blake2b256, Cid};
use forest_blocks::{Block, FullTipset, Tipset, TxMeta};
use forest_chain::ChainStore;
use forest_message::SignedMessage;
use forest_shim::message::Message;
use forest_utils::db::BlockstoreExt;
use fvm_ipld_amt::{Amtv0 as Amt, Error as IpldAmtError};
use fvm_ipld_blockstore::Blockstore;
use fvm_ipld_encoding::{Cbor, Error as EncodingError};
use thiserror::Error;

use crate::bad_block_cache::BadBlockCache;

const MAX_HEIGHT_DRIFT: u64 = 5;

#[derive(Debug, Error)]
pub enum TipsetValidationError {
    #[error("Tipset has no blocks")]
    NoBlocks,
    #[error("Tipset has an epoch that is too large")]
    EpochTooLarge,
    #[error("Tipset has an insufficient weight")]
    InsufficientWeight,
    #[error("Tipset block = [CID = {0}] is invalid: {1}")]
    InvalidBlock(Cid, String),
    #[error("Tipset headers are invalid")]
    InvalidRoots,
    #[error("Tipset IPLD error: {0}")]
    IpldAmt(String),
    #[error("Block store error while validating tipset: {0}")]
    Blockstore(String),
    #[error("Encoding error while validating tipset: {0}")]
    Encoding(EncodingError),
}

impl From<EncodingError> for Box<TipsetValidationError> {
    fn from(err: EncodingError) -> Self {
        Box::new(TipsetValidationError::Encoding(err))
    }
}

impl From<IpldAmtError> for Box<TipsetValidationError> {
    fn from(err: IpldAmtError) -> Self {
        Box::new(TipsetValidationError::IpldAmt(err.to_string()))
    }
}

pub struct TipsetValidator<'a>(pub &'a FullTipset);

impl<'a> TipsetValidator<'a> {
    pub fn validate<DB: Blockstore>(
        &self,
        chainstore: Arc<ChainStore<DB>>,
        bad_block_cache: Arc<BadBlockCache>,
        genesis_tipset: Arc<Tipset>,
        block_delay: u64,
    ) -> Result<(), Box<TipsetValidationError>> {
        // No empty blocks
        if self.0.blocks().is_empty() {
            return Err(Box::new(TipsetValidationError::NoBlocks));
        }

        // Tipset epoch must not be behind current max
        self.validate_epoch(genesis_tipset, block_delay)?;

        // Validate each block in the tipset by:
        // 1. Calculating the message root using all of the messages to ensure it
        // matches the mst root in the block header 2. Ensuring it has not
        // previously been seen in the bad blocks cache
        for block in self.0.blocks() {
            self.validate_msg_root(&chainstore.db, block)?;
            if let Some(bad) = bad_block_cache.peek(block.cid()) {
                return Err(Box::new(TipsetValidationError::InvalidBlock(
                    *block.cid(),
                    bad,
                )));
            }
        }

        Ok(())
    }

    pub fn validate_epoch(
        &self,
        genesis_tipset: Arc<Tipset>,
        block_delay: u64,
    ) -> Result<(), Box<TipsetValidationError>> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let max_epoch = ((now - genesis_tipset.min_timestamp()) / block_delay) + MAX_HEIGHT_DRIFT;
        let too_far_ahead_in_time = self.0.epoch() as u64 > max_epoch;
        if too_far_ahead_in_time {
            Err(Box::new(TipsetValidationError::EpochTooLarge))
        } else {
            Ok(())
        }
    }

    pub fn validate_msg_root<DB: Blockstore>(
        &self,
        blockstore: &DB,
        block: &Block,
    ) -> Result<(), Box<TipsetValidationError>> {
        let msg_root = Self::compute_msg_root(blockstore, block.bls_msgs(), block.secp_msgs())?;
        if block.header().messages() != &msg_root {
            Err(Box::new(TipsetValidationError::InvalidRoots))
        } else {
            Ok(())
        }
    }

    pub fn compute_msg_root<DB: Blockstore>(
        blockstore: &DB,
        bls_msgs: &[Message],
        secp_msgs: &[SignedMessage],
    ) -> Result<Cid, Box<TipsetValidationError>> {
        // Generate message CIDs
        let bls_cids = bls_msgs
            .iter()
            .map(Cbor::cid)
            .collect::<Result<Vec<Cid>, fvm_ipld_encoding::Error>>()?;
        let secp_cids = secp_msgs
            .iter()
            .map(Cbor::cid)
            .collect::<Result<Vec<Cid>, fvm_ipld_encoding::Error>>()?;

        // Generate Amt and batch set message values
        let bls_message_root = Amt::new_from_iter(blockstore, bls_cids)?;
        let secp_message_root = Amt::new_from_iter(blockstore, secp_cids)?;
        let meta = TxMeta {
            bls_message_root,
            secp_message_root,
        };

        // Store message roots and receive meta_root CID
        blockstore
            .put_obj(&meta, Blake2b256)
            .map_err(|e| Box::new(TipsetValidationError::Blockstore(e.to_string())))
    }
}
