// Copyright 2019-2023 ChainSafe Systems
// SPDX-License-Identifier: Apache-2.0, MIT

use std::sync::Arc;

use cid::Cid;
use fil_actor_interface::power;
use fil_actors_runtime_v9::runtime::DomainSeparationTag;
use forest_beacon::{Beacon, BeaconEntry, BeaconSchedule, IGNORE_DRAND_VAR};
use forest_blocks::{Block, BlockHeader, Tipset};
use forest_chain_sync::collect_errs;
use forest_fil_types::verifier::verify_winning_post;
use forest_networks::{ChainConfig, Height};
use forest_shim::{address::Address, randomness::Randomness, version::NetworkVersion};
use forest_state_manager::StateManager;
use futures::stream::FuturesUnordered;
use fvm_ipld_blockstore::Blockstore;
use fvm_ipld_encoding::Cbor;
use fvm_shared::{crypto::signature::ops::verify_bls_sig, TICKET_RANDOMNESS_LOOKBACK};
use nonempty::NonEmpty;

use crate::{metrics, FilecoinConsensusError};

fn to_errs<E: Into<FilecoinConsensusError>>(e: E) -> NonEmpty<FilecoinConsensusError> {
    NonEmpty::new(e.into())
}

/// Validates block semantically according to https://github.com/filecoin-project/specs/blob/6ab401c0b92efb6420c6e198ec387cf56dc86057/validation.md
/// Returns all encountered errors, so they can be merged with the common
/// validations performed by the synchronizer.
///
/// Validation includes:
/// * Sanity checks
/// * Timestamps
/// * Elections and Proof-of-SpaceTime, Beacon values
pub(crate) async fn validate_block<DB: Blockstore + Clone + Sync + Send + 'static, B: Beacon>(
    state_manager: Arc<StateManager<DB>>,
    beacon_schedule: Arc<BeaconSchedule<B>>,
    block: Arc<Block>,
) -> Result<(), NonEmpty<FilecoinConsensusError>> {
    let _timer = metrics::CONSENSUS_BLOCK_VALIDATION_TIME.start_timer();

    let chain_store = state_manager.chain_store().clone();
    let header = block.header();

    block_sanity_checks(header).map_err(to_errs)?;

    let base_tipset = chain_store
        .tipset_from_keys(header.parents())
        .map_err(to_errs)?;

    block_timestamp_checks(
        header,
        base_tipset.as_ref(),
        state_manager.chain_config().as_ref(),
    )
    .map_err(to_errs)?;

    let win_p_nv = state_manager.get_network_version(base_tipset.epoch());

    // Retrieve lookback tipset for validation
    let (lookback_tipset, lookback_state) = state_manager
        .get_lookback_tipset_for_round(base_tipset.clone(), block.header().epoch())
        .map_err(to_errs)?;

    let lookback_state = Arc::new(lookback_state);

    let prev_beacon = chain_store
        .latest_beacon_entry(&base_tipset)
        .map(Arc::new)
        .map_err(to_errs)?;

    // Work address needed for async validations, so necessary
    // to do sync to avoid duplication
    let work_addr = state_manager
        .get_miner_work_addr(*lookback_state, header.miner_address())
        .map_err(to_errs)?;

    // Async validations
    let validations = FuturesUnordered::new();

    // Miner validations
    let v_state_manager = state_manager.clone();
    let v_base_tipset = base_tipset.clone();
    let v_header = header.clone();
    validations.push(tokio::task::spawn_blocking(move || {
        validate_miner(
            v_state_manager.as_ref(),
            v_header.miner_address(),
            v_base_tipset.parent_state(),
        )
    }));

    // Winner election PoSt validations
    let v_block = Arc::clone(&block);
    let v_prev_beacon = Arc::clone(&prev_beacon);
    let v_base_tipset = Arc::clone(&base_tipset);
    let v_state_manager = Arc::clone(&state_manager);
    let v_lookback_state = lookback_state.clone();
    validations.push(tokio::task::spawn_blocking(move || {
        validate_winner_election(
            v_block.header(),
            v_base_tipset.as_ref(),
            lookback_tipset.as_ref(),
            v_lookback_state.as_ref(),
            v_prev_beacon.as_ref(),
            &work_addr,
            v_state_manager.as_ref(),
        )
    }));

    // Beacon values check
    if std::env::var(IGNORE_DRAND_VAR) != Ok("1".to_owned()) {
        let v_block = Arc::clone(&block);
        let parent_epoch = base_tipset.epoch();
        let v_prev_beacon = Arc::clone(&prev_beacon);
        validations.push(tokio::task::spawn(async move {
            v_block
                .header()
                .validate_block_drand(
                    win_p_nv,
                    beacon_schedule.as_ref(),
                    parent_epoch,
                    &v_prev_beacon,
                )
                .map_err(|e| FilecoinConsensusError::BeaconValidation(e.to_string()))
        }));
    }

    // Ticket election proof validations
    let v_block = Arc::clone(&block);
    let v_base_tipset = Arc::clone(&base_tipset);
    let v_prev_beacon = Arc::clone(&prev_beacon);
    let v_state_manager = Arc::clone(&state_manager);
    validations.push(tokio::task::spawn_blocking(move || {
        validate_ticket_election(
            v_block.header(),
            v_base_tipset.as_ref(),
            v_prev_beacon.as_ref(),
            &work_addr,
            v_state_manager.chain_config(),
        )
    }));

    // Winning PoSt proof validation
    let v_block = block.clone();
    let v_prev_beacon = Arc::clone(&prev_beacon);
    validations.push(tokio::task::spawn_blocking(move || {
        verify_winning_post_proof::<_>(
            &state_manager,
            win_p_nv,
            v_block.header(),
            &v_prev_beacon,
            &lookback_state,
        )?;
        Ok(())
    }));

    // Collect the errors from the async validations
    collect_errs(validations).await
}

/// Checks optional values in header.
///
/// In particular it looks for an election proof and a ticket,
/// which are needed for Filecoin consensus.
fn block_sanity_checks(header: &BlockHeader) -> Result<(), FilecoinConsensusError> {
    if header.election_proof().is_none() {
        return Err(FilecoinConsensusError::BlockWithoutElectionProof);
    }
    if header.ticket().is_none() {
        return Err(FilecoinConsensusError::BlockWithoutTicket);
    }
    Ok(())
}

/// Check the timestamp corresponds exactly to the number of epochs since the
/// parents.
fn block_timestamp_checks(
    header: &BlockHeader,
    base_tipset: &Tipset,
    chain_config: &ChainConfig,
) -> Result<(), FilecoinConsensusError> {
    // Timestamp checks
    let block_delay = chain_config.block_delay_secs;
    let nulls = (header.epoch() - (base_tipset.epoch() + 1)) as u64;
    let target_timestamp = base_tipset.min_timestamp() + block_delay * (nulls + 1);
    if target_timestamp != header.timestamp() {
        return Err(FilecoinConsensusError::UnequalBlockTimestamps(
            header.timestamp(),
            target_timestamp,
        ));
    }
    Ok(())
}

// Check that the miner power can be loaded.
// Doesn't check that the miner actually has any power.
fn validate_miner<DB: Blockstore + Clone + Send + Sync + 'static>(
    state_manager: &StateManager<DB>,
    miner_addr: &Address,
    tipset_state: &Cid,
) -> Result<(), FilecoinConsensusError> {
    let _timer = metrics::CONSENSUS_BLOCK_VALIDATION_TASKS_TIME
        .with_label_values(&[metrics::values::VALIDATE_MINER])
        .start_timer();

    let actor = state_manager
        .get_actor(&Address::POWER_ACTOR, *tipset_state)
        .map_err(|_| FilecoinConsensusError::PowerActorUnavailable)?
        .ok_or(FilecoinConsensusError::PowerActorUnavailable)?;

    let state = power::State::load(state_manager.blockstore(), &actor.into())
        .map_err(|err| FilecoinConsensusError::MinerPowerUnavailable(err.to_string()))?;

    state
        .miner_power(state_manager.blockstore(), &miner_addr.into())
        .map_err(|err| FilecoinConsensusError::MinerPowerUnavailable(err.to_string()))?;

    Ok(())
}

fn validate_winner_election<DB: Blockstore + Clone + Sync + Send + 'static>(
    header: &BlockHeader,
    base_tipset: &Tipset,
    lookback_tipset: &Tipset,
    lookback_state: &Cid,
    prev_beacon: &BeaconEntry,
    work_addr: &Address,
    state_manager: &StateManager<DB>,
) -> Result<(), FilecoinConsensusError> {
    let _timer = metrics::CONSENSUS_BLOCK_VALIDATION_TASKS_TIME
        .with_label_values(&[metrics::values::VALIDATE_WINNER_ELECTION])
        .start_timer();

    // Safe to unwrap because checked to `Some` in sanity check
    let election_proof = header.election_proof().as_ref().unwrap();
    if election_proof.win_count < 1 {
        return Err(FilecoinConsensusError::NotClaimingWin);
    }
    let hp =
        state_manager.eligible_to_mine(header.miner_address(), base_tipset, lookback_tipset)?;
    if !hp {
        return Err(FilecoinConsensusError::MinerNotEligibleToMine);
    }

    let beacon = header.beacon_entries().last().unwrap_or(prev_beacon);
    let miner_address = header.miner_address();
    let miner_address_buf = miner_address.marshal_cbor()?;

    let vrf_base = forest_state_manager::chain_rand::draw_randomness(
        beacon.data(),
        DomainSeparationTag::ElectionProofProduction as i64,
        header.epoch(),
        &miner_address_buf,
    )
    .map_err(|e| FilecoinConsensusError::DrawingChainRandomness(e.to_string()))?;

    verify_election_post_vrf(work_addr, &vrf_base, election_proof.vrfproof.as_bytes())?;

    if state_manager.is_miner_slashed(header.miner_address(), base_tipset.parent_state())? {
        return Err(FilecoinConsensusError::InvalidOrSlashedMiner);
    }

    let (mpow, tpow) = state_manager
        .get_power(lookback_state, Some(header.miner_address()))?
        .ok_or(FilecoinConsensusError::MinerPowerNotAvailable)?;

    let j = election_proof.compute_win_count(&mpow.quality_adj_power, &tpow.quality_adj_power);

    if election_proof.win_count != j {
        return Err(FilecoinConsensusError::MinerWinClaimsIncorrect(
            election_proof.win_count,
            j,
        ));
    }

    Ok(())
}

fn validate_ticket_election(
    header: &BlockHeader,
    base_tipset: &Tipset,
    prev_beacon: &BeaconEntry,
    work_addr: &Address,
    chain_config: &ChainConfig,
) -> Result<(), FilecoinConsensusError> {
    let _timer = metrics::CONSENSUS_BLOCK_VALIDATION_TASKS_TIME
        .with_label_values(&[metrics::values::VALIDATE_TICKET_ELECTION])
        .start_timer();

    let mut miner_address_buf = header.miner_address().marshal_cbor()?;
    let smoke_height = chain_config.epoch(Height::Smoke);

    if header.epoch() > smoke_height {
        let vrf_proof = base_tipset
            .min_ticket()
            .ok_or(FilecoinConsensusError::TipsetWithoutTicket)?
            .vrfproof
            .as_bytes();

        miner_address_buf.extend_from_slice(vrf_proof);
    }

    let beacon_base = header.beacon_entries().last().unwrap_or(prev_beacon);

    let vrf_base = forest_state_manager::chain_rand::draw_randomness(
        beacon_base.data(),
        DomainSeparationTag::TicketProduction as i64,
        header.epoch() - TICKET_RANDOMNESS_LOOKBACK,
        &miner_address_buf,
    )
    .map_err(|e| FilecoinConsensusError::DrawingChainRandomness(e.to_string()))?;

    verify_election_post_vrf(
        work_addr,
        &vrf_base,
        // Safe to unwrap here because of block sanity checks
        header.ticket().as_ref().unwrap().vrfproof.as_bytes(),
    )?;

    Ok(())
}

fn verify_election_post_vrf(
    worker: &Address,
    rand: &[u8],
    evrf: &[u8],
) -> Result<(), FilecoinConsensusError> {
    verify_bls_sig(evrf, rand, &worker.into()).map_err(FilecoinConsensusError::VrfValidation)
}

fn verify_winning_post_proof<DB: Blockstore + Clone + Send + Sync + 'static>(
    state_manager: &StateManager<DB>,
    network_version: NetworkVersion,
    header: &BlockHeader,
    prev_beacon_entry: &BeaconEntry,
    lookback_state: &Cid,
) -> Result<(), FilecoinConsensusError> {
    let _timer = metrics::CONSENSUS_BLOCK_VALIDATION_TASKS_TIME
        .with_label_values(&[metrics::values::VERIFY_WINNING_POST_PROOF])
        .start_timer();

    if cfg!(feature = "insecure_post") {
        let wpp = header.winning_post_proof();
        if wpp.is_empty() {
            return Err(FilecoinConsensusError::InsecurePostValidation(
                String::from("No winning PoSt proof provided"),
            ));
        }
        if wpp[0].proof_bytes == b"valid_proof" {
            return Ok(());
        }
        return Err(FilecoinConsensusError::InsecurePostValidation(
            String::from("Winning PoSt is invalid"),
        ));
    }

    let miner_addr_buf = header.miner_address().marshal_cbor()?;
    let rand_base = header
        .beacon_entries()
        .iter()
        .last()
        .unwrap_or(prev_beacon_entry);
    let rand = forest_state_manager::chain_rand::draw_randomness(
        rand_base.data(),
        DomainSeparationTag::WinningPoStChallengeSeed as i64,
        header.epoch(),
        &miner_addr_buf,
    )
    .map_err(|e| FilecoinConsensusError::DrawingChainRandomness(e.to_string()))?;

    let id = header.miner_address().id().map_err(|e| {
        FilecoinConsensusError::WinningPoStValidation(format!(
            "failed to get ID from miner address {}: {}",
            header.miner_address(),
            e
        ))
    })?;

    let sectors = state_manager
        .get_sectors_for_winning_post(
            lookback_state,
            network_version,
            header.miner_address(),
            Randomness::new(rand.to_vec()),
        )
        .map_err(|e| FilecoinConsensusError::WinningPoStValidation(e.to_string()))?;

    verify_winning_post(
        Randomness::new(rand.to_vec()).into(),
        header.winning_post_proof(),
        sectors.as_slice(),
        id,
    )
    .map_err(|e| FilecoinConsensusError::WinningPoStValidation(e.to_string()))
}
