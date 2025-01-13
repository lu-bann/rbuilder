use crate::live_builder::simulation::SimulatedOrderCommand;
use crate::provider::RootHasher;
use crate::roothash::RootHashError;
use crate::{
    building::{
        BlockBuildingContext, BuiltBlockTrace, CriticalCommitOrderError, ExecutionError,
        ExecutionResult,
    },
    primitives::{SimulatedOrder, TransactionSignedEcRecoveredWithBlobs},
};
use alloy_primitives::B256;
use alloy_primitives::U256;
use reth::providers::ExecutionOutcome;
use reth::revm::cached::CachedReads;
use reth_primitives::SealedBlock;
use time::OffsetDateTime;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use super::{
    block_building_helper::{BlockBuildingHelper, BlockBuildingHelperError, FinalizeBlockResult},
    Block,
};

/// Extremely dumb object for test. Adding orders (commit_order) is not allowed.
/// Is has a predefined true_block_value and the only useful thing that generates on finalize_block is the bid value.
#[derive(Clone, Debug)]
pub struct MockBlockBuildingHelper {
    built_block_trace: BuiltBlockTrace,
    block_building_context: BlockBuildingContext,
    can_add_payout_tx: bool,
}

impl MockBlockBuildingHelper {
    pub fn new(true_block_value: U256, can_add_payout_tx: bool) -> Self {
        let built_block_trace = BuiltBlockTrace {
            true_bid_value: true_block_value,
            ..Default::default()
        };
        Self {
            built_block_trace,
            block_building_context: BlockBuildingContext::dummy_for_testing(),
            can_add_payout_tx,
        }
    }
}

impl BlockBuildingHelper for MockBlockBuildingHelper {
    fn box_clone(&self) -> Box<dyn BlockBuildingHelper> {
        Box::new(self.clone())
    }

    fn commit_order(
        &mut self,
        _order: &SimulatedOrder,
    ) -> Result<Result<&ExecutionResult, ExecutionError>, CriticalCommitOrderError> {
        unimplemented!()
    }

    fn set_constraints(&mut self, _constraints: Vec<TransactionSignedEcRecoveredWithBlobs>) {
        unimplemented!()
    }

    fn commit_constraint(
        &mut self,
        _constraint: &TransactionSignedEcRecoveredWithBlobs,
    ) -> Result<Result<ExecutionResult, ExecutionError>, CriticalCommitOrderError> {
        unimplemented!()
    }

    fn set_trace_fill_time(&mut self, time: std::time::Duration) {
        self.built_block_trace.fill_time = time;
    }

    fn set_trace_orders_closed_at(&mut self, orders_closed_at: OffsetDateTime) {
        self.built_block_trace.orders_closed_at = orders_closed_at;
    }

    fn can_add_payout_tx(&self) -> bool {
        self.can_add_payout_tx
    }

    fn true_block_value(&self) -> Result<U256, BlockBuildingHelperError> {
        Ok(self.built_block_trace.true_bid_value)
    }

    fn finalize_block(
        mut self: Box<Self>,
        payout_tx_value: Option<U256>,
    ) -> Result<FinalizeBlockResult, BlockBuildingHelperError> {
        self.built_block_trace.update_orders_sealed_at();
        self.built_block_trace.bid_value = if let Some(payout_tx_value) = payout_tx_value {
            payout_tx_value
        } else {
            self.built_block_trace.true_bid_value
        };
        let block = Block {
            trace: self.built_block_trace,
            sealed_block: SealedBlock::default(),
            txs_blobs_sidecars: Vec::new(),
            builder_name: "BlockBuildingHelper".to_string(),
            execution_requests: Default::default(),
        };

        Ok(FinalizeBlockResult {
            block,
            cached_reads: CachedReads::default(),
        })
    }

    fn clone_cached_reads(&self) -> CachedReads {
        CachedReads::default()
    }

    fn built_block_trace(&self) -> &BuiltBlockTrace {
        &self.built_block_trace
    }

    fn building_context(&self) -> &BlockBuildingContext {
        &self.block_building_context
    }

    fn update_cached_reads(&mut self, _cached_reads: CachedReads) {
        unimplemented!()
    }

    fn builder_name(&self) -> &str {
        "Mock"
    }
}

#[derive(Debug)]
pub struct MockRootHasher {}

impl RootHasher for MockRootHasher {
    fn run_prefetcher(
        &self,
        _simulated_orders: broadcast::Receiver<SimulatedOrderCommand>,
        _cancel: CancellationToken,
    ) {
    }

    fn state_root(&self, _outcome: &ExecutionOutcome) -> Result<B256, RootHashError> {
        Ok(B256::default())
    }
}
