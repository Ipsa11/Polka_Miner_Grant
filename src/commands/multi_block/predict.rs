//! Predict command implementation for election result prediction.

use crate::commands::multi_block::types::TargetSnapshotPageOf;
use crate::commands::multi_block::types::VoterSnapshotPageOf;
use std::sync::Arc;
use tokio::sync::Semaphore;
use crate::{
	client::Client,
	commands::types::PredictConfig,
	error::Error,
	prelude::{ AccountId, LOG_TARGET, Storage },
	runtime::multi_block::{ self as runtime, runtime_types::pallet_election_provider_multi_block::types::Phase },
	static_types::multi_block as static_types,
	utils,
};
// no PerThing import needed
// use polkadot_sdk::pallet_election_provider_multi_block::types::{
//     SolutionOf,
// };
// use crate::predict::ActiveValidator;
// use crate::predict::{PredictionResults, ElectionStatistics};
// use crate::runtime::multi_block::runtime_types::pallet_election_provider_multi_block::types::{
//     PagedRawSolution,
// };
// use polkadot_sdk::pallet_election_provider_multi_block::unsigned::miner::MinerConfig;
use polkadot_sdk::pallet_election_provider_multi_block::{
	types::{ AssignmentOf, PagedRawSolution, SolutionOf },
	unsigned::miner::MinerConfig,
};

use codec::Encode;
use futures::TryStreamExt;
use polkadot_sdk::frame_support::BoundedVec;
use polkadot_sdk::frame_support::traits::Get; // For reading associated `Get` types like MaxVotesPerVoter
use polkadot_sdk::sp_runtime::AccountId32 as RuntimeAccountId32;
use polkadot_sdk::frame_election_provider_support::NposSolution;
use serde::{ Deserialize, Serialize };
// no direct import of sp_arithmetic; we will approximate weights without PerThing
use std::collections::{ HashMap, HashSet };
use subxt::utils::AccountId32 as SubxtAccountId;

/// Input format for custom nominators and validators
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomSetup {
	pub nominators: Vec<CustomNominator>,
	pub nominators_remove: Vec<String>,
	pub validators: Vec<CustomValidator>,
	pub validators_remove: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomNominator {
	pub account: String,
	pub stake: u128,
	pub targets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomValidator {
	pub account: String,
	pub stake: u128,
	pub is_dummy: bool,
}

/// Output format for prediction results
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PredictionResult {
	pub metadata: PredictionMetadata,
	pub results: PredictionResults,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PredictionMetadata {
	pub timestamp: String,
	pub block_number: u32,
	pub chain: String,
	pub desired_validators: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PredictionResults {
	pub active_validators: Vec<ActiveValidator>,
	pub statistics: ElectionStatistics,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveValidator {
	pub account: String,
	pub total_stake: u128,
	pub self_stake: u128,
	pub nominators_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElectionStatistics {
	pub minimum_stake: u128,
	pub average_stake: u128,
	pub total_staked: u128,
}

/// Main predict command implementation
pub async fn predict_cmd<T>(client: Client, config: PredictConfig) -> Result<(), Error>
	where
		T: MinerConfig<AccountId = AccountId> + Send + Sync + 'static,
		T::Solution: Send + Sync + 'static,
		T::Pages: Send + Sync + 'static,
		T::TargetSnapshotPerBlock: Send + Sync + 'static,
		T::VoterSnapshotPerBlock: Send + Sync + 'static,
		T::MaxVotesPerVoter: Send + Sync + 'static
{
	log::info!(target: LOG_TARGET, "Starting election prediction");

	// Get current chain state
	let storage = utils::storage_at_head(&client).await?;
	let current_block = client.chain_api().blocks().at_latest().await?;
	let block_number = current_block.header().number;
	let chain_name = get_chain_name(&client).await?;

	// Get current phase and round
	let current_phase = storage.fetch_or_default(&runtime::storage().multi_block_election().current_phase()).await?;
	let current_round = storage.fetch_or_default(&runtime::storage().multi_block_election().round()).await?;

	log::info!(
		target: LOG_TARGET,
		"Current state: block #{}, round {}, phase {:?}",
		block_number,
		current_round,
		current_phase
	);

	// Determine desired validators
	let desired_validators = match config.desired_validators {
		Some(count) => count,
		None => {
			// Try to get from current round, fallback to a reasonable default
			storage
				.fetch(&runtime::storage().multi_block_election().desired_targets(current_round)).await?
				.unwrap_or(19) // Polkadot default
		}
	};

	log::info!(target: LOG_TARGET, "Desired validators: {}", desired_validators);

	// Handle custom setup if provided
	let prediction_result = if let Some(custom_file) = config.custom_nominators_validators {
		log::info!(target: LOG_TARGET, "Using custom setup from file: {}", custom_file);
		predict_with_custom_setup::<T>(client, custom_file, desired_validators, block_number, chain_name).await?
	} else {
		log::info!(target: LOG_TARGET, "Using current chain state for prediction");
		predict_with_current_state::<T>(
			client,
			storage,
			current_phase,
			current_round,
			desired_validators,
			block_number,
			chain_name
		).await?
	};

	// Write results to file
	let json_output = serde_json
		::to_string_pretty(&prediction_result)
		.map_err(|e| Error::Other(format!("Failed to serialize prediction result: {e}")))?;

	std::fs
		::write(&config.output, json_output)
		.map_err(|e| Error::Other(format!("Failed to write output file {}: {e}", config.output)))?;

	log::info!(target: LOG_TARGET, "Prediction completed and saved to: {}", config.output);
	Ok(())
}

/// Get chain name from client
async fn get_chain_name(_client: &Client) -> Result<String, Error> {
	// For now, return a generic name since accessing RPC properties
	// requires additional setup. In a real implementation, you'd want
	// to fetch this from the chain's system properties.
	Ok("Westend Asset Hub".to_string())
}

/// Predict using current chain state
async fn predict_with_current_state<T>(
	client: Client,
	storage: Storage,
	current_phase: Phase,
	current_round: u32,
	desired_validators: u32,
	block_number: u32,
	chain_name: String
)
	-> Result<PredictionResult, Error>
	where
		T: MinerConfig<AccountId = AccountId> + Send + Sync + 'static,
		T::Solution: Send + Sync + 'static,
		T::Pages: Send + Sync + 'static,
		T::TargetSnapshotPerBlock: Send + Sync + 'static,
		T::VoterSnapshotPerBlock: Send + Sync + 'static,
		T::MaxVotesPerVoter: Send + Sync + 'static
{
	// Check if we have snapshot data available
	let has_snapshot_data = matches!(
		current_phase,
		Phase::Signed(_) | Phase::Snapshot(_) | Phase::Done | Phase::Export(_) | Phase::Unsigned(_)
	);

	if !has_snapshot_data {
		log::warn!(
			target: LOG_TARGET,
			"No snapshot data available in phase {:?}. Using live data.",
			current_phase
		);
		log::info!(
			target: LOG_TARGET,
			"Note: Snapshot data is only available during Signed/Snapshot/Done/Export phases. \
			In Off phase, we fall back to live chain data from the staking pallet."
		);
		return predict_with_live_data::<T>(client, desired_validators, block_number, chain_name).await;
	}

	log::info!(target: LOG_TARGET, "Using snapshot data for prediction");

	// Fetch snapshots from current state
	let mut snapshot = crate::commands::multi_block::types::Snapshot::<T>::new(static_types::Pages::get());
	crate::dynamic::multi_block::fetch_missing_snapshots::<T>(&mut snapshot, &storage, current_round).await?;
	let (target_snapshot, voter_snapshot) = snapshot.get();

	log::info!(
		target: LOG_TARGET,
		"Fetched snapshot: {} targets, {} voter pages",
		target_snapshot.len(),
		voter_snapshot.len()
	);

	// Mine solution using current snapshot data
	let n_pages = static_types::Pages::get();
	let target_snapshot_for_mining = target_snapshot.clone();
	let voter_snapshot_for_mining = voter_snapshot.clone();
	let paged_raw_solution = crate::dynamic::multi_block::mine_solution::<T>(
		target_snapshot_for_mining,
		voter_snapshot_for_mining,
		n_pages,
		current_round,
		desired_validators,
		block_number,
		false // Don't reduce for prediction
	).await?;

	log::info!(target: LOG_TARGET, "Mined solution with score: {:?}", paged_raw_solution.score);

	// Build self-stake map for validators from snapshot (same block context)
	let mut validator_self_stake: HashMap<AccountId, u128> = HashMap::new();
	for who in target_snapshot.iter() {
		// Convert runtime AccountId32 -> Subxt AccountId32 for storage fetch
		let subxt_acc = SubxtAccountId(*who.as_ref());
		let self_stake = storage
			.fetch(&runtime::storage().staking().ledger(subxt_acc)).await?
			.map(|l| l.total)
			.unwrap_or(0);
		validator_self_stake.insert(who.clone(), self_stake);
	}

	// Process results using the snapshots and self stake
	let results = process_solution_results::<T>(
		&paged_raw_solution,
		desired_validators,
		&target_snapshot,
		&voter_snapshot,
		Some(&validator_self_stake)
	)?;

	Ok(PredictionResult {
		metadata: PredictionMetadata {
			timestamp: chrono::Utc::now().to_rfc3339(),
			block_number,
			chain: chain_name,
			desired_validators,
		},
		results,
	})
}
/// Convert by value
fn subxt_to_runtime_account(acc: SubxtAccountId) -> RuntimeAccountId32 {
	RuntimeAccountId32::new(acc.0)
}

use futures::{ stream::FuturesUnordered, StreamExt };

const BATCH_SIZE: usize = 1000;
const CONCURRENCY_LIMIT: usize = 100;

// Alternative: Stream-based approach (even faster for large datasets)
async fn fetch_nominators_streaming<T>(
	storage: &Storage
) -> Result<Vec<(RuntimeAccountId32, u128, Vec<RuntimeAccountId32>)>, Error> {
	log::info!("Starting streaming nominator fetch...");

	let semaphore = Arc::new(Semaphore::new(CONCURRENCY_LIMIT));
	let mut all_voters = Vec::new();

	let mut iter = storage.iter(runtime::storage().staking().nominators_iter()).await?;
	let mut pending = FuturesUnordered::new();

	loop {
		// Fill pipeline
		while pending.len() < CONCURRENCY_LIMIT {
			match iter.try_next().await? {
				Some(entry) => {
					if let Some(acc) = subxt_account_from_key_bytes(&entry.key_bytes) {
						let storage = storage.clone();
						let semaphore = semaphore.clone();
						log::info!("Storage storage");

						pending.push(async move {
							let _permit = semaphore.acquire().await.unwrap();

							// Create query builders first
							let nom_query = runtime::storage().staking().nominators(acc.clone());
							let ledger_query = runtime::storage().staking().ledger(acc.clone());

							let (noms_result, ledger_result) = tokio::join!(
								storage.fetch(&nom_query),
								storage.fetch(&ledger_query)
							);

							match (noms_result, ledger_result) {
								(Ok(Some(noms)), Ok(Some(ledger))) if ledger.total > 0 => {
									let targets = noms.targets.0
										.into_iter()
										.map(subxt_to_runtime_account)
										.collect::<Vec<_>>();
									let runtime_acc = subxt_to_runtime_account(acc);
									Some((runtime_acc, ledger.total as u128, targets))
								}
								_ => None,
							}
						});
					}
				}
				None => {
					break;
				}
			}
		}

		// Process one result
		if let Some(result) = pending.next().await {
			if let Some(voter) = result {
				all_voters.push(voter);

				if all_voters.len() % 1000 == 0 {
					log::info!("Fetched {} voters...", all_voters.len());
				}
			}
		} else {
			break;
		}
	}
	log::info!("Hi bhai bhai bhai");
	// Drain remaining
	while let Some(result) = pending.next().await {
		if let Some(voter) = result {
			all_voters.push(voter);
		}
	}

	log::info!("Fetch complete: {} voters", all_voters.len());
	Ok(all_voters)
}

async fn fetch_nominators_in_batches<T>(
	storage: &Storage
) -> Result<Vec<(RuntimeAccountId32, u128, Vec<RuntimeAccountId32>)>, Error> {
	let mut all_voters = Vec::new();
	let mut nominators_keys = Vec::new();

	// Collect all nominator keys
	if let Ok(mut iter) = storage.iter(runtime::storage().staking().nominators_iter()).await {
		log::info!("nominators_keys {:?}", nominators_keys);
		while let Some(entry) = iter.try_next().await? {
			nominators_keys.push(entry.key_bytes);
		}
	}

	log::info!("Total nominators: {}", nominators_keys.len());

	// Process keys in chunks with limited concurrency
	let mut tasks = FuturesUnordered::new();

	for chunk in nominators_keys.chunks(BATCH_SIZE) {
		let batch = chunk.to_vec();
		let storage = storage.clone();

		tasks.push(async move {
			let mut local_voters = Vec::new();

			for key_bytes in batch {
				if let Some(acc) = subxt_account_from_key_bytes(&key_bytes) {
					if let Some(noms) = storage.fetch(&runtime::storage().staking().nominators(acc.clone())).await? {
						let stake = storage
							.fetch(&runtime::storage().staking().ledger(acc.clone())).await?
							.map(|l| l.total)
							.unwrap_or(0);

						if stake > 0 {
							let targets = noms.targets.0.into_iter().map(subxt_to_runtime_account).collect::<Vec<_>>();
							let runtime_acc = subxt_to_runtime_account(acc);
							local_voters.push((runtime_acc, stake, targets));
						}
					}
				}
			}

			Ok::<_, Error>(local_voters)
		});

		// Limit concurrency
		if tasks.len() >= CONCURRENCY_LIMIT {
			if let Some(result) = tasks.next().await {
				all_voters.extend(result?);
			}
		}
	}

	// Drain remaining
	while let Some(result) = tasks.next().await {
		all_voters.extend(result?);
	}

	Ok(all_voters)
}

async fn predict_with_live_data<T>(
	client: Client,
	desired_validators: u32,
	block_number: u32,
	chain_name: String
)
	-> Result<PredictionResult, Error>
	where
		T: MinerConfig<AccountId = RuntimeAccountId32> + Send + Sync + 'static,
		T::Solution: Send + Sync + 'static,
		T::Pages: Send + Sync + 'static,
		T::TargetSnapshotPerBlock: Send + Sync + 'static,
		T::VoterSnapshotPerBlock: Send + Sync + 'static,
		T::MaxVotesPerVoter: Get<u32> + Send + Sync + 'static
{
	log::info!(target: LOG_TARGET, "Simulating snapshot from live staking data");

	let storage = utils::storage_at_head(&client).await?;

	println!("Aagi storage");

	// ---------------------------------
	// Collect validators + self stake
	// ---------------------------------
	let mut validators: Vec<RuntimeAccountId32> = Vec::new();
	let mut validator_self_stake: HashMap<RuntimeAccountId32, u128> = HashMap::new();

	if let Ok(mut iter) = storage.iter(runtime::storage().staking().validators_iter()).await {
		while let Some(entry) = iter.try_next().await? {
			if let Some(acc) = subxt_account_from_key_bytes(&entry.key_bytes) {
				// Keep acc as SubxtAccountId for fetching
				let self_stake = storage
					.fetch(&runtime::storage().staking().ledger(acc.clone())).await?
					.map(|l| l.total)
					.unwrap_or(0);

				let runtime_acc = subxt_to_runtime_account(acc); // convert here
				validators.push(runtime_acc.clone());
				validator_self_stake.insert(runtime_acc, self_stake);
			}
		}
	}
	log::info!("Validators Agge bhai ");

	let voters = fetch_nominators_streaming::<T>(&storage).await?;
	log::info!("Collected {} voters", voters.len());

	log::info!("Snapshot bnaooo");

	// ---------------------------------
	// Build bounded snapshots
	// ---------------------------------
	
	log::info!(target: LOG_TARGET, "Building paginated snapshots...");

	let target_snapshot: TargetSnapshotPageOf<T> = BoundedVec::try_from(validators.clone()).map_err(|_|
		Error::Other(format!("Too many validators ({})", validators.len()))
	)?;

	log::info!(target: LOG_TARGET, "Target snapshot: {} validators", target_snapshot.len());

    // Build voter pages with strict per-page capacity
    let n_pages = static_types::Pages::get();
    let max_voters_per_page = T::VoterSnapshotPerBlock::get() as usize;
    let max_targets_per_voter = T::MaxVotesPerVoter::get() as usize;

    let mut voter_snapshot: Vec<VoterSnapshotPageOf<T>> = Vec::new();
    let mut current_page: Vec<(RuntimeAccountId32, u64, BoundedVec<RuntimeAccountId32, T::MaxVotesPerVoter>)> =
        Vec::with_capacity(max_voters_per_page);
    let mut skipped_voters = 0usize;

    for (voter_acc, stake, targets) in voters {
        // enforce per-voter target limit
        let bounded_targets = if targets.len() <= max_targets_per_voter {
            match BoundedVec::try_from(targets.clone()) {
                Ok(bt) => bt,
                Err(_) => { skipped_voters += 1; continue; }
            }
        } else {
            let truncated: Vec<_> = targets.into_iter().take(max_targets_per_voter).collect();
            match BoundedVec::try_from(truncated) {
                Ok(bt) => bt,
                Err(_) => { skipped_voters += 1; continue; }
            }
        };

        if current_page.len() == max_voters_per_page {
            // finalize current page
            if let Ok(page) = BoundedVec::<_, T::VoterSnapshotPerBlock>::try_from(current_page.clone()) {
                voter_snapshot.push(page);
            }
            current_page.clear();

            // stop if we've reached maximum allowed pages
            if voter_snapshot.len() as u32 >= n_pages { break; }
        }

        current_page.push((voter_acc.clone(), stake as u64, bounded_targets));
    }

    if !current_page.is_empty() && (voter_snapshot.len() as u32) < n_pages {
        if let Ok(page) = BoundedVec::<_, T::VoterSnapshotPerBlock>::try_from(current_page.clone()) {
            voter_snapshot.push(page);
        }
    }

    // pad remaining pages with empty ones
    while (voter_snapshot.len() as u32) < n_pages {
        voter_snapshot.push(BoundedVec::default());
    }

	log::info!(target: LOG_TARGET, "Starting solution mining...");

	// ---------------------------------
	// Mine solution
	// ---------------------------------
	let n_pages = static_types::Pages::get();
	let current_round = storage.fetch_or_default(&runtime::storage().multi_block_election().round()).await?;

	let target_snapshot_for_mining = target_snapshot.clone();
	let voter_snapshot_for_mining = voter_snapshot.clone();
	let paged_raw_solution = crate::dynamic::multi_block::mine_solution::<T>(
		target_snapshot_for_mining,
		voter_snapshot_for_mining,
		n_pages,
		current_round,
		desired_validators,
		block_number,
		false
	).await?;

	log::info!(target: LOG_TARGET, "Mined simulated solution: {:?}", paged_raw_solution.score);

	let results = process_solution_results::<T>(
		&paged_raw_solution,
		desired_validators,
		&target_snapshot,
		&voter_snapshot,
		Some(&validator_self_stake)
	)?;

	Ok(PredictionResult {
		metadata: PredictionMetadata {
			timestamp: chrono::Utc::now().to_rfc3339(),
			block_number,
			chain: chain_name,
			desired_validators,
		},
		results,
	})
}

fn subxt_account_from_key_bytes(key_bytes: &[u8]) -> Option<SubxtAccountId> {
	if key_bytes.len() >= 32 {
		let acc_bytes = &key_bytes[key_bytes.len() - 32..];
		let mut arr = [0u8; 32];
		arr.copy_from_slice(acc_bytes);
		Some(SubxtAccountId(arr))
	} else {
		None
	}
}

/// Predict using custom nominator/validator setup
async fn predict_with_custom_setup<T>(
	_client: Client,
	_custom_file: String,
	desired_validators: u32,
	block_number: u32,
	chain_name: String
)
	-> Result<PredictionResult, Error>
	where
		T: MinerConfig<AccountId = AccountId> + Send + Sync + 'static,
		T::Solution: Send + Sync + 'static,
		T::Pages: Send + Sync + 'static,
		T::TargetSnapshotPerBlock: Send + Sync + 'static,
		T::VoterSnapshotPerBlock: Send + Sync + 'static,
		T::MaxVotesPerVoter: Send + Sync + 'static
{
	// TODO: Implement custom setup prediction
	log::info!(target: LOG_TARGET, "Custom setup prediction not yet implemented");
	Ok(PredictionResult {
		metadata: PredictionMetadata {
			timestamp: chrono::Utc::now().to_rfc3339(),
			block_number,
			chain: chain_name,
			desired_validators,
		},
		results: PredictionResults {
			active_validators: vec![],
			statistics: ElectionStatistics { minimum_stake: 0, average_stake: 0, total_staked: 0 },
		},
	})
}

pub fn all_assignments<T: MinerConfig<AccountId = AccountId>>(
	paged: &PagedRawSolution<T>,
	target_snapshot: &crate::commands::multi_block::types::TargetSnapshotPageOf<T>,
	voter_snapshot_pages: &Vec<crate::commands::multi_block::types::VoterSnapshotPageOf<T>>
) -> Result<Vec<AssignmentOf<T>>, Error> {
	let mut assignments = Vec::new();

	for (page_idx, page) in paged.solution_pages.iter().enumerate() {
		// Map solution indices to AccountIds using the provided snapshots
		let voter_page_opt = voter_snapshot_pages.get(page_idx);
		if voter_page_opt.is_none() {
			continue;
		}
		let voter_page = voter_page_opt.unwrap();

		let voter_at = |voter_index: <SolutionOf<T> as NposSolution>::VoterIndex| -> Option<T::AccountId> {
			use core::convert::TryInto;
			let idx: usize = match voter_index.try_into().ok() {
				Some(i) => i,
				None => {
					return None;
				}
			};
			voter_page.get(idx).map(|(who, _stake, _targets)| {
				let a: T::AccountId = who.clone();
				a
			})
		};

		let target_at = |target_index: <SolutionOf<T> as NposSolution>::TargetIndex| -> Option<T::AccountId> {
			use core::convert::TryInto;
			let idx: usize = match target_index.try_into().ok() {
				Some(i) => i,
				None => {
					return None;
				}
			};
			target_snapshot
				.get(idx)
				.cloned()
				.map(|who| {
					let a: T::AccountId = who;
					a
				})
		};

		let page_assignments = page
			.clone()
			.into_assignment(voter_at, target_at)
			.map_err(|e| Error::Other(format!("Assignment error: {:?}", e)))?;

		assignments.extend(page_assignments);
	}

	Ok(assignments)
}

pub fn assignment_info<T: MinerConfig<AccountId = AccountId>>(assignment: &AssignmentOf<T>) -> (String, usize) {
	let account_hex = format!("0x{}", hex::encode(assignment.who.encode()));
	let targets_count = assignment.distribution.len();
	(account_hex, targets_count)
}

/// Process a paged raw solution into PredictionResults
pub fn process_solution_results<T: MinerConfig<AccountId = AccountId>>(
	paged_raw_solution: &PagedRawSolution<T>,
	desired_validators: u32,
	target_snapshot: &crate::commands::multi_block::types::TargetSnapshotPageOf<T>,
	voter_snapshot_pages: &Vec<crate::commands::multi_block::types::VoterSnapshotPageOf<T>>,
	validator_self_stake: Option<&HashMap<AccountId, u128>>
) -> Result<PredictionResults, Error> {
	// Build voter -> stake map from snapshot pages
	let mut voter_stake: HashMap<AccountId, u128> = HashMap::new();
	for page in voter_snapshot_pages.iter() {
		for (who, stake, _targets) in page.iter() {
			voter_stake.insert(who.clone(), *stake as u128);
		}
	}

	// Convert solution pages -> voter assignments
	let assignments = all_assignments(paged_raw_solution, target_snapshot, voter_snapshot_pages)?;

	// Aggregate per validator totals using assignment weights (fallback to equal split if PerThing not available)
	let mut validator_total: HashMap<AccountId, u128> = HashMap::new();
	let mut validator_nominators: HashMap<AccountId, HashSet<AccountId>> = HashMap::new();

	for voter_assignment in assignments {
		let voter = voter_assignment.who.clone();
		let Some(stake) = voter_stake.get(&voter).copied() else {
			continue;
		};

		// Equal-split approximation across all edges for this voter
		let edges = voter_assignment.distribution.len() as u128;
		if edges == 0 {
			continue;
		}
		let part = stake / edges;
		for (validator, _weight) in voter_assignment.distribution.iter() {
			*validator_total.entry(validator.clone()).or_insert(0) += part;
			validator_nominators.entry(validator.clone()).or_insert_with(HashSet::new).insert(voter.clone());
		}
	}

	// Sort validators by total stake desc and take top desired_validators
	let mut validators: Vec<(AccountId, u128, u32)> = validator_total
		.into_iter()
		.map(|(who, total)| {
			let nominators_count = validator_nominators
				.get(&who)
				.map(|s| s.len() as u32)
				.unwrap_or(0);
			(who, total, nominators_count)
		})
		.collect();
	validators.sort_by(|a, b| b.1.cmp(&a.1));
	validators.truncate(desired_validators as usize);

	// Build output
	let mut active_validators = Vec::with_capacity(validators.len());
	let mut total_staked: u128 = 0;
	let mut stake_values: Vec<u128> = Vec::with_capacity(validators.len());

	for (who, total, nominators_count) in validators {
		let account_hex = format!("0x{}", hex::encode(who.encode()));
		let self_stake_val = validator_self_stake.and_then(|m| m.get(&who).copied()).unwrap_or(0);
		active_validators.push(ActiveValidator {
			account: account_hex,
			total_stake: total,
			self_stake: self_stake_val,
			nominators_count,
		});
		total_staked += total;
		stake_values.push(total);
	}

	let minimum_stake = stake_values.iter().copied().min().unwrap_or(0);
	let average_stake = if !stake_values.is_empty() { total_staked / (stake_values.len() as u128) } else { 0 };

	Ok(PredictionResults {
		active_validators,
		statistics: ElectionStatistics { minimum_stake, average_stake, total_staked },
	})
}
