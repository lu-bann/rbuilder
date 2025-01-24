pub mod base_config;
pub mod block_output;
pub mod building;
pub mod cli;
pub mod config;
pub mod constraint_client;
pub mod order_input;
pub mod payload_events;
pub mod simulation;
pub mod watchdog;

use crate::{
    building::{
        builders::{BlockBuildingAlgorithm, UnfinishedBlockBuildingSinkFactory},
        BlockBuildingContext,
    },
    live_builder::{
        order_input::{start_orderpool_jobs, OrderInputConfig},
        simulation::OrderSimulationPool,
        watchdog::spawn_watchdog_thread,
    },
    primitives::constraints::SignedConstraints,
    provider::StateProviderFactory,
    telemetry::inc_active_slots,
    utils::{
        error_storage::spawn_error_storage_writer, provider_head_state::ProviderHeadState, Signer,
    },
};
use ahash::{HashMap, HashSet};
use alloy_consensus::Header;
use alloy_primitives::{Address, B256};
use building::BlockBuildingPool;
use constraint_client::ConstraintSubscriber;
use ethereum_consensus::configs::mainnet::SECONDS_PER_SLOT;
use eyre::Context;
use jsonrpsee::RpcModule;
use order_input::ReplaceableOrderPoolCommand;
use parking_lot::RwLock;
use payload_events::MevBoostSlotData;
use reth_chainspec::ChainSpec;
use std::{cmp::min, fmt::Debug, path::PathBuf, sync::Arc, time::Duration};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

#[derive(Debug, Clone)]
pub struct TimingsConfig {
    /// Time the proposer have to propose a block from the beginning of the
    /// slot (https://www.paradigm.xyz/2023/04/mev-boost-ethereum-consensus Slot anatomy)
    pub slot_proposal_duration: Duration,
    /// Delta from slot time to get_header dead line. If we can't get the block header
    /// before slot_time + BLOCK_HEADER_DEAD_LINE_DELTA we cancel the slot.
    /// Careful: It's signed and usually negative since we need de header BEFORE the slot time.
    pub block_header_deadline_delta: time::Duration,
    /// Polling period while trying to get a block header
    pub get_block_header_period: time::Duration,
    /// Time to wait for constraints to be received before building a block
    pub receive_constraints_cuttoff_duration: Option<Duration>,
}

impl TimingsConfig {
    /// Classic rbuilder
    pub fn ethereum() -> Self {
        Self {
            slot_proposal_duration: Duration::from_secs(4),
            block_header_deadline_delta: time::Duration::milliseconds(-2500),
            get_block_header_period: time::Duration::milliseconds(250),
            receive_constraints_cuttoff_duration: Some(Duration::from_secs(8)),
        }
    }

    /// Configuration for OP-based chains with fast block times
    pub fn optimism() -> Self {
        Self {
            slot_proposal_duration: Duration::from_secs(0),
            block_header_deadline_delta: time::Duration::milliseconds(-25),
            get_block_header_period: time::Duration::milliseconds(25),
            receive_constraints_cuttoff_duration: None,
        }
    }
}

/// Trait used to trigger a new block building process in the slot.
pub trait SlotSource {
    fn recv_slot_channel(self) -> mpsc::UnboundedReceiver<MevBoostSlotData>;
}

/// Max headers sent to the cleaning task before the main loop blocks.
/// Cleaning task is super fast so it should never lag behind block building, even 1 should be enough, 10 is super safe.
const CLEAN_TASKS_CHANNEL_SIZE: usize = 10;

/// Main builder struct.
/// Connects to the CL, get the new slots and builds blocks for each slot.
/// # Usage
/// Create and run()
#[derive(Debug)]
pub struct LiveBuilder<P, BlocksSourceType>
where
    P: StateProviderFactory,
    BlocksSourceType: SlotSource,
{
    pub watchdog_timeout: Option<Duration>,
    pub error_storage_path: Option<PathBuf>,
    pub simulation_threads: usize,
    pub order_input_config: OrderInputConfig,
    pub blocks_source: BlocksSourceType,
    pub run_sparse_trie_prefetcher: bool,

    pub chain_chain_spec: Arc<ChainSpec>,
    pub provider: P,

    pub coinbase_signer: Signer,
    pub extra_data: Vec<u8>,
    pub blocklist: HashSet<Address>,

    pub global_cancellation: CancellationToken,

    pub sink_factory: Box<dyn UnfinishedBlockBuildingSinkFactory>,
    pub builders: Vec<Arc<dyn BlockBuildingAlgorithm<P>>>,
    pub extra_rpc: RpcModule<()>,

    /// Notify rbuilder of new [`ReplaceableOrderPoolCommand`] flow via this channel.
    pub orderpool_sender: mpsc::Sender<ReplaceableOrderPoolCommand>,
    pub orderpool_receiver: mpsc::Receiver<ReplaceableOrderPoolCommand>,
    pub sbundle_merger_selected_signers: Arc<Vec<Address>>,

    /// constraint stream subsciber
    pub constraint_subscriber: Option<ConstraintSubscriber>,
    /// Used to store constraints
    pub constraint_store: Arc<RwLock<HashMap<u64, Vec<SignedConstraints>>>>,
}

impl<P, BlocksSourceType: SlotSource> LiveBuilder<P, BlocksSourceType>
where
    P: StateProviderFactory + Clone + 'static,
    BlocksSourceType: SlotSource,
{
    pub fn with_extra_rpc(self, extra_rpc: RpcModule<()>) -> Self {
        Self { extra_rpc, ..self }
    }

    pub fn with_builders(self, builders: Vec<Arc<dyn BlockBuildingAlgorithm<P>>>) -> Self {
        Self { builders, ..self }
    }

    pub fn with_constraint_subscriber(self, subscriber: ConstraintSubscriber) -> Self {
        Self {
            constraint_subscriber: Some(subscriber),
            ..self
        }
    }

    pub async fn run(self) -> eyre::Result<()> {
        info!("Builder block list size: {}", self.blocklist.len(),);
        info!(
            "Builder coinbase address: {:?}",
            self.coinbase_signer.address
        );
        let timings = self.timings();

        if let Some(error_storage_path) = self.error_storage_path {
            spawn_error_storage_writer(error_storage_path, self.global_cancellation.clone())
                .await
                .with_context(|| "Error spawning error storage writer")?;
        }

        let mut inner_jobs_handles = Vec::new();
        let mut payload_events_channel = self.blocks_source.recv_slot_channel();

        let (header_sender, header_receiver) = mpsc::channel(CLEAN_TASKS_CHANNEL_SIZE);

        let orderpool_subscriber = {
            let (handle, sub) = start_orderpool_jobs(
                self.order_input_config,
                self.provider.clone(),
                self.extra_rpc,
                self.global_cancellation.clone(),
                self.orderpool_sender,
                self.orderpool_receiver,
                header_receiver,
            )
            .await?;
            inner_jobs_handles.push(handle);
            sub
        };

        let order_simulation_pool = {
            OrderSimulationPool::new(
                self.provider.clone(),
                self.simulation_threads,
                self.global_cancellation.clone(),
            )
        };

        let mut builder_pool = BlockBuildingPool::new(
            self.provider.clone(),
            self.builders,
            self.sink_factory,
            orderpool_subscriber,
            order_simulation_pool,
            self.run_sparse_trie_prefetcher,
            self.sbundle_merger_selected_signers.clone(),
        );

        let watchdog_sender = match self.watchdog_timeout {
            Some(duration) => Some(spawn_watchdog_thread(
                duration,
                "block build started".to_string(),
            )?),
            None => {
                info!("Watchdog not enabled");
                None
            }
        };

        let mut constraint_stream_channel = self.constraint_subscriber.unwrap().spawn();
        tokio::spawn({
            let constraint_store_clone = self.constraint_store.clone();
            async move {
                while let Some(constraint) = constraint_stream_channel.recv().await {
                    constraint_store_clone
                        .write()
                        .entry(constraint.message.slot)
                        .or_default()
                        .push(constraint.clone());
                    info!("Wrote constraint to constraint-store: {:?}", constraint);
                }
            }
        });

        while let Some(payload) = payload_events_channel.recv().await {
            if self.blocklist.contains(&payload.fee_recipient()) {
                warn!(
                    slot = payload.slot(),
                    "Fee recipient is in blocklist: {:?}",
                    payload.fee_recipient()
                );
                continue;
            }
            let current_time = OffsetDateTime::now_utc();
            // see if we can get parent header in a reasonable time
            let time_to_slot = payload.timestamp() - current_time;
            debug!(
                slot = payload.slot(),
                block = payload.block(),
                ?current_time,
                payload_timestamp = ?payload.timestamp(),
                ?time_to_slot,
                parent_hash = ?payload.parent_block_hash(),
                provider_head_state = ?ProviderHeadState::new(&self.provider),
                "Received payload, time till slot timestamp",
            );

            // If we have a constraints cuttoff time, we should wait until it passes before
            match timings.receive_constraints_cuttoff_duration {
                Some(cuttoff_duration) => {
                    let time_until_constraints_cuttoff =
                        time_to_slot + cuttoff_duration - Duration::from_secs(SECONDS_PER_SLOT);
                    if time_until_constraints_cuttoff.is_negative() {
                        debug!(
                            slot = payload.slot(),
                            "Constraints cuttoff time hasn't passed, sleeping for {}",
                            time_until_constraints_cuttoff.as_seconds_f64()
                        );
                        tokio::time::sleep(Duration::from_secs_f64(
                            time_until_constraints_cuttoff.as_seconds_f64(),
                        ))
                        .await;
                    };
                }
                None => debug!("No constraints cuttoff time, proceeding with block building"),
            };

            let time_until_slot_end = time_to_slot + timings.slot_proposal_duration;
            if time_until_slot_end.is_negative() {
                warn!(
                    slot = payload.slot(),
                    parent_hash = ?payload.parent_block_hash(),
                    "Slot already ended, skipping block building"
                );
                continue;
            };

            let parent_header = {
                // @Nicer
                let parent_block = payload.parent_block_hash();
                let timestamp = payload.timestamp();
                match wait_for_block_header(parent_block, timestamp, &self.provider, &timings).await
                {
                    Ok(header) => header,
                    Err(err) => {
                        warn!(parent_hash = ?payload.parent_block_hash(),"Failed to get parent header for new slot: {:?}", err);
                        continue;
                    }
                }
            };

            debug!(
                slot = payload.slot(),
                block = payload.block(),
                parent_hash = ?payload.parent_block_hash(),
                "Got header for slot"
            );

            // notify the order pool that there is a new header
            if let Err(err) = header_sender.send(parent_header.clone()).await {
                warn!("Failed to send header to builder pool: {:?}", err);
            }

            inc_active_slots();

            let root_hasher = Arc::from(self.provider.root_hasher(payload.parent_block_hash()));

            if let Some(block_ctx) = BlockBuildingContext::from_attributes(
                payload.payload_attributes_event.clone(),
                &parent_header,
                self.coinbase_signer.clone(),
                self.chain_chain_spec.clone(),
                self.blocklist.clone(),
                Some(payload.suggested_gas_limit),
                self.extra_data.clone(),
                None,
                root_hasher,
            ) {
                builder_pool.start_block_building(
                    payload.clone(),
                    block_ctx,
                    self.global_cancellation.clone(),
                    time_until_slot_end.try_into().unwrap_or_default(),
                    self.constraint_store.read().get(&payload.slot()).cloned(),
                );

                if let Some(watchdog_sender) = watchdog_sender.as_ref() {
                    watchdog_sender.try_send(()).unwrap_or_default();
                };
            }
        }

        info!("Builder shutting down");
        self.global_cancellation.cancel();
        for handle in inner_jobs_handles {
            handle
                .await
                .map_err(|err| warn!("Job handle await error: {:?}", err))
                .unwrap_or_default();
        }
        Ok(())
    }

    // Currently we only need two timings config, depending on whether rbuilder is being
    // used in the optimism context. If further customisation is required in the future
    // this should be improved on.
    fn timings(&self) -> TimingsConfig {
        if cfg!(feature = "optimism") {
            TimingsConfig::optimism()
        } else {
            TimingsConfig::ethereum()
        }
    }
}

/// May fail if we wait too much (see [BLOCK_HEADER_DEAD_LINE_DELTA])
async fn wait_for_block_header<P>(
    block: B256,
    slot_time: OffsetDateTime,
    provider: &P,
    timings: &TimingsConfig,
) -> eyre::Result<Header>
where
    P: StateProviderFactory,
{
    let deadline = slot_time + timings.block_header_deadline_delta;
    while OffsetDateTime::now_utc() < deadline {
        if let Some(header) = provider.header(&block)? {
            return Ok(header);
        } else {
            let time_to_sleep = min(
                deadline - OffsetDateTime::now_utc(),
                timings.get_block_header_period,
            );
            if time_to_sleep.is_negative() {
                break;
            }
            tokio::time::sleep(time_to_sleep.try_into().unwrap()).await;
        }
    }
    Err(eyre::eyre!("Block header not found"))
}
