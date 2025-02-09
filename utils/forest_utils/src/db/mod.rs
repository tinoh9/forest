// Copyright 2019-2023 ChainSafe Systems
// SPDX-License-Identifier: Apache-2.0, MIT

pub mod file_backed_obj;

use async_trait::async_trait;
use chrono::Utc;
use cid::{
    multihash::{Code, MultihashDigest},
    Cid,
};
use fvm_ipld_blockstore::Blockstore;
use fvm_ipld_encoding::{from_slice, to_vec, DAG_CBOR};
use human_repr::HumanCount;
use log::info;
use serde::{de::DeserializeOwned, ser::Serialize};

/// DB key size in bytes for estimating reachable data size. Use parity-db value
/// for simplicity. The actual value for other underlying DB might be slightly
/// different but that is negligible for calculating the total reachable data
/// size
pub const DB_KEY_BYTES: usize = 32;
/// Extension methods for inserting and retrieving IPLD data with CIDs
pub trait BlockstoreExt: Blockstore {
    /// Get typed object from block store by CID
    fn get_obj<T>(&self, cid: &Cid) -> anyhow::Result<Option<T>>
    where
        T: DeserializeOwned,
    {
        match self.get(cid)? {
            Some(bz) => Ok(Some(from_slice(&bz)?)),
            None => Ok(None),
        }
    }

    /// Put an object in the block store and return the Cid identifier.
    fn put_obj<S>(&self, obj: &S, code: Code) -> anyhow::Result<Cid>
    where
        S: Serialize,
    {
        let bytes = to_vec(obj)?;
        self.put_raw(bytes, code)
    }

    /// Put raw bytes in the block store and return the Cid identifier.
    fn put_raw(&self, bytes: Vec<u8>, code: Code) -> anyhow::Result<Cid> {
        let cid = Cid::new_v1(DAG_CBOR, code.digest(&bytes));
        self.put_keyed(&cid, &bytes)?;
        Ok(cid)
    }

    /// Batch put CBOR objects into block store and returns vector of CIDs
    fn bulk_put<'a, S, V>(&self, values: V, code: Code) -> anyhow::Result<Vec<Cid>>
    where
        Self: Sized,
        S: Serialize + 'a,
        V: IntoIterator<Item = &'a S>,
    {
        let keyed_objects = values
            .into_iter()
            .map(|value| {
                let bytes = to_vec(value)?;
                let cid = Cid::new_v1(DAG_CBOR, code.digest(&bytes));
                Ok((cid, bytes))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        let cids = keyed_objects
            .iter()
            .map(|(cid, _)| cid.to_owned())
            .collect();

        self.put_many_keyed(keyed_objects)?;

        Ok(cids)
    }
}

impl<T: fvm_ipld_blockstore::Blockstore> BlockstoreExt for T {}

/// Extension methods for buffered write with manageable limit of RAM usage
#[async_trait]
pub trait BlockstoreBufferedWriteExt: Blockstore + Sized {
    async fn buffered_write(
        &self,
        rx: flume::Receiver<(Cid, Vec<u8>)>,
        buffer_capacity_bytes: usize,
    ) -> anyhow::Result<()> {
        let start = Utc::now();
        let mut total_bytes = 0;
        let mut total_entries = 0;
        let mut estimated_buffer_bytes = 0;
        let mut buffer = vec![];
        while let Ok((key, value)) = rx.recv_async().await {
            // Key is stored in 32 bytes in paritydb
            estimated_buffer_bytes += DB_KEY_BYTES + value.len();
            total_bytes += DB_KEY_BYTES + value.len();
            total_entries += 1;
            buffer.push((key, value));
            if estimated_buffer_bytes >= buffer_capacity_bytes {
                self.put_many_keyed(std::mem::take(&mut buffer))?;
                estimated_buffer_bytes = 0;
            }
        }
        self.put_many_keyed(buffer)?;
        info!(
            "Buffered write completed: total entries: {total_entries}, total size: {}, took: {}s",
            total_bytes.human_count_bytes(),
            (Utc::now() - start).num_seconds()
        );

        Ok(())
    }
}

impl<T: fvm_ipld_blockstore::Blockstore> BlockstoreBufferedWriteExt for T {}
