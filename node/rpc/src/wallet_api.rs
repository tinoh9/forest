// Copyright 2019-2023 ChainSafe Systems
// SPDX-License-Identifier: Apache-2.0, MIT
#![allow(clippy::unused_async)]
use std::{convert::TryFrom, str::FromStr};

use base64::{prelude::BASE64_STANDARD, Engine};
use forest_beacon::Beacon;
use forest_json::{address::json::AddressJson, signature::json::SignatureJson};
use forest_key_management::{json::KeyInfoJson, Error, Key};
use forest_rpc_api::{data_types::RPCState, wallet_api::*};
use forest_shim::{address::Address, econ::TokenAmount, state_tree::StateTree};
use fvm_ipld_blockstore::Blockstore;
use jsonrpc_v2::{Data, Error as JsonRpcError, Params};
use num_traits::Zero;

/// Return the balance from `StateManager` for a given `Address`
pub(crate) async fn wallet_balance<DB, B>(
    data: Data<RPCState<DB, B>>,
    Params(params): Params<WalletBalanceParams>,
) -> Result<WalletBalanceResult, JsonRpcError>
where
    DB: Blockstore + Clone + Send + Sync + 'static,
    B: Beacon,
{
    let (addr_str,) = params;
    let address = Address::from_str(&addr_str)?;

    let heaviest_ts = data.state_manager.chain_store().heaviest_tipset();
    let cid = heaviest_ts.parent_state();

    let state = StateTree::new_from_root(data.state_manager.blockstore(), cid)?;
    match state.get_actor(&address) {
        Ok(act) => {
            if let Some(actor) = act {
                let actor_balance = &actor.balance;
                Ok(actor_balance.atto().to_string())
            } else {
                Ok(TokenAmount::zero().atto().to_string())
            }
        }
        Err(e) => Err(e.into()),
    }
}

/// Get the default Address for the Wallet
pub(crate) async fn wallet_default_address<DB, B>(
    data: Data<RPCState<DB, B>>,
) -> Result<WalletDefaultAddressResult, JsonRpcError>
where
    DB: Blockstore,
    B: Beacon,
{
    let keystore = data.keystore.read().await;

    let addr = forest_key_management::get_default(&keystore)?;
    Ok(addr.map(|s| s.to_string()))
}

/// Export `KeyInfo` from the Wallet given its address
pub(crate) async fn wallet_export<DB, B>(
    data: Data<RPCState<DB, B>>,
    Params(params): Params<WalletExportParams>,
) -> Result<WalletExportResult, JsonRpcError>
where
    DB: Blockstore,
    B: Beacon,
{
    let (addr_str,) = params;
    let addr = Address::from_str(&addr_str)?;

    let keystore = data.keystore.read().await;

    let key_info = forest_key_management::export_key_info(&addr, &keystore)?;
    Ok(KeyInfoJson(key_info))
}

/// Return whether or not a Key is in the Wallet
pub(crate) async fn wallet_has<DB, B>(
    data: Data<RPCState<DB, B>>,
    Params(params): Params<WalletHasParams>,
) -> Result<WalletHasResult, JsonRpcError>
where
    DB: Blockstore,
    B: Beacon,
{
    let (addr_str,) = params;
    let addr = Address::from_str(&addr_str)?;

    let keystore = data.keystore.read().await;

    let key = forest_key_management::find_key(&addr, &keystore).is_ok();
    Ok(key)
}

/// Import `KeyInfo` to the Wallet, return the Address that corresponds to it
pub(crate) async fn wallet_import<DB, B>(
    data: Data<RPCState<DB, B>>,
    Params(params): Params<WalletImportParams>,
) -> Result<WalletImportResult, JsonRpcError>
where
    DB: Blockstore,
    B: Beacon,
{
    let key_info: forest_key_management::KeyInfo = match params.first().cloned() {
        Some(key_info) => key_info.into(),
        None => return Err(JsonRpcError::INTERNAL_ERROR),
    };

    let key = Key::try_from(key_info)?;

    let addr = format!("wallet-{}", key.address);

    let mut keystore = data.keystore.write().await;

    if let Err(error) = keystore.put(addr, key.key_info) {
        match error {
            Error::KeyExists => Err(JsonRpcError::Provided {
                code: 1,
                message: "Key already exists",
            }),
            _ => Err(error.into()),
        }
    } else {
        Ok(key.address.to_string())
    }
}

/// List all Addresses in the Wallet
pub(crate) async fn wallet_list<DB, B>(
    data: Data<RPCState<DB, B>>,
) -> Result<WalletListResult, JsonRpcError>
where
    DB: Blockstore,
    B: Beacon,
{
    let keystore = data.keystore.read().await;
    Ok(forest_key_management::list_addrs(&keystore)?
        .into_iter()
        .map(AddressJson::from)
        .collect())
}

/// Generate a new Address that is stored in the Wallet
pub(crate) async fn wallet_new<DB, B>(
    data: Data<RPCState<DB, B>>,
    Params(params): Params<WalletNewParams>,
) -> Result<WalletNewResult, JsonRpcError>
where
    DB: Blockstore,
    B: Beacon,
{
    let (sig_raw,) = params;
    let mut keystore = data.keystore.write().await;
    let key = forest_key_management::generate_key(sig_raw.0)?;

    let addr = format!("wallet-{}", key.address);
    keystore.put(addr, key.key_info.clone())?;
    let value = keystore.get("default");
    if value.is_err() {
        keystore.put("default".to_string(), key.key_info)?
    }

    Ok(key.address.to_string())
}

/// Set the default Address for the Wallet
pub(crate) async fn wallet_set_default<DB, B>(
    data: Data<RPCState<DB, B>>,
    Params(params): Params<WalletSetDefaultParams>,
) -> Result<WalletSetDefaultResult, JsonRpcError>
where
    DB: Blockstore,
    B: Beacon,
{
    let (address,) = params;
    let mut keystore = data.keystore.write().await;

    let addr_string = format!("wallet-{}", address.0);
    let key_info = keystore.get(&addr_string)?;
    keystore.remove("default".to_string())?; // This line should unregister current default key then continue
    keystore.put("default".to_string(), key_info)?;
    Ok(())
}

/// Sign a vector of bytes
pub(crate) async fn wallet_sign<DB, B>(
    data: Data<RPCState<DB, B>>,
    Params(params): Params<WalletSignParams>,
) -> Result<WalletSignResult, JsonRpcError>
where
    DB: Blockstore + Clone + Send + Sync + 'static,
    B: Beacon,
{
    let state_manager = &data.state_manager;
    let (addr, msg_string) = params;
    let address = addr.0;
    let heaviest_tipset = data.state_manager.chain_store().heaviest_tipset();
    let key_addr = state_manager
        .resolve_to_key_addr(&address, &heaviest_tipset)
        .await?;
    let keystore = &mut *data.keystore.write().await;
    let key = match forest_key_management::find_key(&key_addr, keystore) {
        Ok(key) => key,
        Err(_) => {
            let key_info = forest_key_management::try_find(&key_addr, keystore)?;
            Key::try_from(key_info)?
        }
    };

    let sig = forest_key_management::sign(
        *key.key_info.key_type(),
        key.key_info.private_key(),
        &BASE64_STANDARD.decode(msg_string)?,
    )?;

    Ok(SignatureJson(sig))
}

/// Verify a Signature, true if verified, false otherwise
pub(crate) async fn wallet_verify<DB, B>(
    _data: Data<RPCState<DB, B>>,
    Params(params): Params<WalletVerifyParams>,
) -> Result<WalletVerifyResult, JsonRpcError>
where
    DB: Blockstore,
    B: Beacon,
{
    let (addr, msg, SignatureJson(sig)) = params;
    let address = addr.0;

    let ret = sig.verify(&msg, &address.into()).is_ok();
    Ok(ret)
}
