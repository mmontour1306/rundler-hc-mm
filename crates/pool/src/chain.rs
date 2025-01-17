// This file is part of Rundler.
//
// Rundler is free software: you can redistribute it and/or modify it under the
// terms of the GNU Lesser General Public License as published by the Free Software
// Foundation, either version 3 of the License, or (at your option) any later version.
//
// Rundler is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY;
// without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
// See the GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License along with Rundler.
// If not, see https://www.gnu.org/licenses/.

use std::{
    collections::{HashSet, VecDeque},
    sync::Arc,
    time::Duration,
};

use anyhow::{ensure, Context};
use ethers::{
    contract,
    prelude::EthEvent,
    types::{Address, Block, Filter, Log, H256, U256},
};
use futures::future;
use rundler_provider::Provider;
use rundler_task::block_watcher;
use rundler_types::{
    contracts::{entry_point::DepositedFilter, i_entry_point::UserOperationEventFilter},
    Timestamp, UserOperationId,
};
use tokio::{
    select,
    sync::{broadcast, Semaphore},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

const MAX_LOAD_OPS_CONCURRENCY: usize = 64;

/// A data structure that holds the currently known recent state of the chain,
/// with logic for updating itself and returning what has changed.
///
/// Will update itself when `.sync_to_block_number` is called, at which point it
/// will query a node to determine the new state of the chain.
#[derive(Debug)]
pub(crate) struct Chain<P: Provider> {
    provider: Arc<P>,
    settings: Settings,
    /// Blocks are stored from earliest to latest, so the oldest block is at the
    /// front of this deque and the newest at the back.
    blocks: VecDeque<BlockSummary>,
    /// Semaphore to limit the number of concurrent `eth_getLogs` calls.
    load_ops_semaphore: Semaphore,
}

#[derive(Default, Debug, Eq, PartialEq)]
pub struct ChainUpdate {
    pub latest_block_number: u64,
    pub latest_block_hash: H256,
    pub latest_block_timestamp: Timestamp,
    /// Blocks before this number are no longer tracked in this `Chain`, so no
    /// further updates related to them will be sent.
    pub earliest_remembered_block_number: u64,
    pub reorg_depth: u64,
    pub mined_ops: Vec<MinedOp>,
    pub unmined_ops: Vec<MinedOp>,
    /// List of on-chain entity deposits made in the most recent block
    pub entity_deposits: Vec<DepositInfo>,
    /// List of entity deposits that have been unmined due to a reorg
    pub unmined_entity_deposits: Vec<DepositInfo>,
    /// Boolean to state if the most recent chain update had a reorg
    /// that was larger than the existing history that has been tracked
    pub reorg_larger_than_history: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MinedOp {
    pub hash: H256,
    pub entry_point: Address,
    pub sender: Address,
    pub nonce: U256,
    pub actual_gas_cost: U256,
    pub paymaster: Option<Address>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DepositInfo {
    pub address: Address,
    pub entrypoint: Address,
    pub amount: U256,
}

impl MinedOp {
    pub fn id(&self) -> UserOperationId {
        UserOperationId {
            sender: self.sender,
            nonce: self.nonce,
        }
    }
}

#[derive(Debug)]
pub(crate) struct Settings {
    pub(crate) history_size: u64,
    pub(crate) poll_interval: Duration,
    pub(crate) entry_point_addresses: Vec<Address>,
}

#[derive(Debug)]
struct BlockSummary {
    number: u64,
    hash: H256,
    timestamp: Timestamp,
    parent_hash: H256,
    ops: Vec<MinedOp>,
    entity_deposits: Vec<DepositInfo>,
}

impl<P: Provider> Chain<P> {
    pub(crate) fn new(provider: Arc<P>, settings: Settings) -> Self {
        let history_size = settings.history_size as usize;
        assert!(history_size > 0, "history size should be positive");
        Self {
            provider,
            settings,
            blocks: VecDeque::new(),
            load_ops_semaphore: Semaphore::new(MAX_LOAD_OPS_CONCURRENCY),
        }
    }

    pub(crate) fn spawn_watcher(
        mut self,
        sender: broadcast::Sender<Arc<ChainUpdate>>,
        shutdown_token: CancellationToken,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                select! {
                    update = self.wait_for_update() => {
                        let _ = sender.send(Arc::new(update));
                    }
                    _ = shutdown_token.cancelled() => {
                        info!("Shutting down chain watcher");
                        break;
                    }
                }
            }
        })
    }

    async fn wait_for_update(&mut self) -> ChainUpdate {
        let mut block_hash = self
            .blocks
            .back()
            .map(|block| block.hash)
            .unwrap_or_default();
        loop {
            let (hash, block) = block_watcher::wait_for_new_block(
                &*self.provider,
                block_hash,
                self.settings.poll_interval,
            )
            .await;
            block_hash = hash;
            let update = self.sync_to_block(block).await;
            match update {
                Ok(update) => return update,
                Err(error) => {
                    error!("Failed to update chain at block {block_hash:?}. Will try again at next block. {error:?}");
                }
            }
        }
    }

    pub(crate) async fn sync_to_block(
        &mut self,
        new_head: Block<H256>,
    ) -> anyhow::Result<ChainUpdate> {
        let new_head = BlockSummary::try_from_block_without_ops(new_head, None)?;
        let Some(current_block) = self.blocks.back() else {
            return self.reset_and_initialize(new_head).await;
        };
        let current_block_number = current_block.number;
        let new_block_number = new_head.number;
        ensure!(
            current_block_number < new_block_number + self.settings.history_size,
            "new block number {new_block_number} should be greater than start of history (current block: {current_block_number})"
        );

        if current_block_number + self.settings.history_size < new_block_number {
            warn!(
                "New block {new_block_number} number is {} blocks ahead of the previously known head. Chain history will skip ahead.",
                new_block_number - current_block_number,
            );
            return self.reset_and_initialize(new_head).await;
        }

        let added_blocks = self
            .load_added_blocks_connecting_to_existing_chain(current_block_number, new_head)
            .await?;
        Ok(self.update_with_blocks(current_block_number, added_blocks))
    }

    async fn reset_and_initialize(&mut self, head: BlockSummary) -> anyhow::Result<ChainUpdate> {
        let min_block_number = head.number.saturating_sub(self.settings.history_size - 1);
        let mut blocks = self
            .load_blocks_back_to_number_no_ops(head, min_block_number)
            .await
            .context("should load full history when resetting chain")?;
        self.load_ops_into_block_summaries(&mut blocks).await?;
        self.blocks = blocks;
        let mined_ops: Vec<_> = self
            .blocks
            .iter()
            .flat_map(|block| &block.ops)
            .copied()
            .collect();

        let entity_deposits: Vec<_> = self
            .blocks
            .iter()
            .flat_map(|block| &block.entity_deposits)
            .copied()
            .collect();
        Ok(self.new_update(0, mined_ops, vec![], entity_deposits, vec![], false))
    }

    /// Given a collection of blocks to add to the chain, whose numbers may
    /// overlap the current numbers in the case of reorgs, update the state of
    /// this data structure and return an update struct.
    fn update_with_blocks(
        &mut self,
        current_block_number: u64,
        added_blocks: VecDeque<BlockSummary>,
    ) -> ChainUpdate {
        let mined_ops: Vec<_> = added_blocks
            .iter()
            .flat_map(|block| &block.ops)
            .copied()
            .collect();

        let entity_deposits: Vec<_> = added_blocks
            .iter()
            .flat_map(|block| &block.entity_deposits)
            .copied()
            .collect();
        let reorg_depth = current_block_number + 1 - added_blocks[0].number;
        let unmined_ops: Vec<_> = self
            .blocks
            .iter()
            .skip(self.blocks.len() - reorg_depth as usize)
            .flat_map(|block| &block.ops)
            .copied()
            .collect();

        let unmined_entity_deposits: Vec<_> = self
            .blocks
            .iter()
            .skip(self.blocks.len() - reorg_depth as usize)
            .flat_map(|block| &block.entity_deposits)
            .copied()
            .collect();

        let is_reorg_larger_than_history = reorg_depth >= self.settings.history_size;

        for _ in 0..reorg_depth {
            self.blocks.pop_back();
        }
        self.blocks.extend(added_blocks);
        while self.blocks.len() > self.settings.history_size as usize {
            self.blocks.pop_front();
        }

        ChainMetrics::set_block_height(current_block_number);
        if reorg_depth > 0 {
            ChainMetrics::increment_reorgs_detected();
            ChainMetrics::increment_total_reorg_depth(reorg_depth);
        }

        self.new_update(
            reorg_depth,
            mined_ops,
            unmined_ops,
            entity_deposits,
            unmined_entity_deposits,
            is_reorg_larger_than_history,
        )
    }

    async fn load_added_blocks_connecting_to_existing_chain(
        &self,
        current_block_number: u64,
        new_head: BlockSummary,
    ) -> anyhow::Result<VecDeque<BlockSummary>> {
        // Load blocks from last known number to current.
        let mut added_blocks = self
            .load_blocks_back_to_number_no_ops(new_head, current_block_number + 1)
            .await
            .context("chain should load blocks from last processed to latest block")?;
        ensure!(
            !added_blocks.is_empty(),
            "added blocks should never be empty"
        );
        // Continue to load blocks backwards until we connect with the known chain, if necessary.
        loop {
            let earliest_new_block = &added_blocks[0];
            if earliest_new_block.number == 0 {
                break;
            }
            let Some(presumed_parent) = self.block_with_number(earliest_new_block.number - 1)
            else {
                warn!(
                    "Reorg is deeper than chain history size ({})",
                    self.blocks.len()
                );
                break;
            };
            if presumed_parent.hash == earliest_new_block.parent_hash {
                break;
            }
            // The earliest newly loaded block's parent does not match the known
            // chain, so continue to load blocks backwards, replacing the known
            // chain, until it does.
            let block = self
                .provider
                .get_block(earliest_new_block.parent_hash)
                .await
                .context("should load parent block when handling reorg")?
                .context("block with parent hash of known block should exist")?;
            let block = BlockSummary::try_from_block_without_ops(
                block,
                Some(earliest_new_block.number - 1),
            )?;
            added_blocks.push_front(block);
        }
        self.load_ops_into_block_summaries(&mut added_blocks)
            .await?;
        Ok(added_blocks)
    }

    async fn load_blocks_back_to_number_no_ops(
        &self,
        head: BlockSummary,
        min_block_number: u64,
    ) -> anyhow::Result<VecDeque<BlockSummary>> {
        let mut blocks =
            VecDeque::with_capacity(head.number.saturating_sub(min_block_number) as usize + 1);
        blocks.push_front(head);
        while blocks[0].number > min_block_number {
            let parent_hash = blocks[0].parent_hash;
            let parent = self
                .provider
                .get_block(parent_hash)
                .await
                .context("should load parent block by hash")?
                .context("block with parent hash of known block should exist")?;
            blocks.push_front(BlockSummary::try_from_block_without_ops(
                parent,
                Some(blocks[0].number - 1),
            )?);
        }
        Ok(blocks)
    }

    async fn load_ops_into_block_summaries(
        &self,
        blocks: &mut VecDeque<BlockSummary>,
    ) -> anyhow::Result<()> {
        // As when loading blocks, load op events block-by-block, specifying
        // block hash. Don't load with a single call by block number range
        // because if the network is in the middle of a reorg, then we can't
        // tell which branch we read events from.
        let future_opses = blocks
            .iter()
            .map(|block| self.load_ops_in_block_with_hash(block.hash));
        let opses = future::try_join_all(future_opses)
            .await
            .context("should load ops for new blocks")?;
        for (i, (ops, deposits)) in opses.into_iter().enumerate() {
            blocks[i].ops = ops;
            blocks[i].entity_deposits = deposits;
        }
        Ok(())
    }

    async fn load_ops_in_block_with_hash(
        &self,
        block_hash: H256,
    ) -> anyhow::Result<(Vec<MinedOp>, Vec<DepositInfo>)> {
        let _permit = self
            .load_ops_semaphore
            .acquire()
            .await
            .expect("semaphore should not be closed");

        let deposit = DepositedFilter::abi_signature();
        let uo_filter = UserOperationEventFilter::abi_signature();
        let events: Vec<&str> = vec![&deposit, &uo_filter];

        let filter = Filter::new()
            .address(self.settings.entry_point_addresses.clone())
            .events(events)
            .at_block_hash(block_hash);
        let logs = self
            .provider
            .get_logs(&filter)
            .await
            .context("chain state should load user operation events")?;

        let deposits = self.load_entity_deposits(&logs);
        let mined_ops = self.load_mined_ops(&logs);

        Ok((mined_ops, deposits))
    }

    fn load_mined_ops(&self, logs: &Vec<Log>) -> Vec<MinedOp> {
        let mut mined_ops = vec![];
        for log in logs {
            let entry_point = log.address;
            if let Ok(event) = contract::parse_log::<UserOperationEventFilter>(log.clone()) {
                let paymaster = if event.paymaster.is_zero() {
                    None
                } else {
                    Some(event.paymaster)
                };

                let mined = MinedOp {
                    hash: event.user_op_hash.into(),
                    entry_point,
                    sender: event.sender,
                    nonce: event.nonce,
                    actual_gas_cost: event.actual_gas_cost,
                    paymaster,
                };

                mined_ops.push(mined);
            }
        }

        mined_ops
    }

    fn load_entity_deposits(&self, logs: &Vec<Log>) -> Vec<DepositInfo> {
        let mut deposits = vec![];
        for log in logs {
            let entrypoint = log.address;
            if let Ok(event) = contract::parse_log::<DepositedFilter>(log.clone()) {
                let info = DepositInfo {
                    entrypoint,
                    address: event.account,
                    amount: event.total_deposit,
                };

                deposits.push(info);
            }
        }

        deposits
    }

    fn block_with_number(&self, number: u64) -> Option<&BlockSummary> {
        let earliest_number = self.blocks.front()?.number;
        if number < earliest_number {
            return None;
        }
        self.blocks.get((number - earliest_number) as usize)
    }

    fn new_update(
        &self,
        reorg_depth: u64,
        mined_ops: Vec<MinedOp>,
        unmined_ops: Vec<MinedOp>,
        entity_deposits: Vec<DepositInfo>,
        unmined_entity_deposits: Vec<DepositInfo>,
        reorg_larger_than_history: bool,
    ) -> ChainUpdate {
        let latest_block = self
            .blocks
            .back()
            .expect("new_update should not be called when blocks is empty");
        ChainUpdate {
            latest_block_number: latest_block.number,
            latest_block_hash: latest_block.hash,
            latest_block_timestamp: latest_block.timestamp,
            earliest_remembered_block_number: self.blocks[0].number,
            reorg_depth,
            mined_ops,
            unmined_ops,
            entity_deposits,
            unmined_entity_deposits,
            reorg_larger_than_history,
        }
    }
}

impl BlockSummary {
    /// Converts a block returned from a provider into a `BlockSummary` with no
    /// ops. Takes an expected block number and returns an error if it doesn't
    /// match the block. While a provider should never return a block number
    /// that doesn't match what we expect, if the provider does return bad data
    /// it's better to catch it now than run into panics from bad indexing math
    /// later.
    fn try_from_block_without_ops(
        block: Block<H256>,
        expected_block_number: Option<u64>,
    ) -> anyhow::Result<Self> {
        let number = block
            .number
            .context("block number should be present")?
            .as_u64();
        if let Some(expected_block_number) = expected_block_number {
            ensure!(
                number == expected_block_number,
                "block number {number} should match expected {expected_block_number}"
            );
        }
        Ok(Self {
            number: block
                .number
                .context("block number should be present")?
                .as_u64(),
            hash: block.hash.context("block hash should exist")?,
            timestamp: block.timestamp.as_u64().into(),
            parent_hash: block.parent_hash,
            ops: Vec::new(),
            entity_deposits: Vec::new(),
        })
    }
}

#[derive(Debug)]
pub struct DedupedOps {
    pub mined_ops: Vec<MinedOp>,
    pub unmined_ops: Vec<MinedOp>,
}

impl ChainUpdate {
    /// "Cancels out" ops that appear in both mined and unmined.
    pub fn deduped_ops(&self) -> DedupedOps {
        let mined_op_hashes: HashSet<_> = self.mined_ops.iter().map(|op| op.hash).collect();
        let unmined_op_hashes: HashSet<_> = self.unmined_ops.iter().map(|op| op.hash).collect();
        let mined_ops = self
            .mined_ops
            .iter()
            .filter(|op| !unmined_op_hashes.contains(&op.hash))
            .copied()
            .collect();
        let unmined_ops = self
            .unmined_ops
            .iter()
            .filter(|op| !mined_op_hashes.contains(&op.hash))
            .copied()
            .collect();
        DedupedOps {
            mined_ops,
            unmined_ops,
        }
    }
}

struct ChainMetrics {}

impl ChainMetrics {
    fn set_block_height(block_height: u64) {
        metrics::gauge!("op_pool_chain_block_height", block_height as f64);
    }

    fn increment_reorgs_detected() {
        metrics::increment_counter!("op_pool_chain_reorgs_detected");
    }

    fn increment_total_reorg_depth(depth: u64) {
        metrics::counter!("op_pool_chain_total_reorg_depth", depth);
    }
}

#[cfg(test)]
mod tests {
    use std::ops::DerefMut;

    use ethers::{
        abi::AbiEncode,
        prelude::EthEvent,
        types::{FilterBlockOption, Log, H160},
        utils,
    };
    use parking_lot::RwLock;
    use rundler_provider::MockProvider;

    use super::*;

    const HISTORY_SIZE: u64 = 3;
    const ENTRY_POINT_ADDRESS: Address = H160(*b"01234567890123456789");

    #[derive(Clone, Debug)]
    struct MockBlock {
        hash: H256,
        op_hashes: Vec<H256>,
        deposit_addresses: Vec<Address>,
    }

    impl MockBlock {
        fn new(hash: H256, op_hashes: Vec<H256>, deposit_addresses: Vec<Address>) -> Self {
            Self {
                hash,
                op_hashes,
                deposit_addresses,
            }
        }
    }

    #[derive(Clone, Debug)]
    struct ProviderController {
        blocks: Arc<RwLock<Vec<MockBlock>>>,
    }

    impl ProviderController {
        fn set_blocks(&self, blocks: Vec<MockBlock>) {
            *self.blocks.write() = blocks;
        }

        fn get_blocks_mut(&self) -> impl DerefMut<Target = Vec<MockBlock>> + '_ {
            self.blocks.write()
        }

        fn get_head(&self) -> Block<H256> {
            let hash = self.blocks.read().last().unwrap().hash;
            self.get_block_by_hash(hash).unwrap()
        }

        fn get_block_by_hash(&self, hash: H256) -> Option<Block<H256>> {
            let blocks = self.blocks.read();
            let number = blocks.iter().position(|block| block.hash == hash)?;
            let parent_hash = if number > 0 {
                blocks[number - 1].hash
            } else {
                H256::zero()
            };
            Some(Block {
                hash: Some(hash),
                parent_hash,
                number: Some(number.into()),
                ..Default::default()
            })
        }

        fn get_logs_by_block_hash(&self, block_hash: H256) -> Vec<Log> {
            let blocks = self.blocks.read();
            let block = blocks.iter().find(|block| block.hash == block_hash);
            let Some(block) = block else {
                return vec![];
            };

            let mut joined_logs: Vec<Log> = Vec::new();
            joined_logs.extend(block.op_hashes.iter().copied().map(fake_log));
            joined_logs.extend(
                block
                    .deposit_addresses
                    .iter()
                    .copied()
                    .map(fake_deposit_log),
            );

            joined_logs
        }
    }

    #[tokio::test]
    async fn test_initial_load() {
        let (mut chain, controller) = new_chain();
        controller.set_blocks(vec![
            MockBlock::new(hash(0), vec![hash(101), hash(102)], vec![]),
            MockBlock::new(hash(1), vec![hash(103)], vec![]),
            MockBlock::new(hash(2), vec![], vec![]),
            MockBlock::new(hash(3), vec![hash(104), hash(105)], vec![]),
        ]);
        let update = chain.sync_to_block(controller.get_head()).await.unwrap();
        // With a history size of 3, we should get updates from all blocks except the first one.
        assert_eq!(
            update,
            ChainUpdate {
                latest_block_number: 3,
                latest_block_hash: hash(3),
                latest_block_timestamp: 0.into(),
                earliest_remembered_block_number: 1,
                reorg_depth: 0,
                mined_ops: vec![fake_mined_op(103), fake_mined_op(104), fake_mined_op(105),],
                unmined_ops: vec![],
                entity_deposits: vec![],
                unmined_entity_deposits: vec![],
                reorg_larger_than_history: false,
            }
        );
    }

    #[tokio::test]
    async fn test_simple_advance() {
        let (mut chain, controller) = new_chain();
        controller.set_blocks(vec![
            MockBlock::new(hash(0), vec![hash(101), hash(102)], vec![]),
            MockBlock::new(hash(1), vec![hash(103)], vec![]),
            MockBlock::new(hash(2), vec![], vec![]),
            MockBlock::new(hash(3), vec![hash(104), hash(105)], vec![]),
        ]);
        chain.sync_to_block(controller.get_head()).await.unwrap();
        controller
            .get_blocks_mut()
            .push(MockBlock::new(hash(4), vec![hash(106)], vec![]));
        let update = chain.sync_to_block(controller.get_head()).await.unwrap();
        assert_eq!(
            update,
            ChainUpdate {
                latest_block_number: 4,
                latest_block_hash: hash(4),
                latest_block_timestamp: 0.into(),
                earliest_remembered_block_number: 2,
                reorg_depth: 0,
                mined_ops: vec![fake_mined_op(106)],
                unmined_ops: vec![],
                entity_deposits: vec![],
                unmined_entity_deposits: vec![],
                reorg_larger_than_history: false,
            }
        );
    }

    #[tokio::test]
    async fn test_forward_reorg() {
        let (mut chain, controller) = new_chain();
        controller.set_blocks(vec![
            MockBlock::new(hash(0), vec![hash(100)], vec![]),
            MockBlock::new(hash(1), vec![hash(101)], vec![]),
            MockBlock::new(hash(2), vec![hash(102)], vec![Address::zero()]),
        ]);
        chain.sync_to_block(controller.get_head()).await.unwrap();
        {
            // Replaces the head of the chain with three new blocks.
            let mut blocks = controller.get_blocks_mut();
            blocks.pop();
            blocks.extend([
                MockBlock::new(hash(12), vec![hash(112)], vec![]),
                MockBlock::new(hash(13), vec![hash(113)], vec![]),
                MockBlock::new(hash(14), vec![hash(114)], vec![]),
            ]);
        }
        let update = chain.sync_to_block(controller.get_head()).await.unwrap();
        assert_eq!(
            update,
            ChainUpdate {
                latest_block_number: 4,
                latest_block_hash: hash(14),
                latest_block_timestamp: 0.into(),
                earliest_remembered_block_number: 2,
                reorg_depth: 1,
                mined_ops: vec![fake_mined_op(112), fake_mined_op(113), fake_mined_op(114)],
                unmined_ops: vec![fake_mined_op(102)],
                entity_deposits: vec![],
                unmined_entity_deposits: vec![fake_mined_deposit(Address::zero(), 0.into())],
                reorg_larger_than_history: false,
            }
        );
    }

    #[tokio::test]
    async fn test_sideways_reorg() {
        let (mut chain, controller) = new_chain();
        controller.set_blocks(vec![
            MockBlock::new(hash(0), vec![hash(100)], vec![]),
            MockBlock::new(hash(1), vec![hash(101)], vec![addr(1)]),
            MockBlock::new(hash(2), vec![hash(102)], vec![]),
        ]);
        chain.sync_to_block(controller.get_head()).await.unwrap();
        {
            // Replaces the top two blocks with two new ones.
            let mut blocks = controller.get_blocks_mut();
            blocks.pop();
            blocks.pop();
            blocks.extend([
                MockBlock::new(hash(11), vec![hash(111)], vec![addr(2)]),
                MockBlock::new(hash(12), vec![hash(112)], vec![]),
            ]);
        }
        let update = chain.sync_to_block(controller.get_head()).await.unwrap();
        assert_eq!(
            update,
            ChainUpdate {
                entity_deposits: vec![fake_mined_deposit(addr(2), 0.into())],
                latest_block_number: 2,
                latest_block_hash: hash(12),
                latest_block_timestamp: 0.into(),
                earliest_remembered_block_number: 0,
                reorg_depth: 2,
                mined_ops: vec![fake_mined_op(111), fake_mined_op(112)],
                unmined_ops: vec![fake_mined_op(101), fake_mined_op(102)],
                unmined_entity_deposits: vec![fake_mined_deposit(addr(1), 0.into())],
                reorg_larger_than_history: false,
            }
        );
    }

    #[tokio::test]
    async fn test_backwards_reorg() {
        let (mut chain, controller) = new_chain();
        controller.set_blocks(vec![
            MockBlock::new(hash(0), vec![hash(100)], vec![]),
            MockBlock::new(hash(1), vec![hash(101)], vec![]),
            MockBlock::new(hash(2), vec![hash(102)], vec![]),
        ]);
        chain.sync_to_block(controller.get_head()).await.unwrap();
        {
            // Replaces the top two blocks with just one new one.
            let mut blocks = controller.get_blocks_mut();
            blocks.pop();
            blocks.pop();
            blocks.push(MockBlock::new(hash(11), vec![hash(111)], vec![addr(1)]));
        }
        let update = chain.sync_to_block(controller.get_head()).await.unwrap();
        assert_eq!(
            update,
            ChainUpdate {
                latest_block_number: 1,
                entity_deposits: vec![fake_mined_deposit(addr(1), 0.into())],
                latest_block_hash: hash(11),
                latest_block_timestamp: 0.into(),
                earliest_remembered_block_number: 0,
                reorg_depth: 2,
                mined_ops: vec![fake_mined_op(111)],
                unmined_ops: vec![fake_mined_op(101), fake_mined_op(102)],
                unmined_entity_deposits: vec![],
                reorg_larger_than_history: false,
            }
        );
    }

    #[tokio::test]
    async fn test_reorg_longer_than_history() {
        let (mut chain, controller) = new_chain();
        controller.set_blocks(vec![
            MockBlock::new(hash(0), vec![hash(100)], vec![]),
            MockBlock::new(hash(1), vec![hash(101)], vec![]),
            MockBlock::new(hash(2), vec![hash(102)], vec![]),
            MockBlock::new(hash(3), vec![hash(103)], vec![]),
        ]);
        chain.sync_to_block(controller.get_head()).await.unwrap();
        // The history has size 3, so after this update it's completely unrecognizable.
        controller.set_blocks(vec![
            MockBlock::new(hash(0), vec![hash(100)], vec![]),
            MockBlock::new(hash(11), vec![hash(111)], vec![]),
            MockBlock::new(hash(12), vec![hash(112)], vec![]),
            MockBlock::new(hash(13), vec![hash(113)], vec![]),
        ]);
        let update = chain.sync_to_block(controller.get_head()).await.unwrap();
        assert_eq!(
            update,
            ChainUpdate {
                entity_deposits: vec![],
                latest_block_number: 3,
                latest_block_hash: hash(13),
                latest_block_timestamp: 0.into(),
                earliest_remembered_block_number: 1,
                reorg_depth: 3,
                mined_ops: vec![fake_mined_op(111), fake_mined_op(112), fake_mined_op(113)],
                unmined_ops: vec![fake_mined_op(101), fake_mined_op(102), fake_mined_op(103)],
                unmined_entity_deposits: vec![],
                reorg_larger_than_history: true,
            }
        );
    }

    #[tokio::test]
    async fn test_advance_larger_than_history_size() {
        let (mut chain, controller) = new_chain();
        controller.set_blocks(vec![
            MockBlock::new(hash(0), vec![hash(100)], vec![]),
            MockBlock::new(hash(1), vec![hash(101)], vec![]),
            MockBlock::new(hash(2), vec![hash(102)], vec![]),
        ]);
        chain.sync_to_block(controller.get_head()).await.unwrap();
        {
            let mut blocks = controller.get_blocks_mut();
            for i in 3..7 {
                blocks.push(MockBlock::new(hash(10 + i), vec![hash(100 + i)], vec![]));
            }
        }
        let update = chain.sync_to_block(controller.get_head()).await.unwrap();
        assert_eq!(
            update,
            ChainUpdate {
                latest_block_number: 6,
                latest_block_hash: hash(16),
                latest_block_timestamp: 0.into(),
                earliest_remembered_block_number: 4,
                reorg_depth: 0,
                entity_deposits: vec![],
                mined_ops: vec![fake_mined_op(104), fake_mined_op(105), fake_mined_op(106)],
                unmined_ops: vec![],
                unmined_entity_deposits: vec![],
                reorg_larger_than_history: false,
            }
        );
    }

    /// This test probably only matters for running against a local chain.
    #[tokio::test]
    async fn test_latest_block_number_smaller_than_history_size() {
        let (mut chain, controller) = new_chain();
        let blocks = vec![
            MockBlock::new(hash(0), vec![hash(101), hash(102)], vec![]),
            MockBlock::new(hash(1), vec![hash(103)], vec![]),
        ];
        controller.set_blocks(blocks);
        let update = chain.sync_to_block(controller.get_head()).await.unwrap();
        assert_eq!(
            update,
            ChainUpdate {
                latest_block_number: 1,
                latest_block_hash: hash(1),
                latest_block_timestamp: 0.into(),
                earliest_remembered_block_number: 0,
                reorg_depth: 0,
                entity_deposits: vec![],
                mined_ops: vec![fake_mined_op(101), fake_mined_op(102), fake_mined_op(103),],
                unmined_ops: vec![],
                unmined_entity_deposits: vec![],
                reorg_larger_than_history: false,
            }
        );
    }

    fn new_chain() -> (Chain<impl Provider>, ProviderController) {
        let (provider, controller) = new_mock_provider();
        let chain = Chain::new(
            Arc::new(provider),
            Settings {
                history_size: HISTORY_SIZE,
                poll_interval: Duration::from_secs(250), // Not used in tests.
                entry_point_addresses: vec![ENTRY_POINT_ADDRESS],
            },
        );
        (chain, controller)
    }

    fn new_mock_provider() -> (impl Provider, ProviderController) {
        let controller = ProviderController {
            blocks: Arc::new(RwLock::new(vec![])),
        };
        let mut provider = MockProvider::new();

        provider.expect_get_block::<H256>().returning({
            let controller = controller.clone();
            move |hash| Ok(controller.get_block_by_hash(hash))
        });

        provider.expect_get_logs().returning({
            let controller = controller.clone();
            move |filter| {
                let FilterBlockOption::AtBlockHash(block_hash) = filter.block_option else {
                    panic!("mock provider only supports getLogs at specific block hashes");
                };
                Ok(controller.get_logs_by_block_hash(block_hash))
            }
        });

        (provider, controller)
    }

    fn fake_log(op_hash: H256) -> Log {
        Log {
            address: ENTRY_POINT_ADDRESS,
            topics: vec![
                H256::from(utils::keccak256(
                    UserOperationEventFilter::abi_signature().as_bytes(),
                )),
                op_hash,
                H256::zero(), // sender
                H256::zero(), // paymaster
            ],
            data: AbiEncode::encode((
                U256::zero(), // nonce
                true,         // success
                U256::zero(), // actual_gas_cost
                U256::zero(), // actual_gas_used
            ))
            .into(),
            ..Default::default()
        }
    }

    fn fake_deposit_log(deposit_address: Address) -> Log {
        Log {
            address: ENTRY_POINT_ADDRESS,
            topics: vec![
                H256::from(utils::keccak256(
                    DepositedFilter::abi_signature().as_bytes(),
                )),
                H256::from(deposit_address),
            ],
            data: AbiEncode::encode((
                U256::zero(), // totalDeposits
            ))
            .into(),
            ..Default::default()
        }
    }

    fn fake_mined_op(n: u8) -> MinedOp {
        MinedOp {
            hash: hash(n),
            entry_point: ENTRY_POINT_ADDRESS,
            sender: Address::zero(),
            nonce: U256::zero(),
            actual_gas_cost: U256::zero(),
            paymaster: None,
        }
    }

    fn fake_mined_deposit(address: Address, amount: U256) -> DepositInfo {
        DepositInfo {
            address,
            entrypoint: ENTRY_POINT_ADDRESS,
            amount,
        }
    }

    // Helper that makes fake hashes.
    fn hash(n: u8) -> H256 {
        let mut hash = H256::zero();
        hash.0[0] = n;
        hash
    }

    // Helper that makes fake addresses.
    fn addr(n: u8) -> Address {
        let mut address = Address::zero();
        address.0[0] = n;
        address
    }
}
