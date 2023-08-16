//! Support for pruning.

use crate::{Metrics, PrunerError};
use rayon::prelude::*;
use reth_db::{
    abstraction::cursor::{DbCursorRO, DbCursorRW},
    database::Database,
    models::{storage_sharded_key::StorageShardedKey, BlockNumberAddress, ShardedKey},
    table::Table,
    tables,
    transaction::DbTxMut,
    BlockNumberList,
};
use reth_primitives::{
    BlockNumber, ChainSpec, PruneCheckpoint, PruneMode, PruneModes, PrunePart, TxNumber,
};
use reth_provider::{
    BlockReader, DatabaseProviderRW, ProviderFactory, PruneCheckpointReader, PruneCheckpointWriter,
    TransactionsProvider,
};
use std::{ops::RangeInclusive, sync::Arc, time::Instant};
use tracing::{debug, instrument, trace};

type ResultWithDone<T> = Result<Option<(T, bool)>, PrunerError>;

/// Result of [Pruner::run] execution
pub type PrunerResult = Result<(), PrunerError>;

/// Result of prune part execution.
///
/// Returns:
/// - [Option::Some] with number of rows pruned and if this is final block range
/// - [Option::None] if there was nothing to prune
type PrunerPartResult = ResultWithDone<usize>;

/// The pipeline type itself with the result of [Pruner::run]
pub type PrunerWithResult<DB> = (Pruner<DB>, PrunerResult);

pub struct CommitThresholds {
    receipts: usize,
    transaction_lookup: usize,
    transaction_senders: usize,
    account_history: usize,
    storage_history: usize,
}

impl Default for CommitThresholds {
    fn default() -> Self {
        Self {
            receipts: 10000,
            transaction_lookup: 10000,
            transaction_senders: 10000,
            account_history: 10000,
            storage_history: 10000,
        }
    }
}

/// Pruning routine. Main pruning logic happens in [Pruner::run].
pub struct Pruner<DB> {
    metrics: Metrics,
    provider_factory: ProviderFactory<DB>,
    /// Minimum pruning interval measured in blocks. All prune parts are checked and, if needed,
    /// pruned, when the chain advances by the specified number of blocks.
    min_block_interval: u64,
    /// Last pruned block number. Used in conjunction with `min_block_interval` to determine
    /// when the pruning needs to be initiated.
    last_pruned_block_number: Option<BlockNumber>,
    modes: PruneModes,
    commit_thresholds: CommitThresholds,
}

impl<DB: Database> Pruner<DB> {
    /// Creates a new [Pruner].
    pub fn new(
        db: DB,
        chain_spec: Arc<ChainSpec>,
        min_block_interval: u64,
        modes: PruneModes,
        commit_thresholds: CommitThresholds,
    ) -> Self {
        Self {
            metrics: Metrics::default(),
            provider_factory: ProviderFactory::new(db, chain_spec),
            min_block_interval,
            last_pruned_block_number: None,
            modes,
            commit_thresholds,
        }
    }

    /// Run the pruner
    pub fn run(&mut self, tip_block_number: BlockNumber) -> PrunerResult {
        trace!(
            target: "pruner",
            %tip_block_number,
            "Pruner started"
        );
        let start = Instant::now();

        if let Some((to_block, prune_mode)) =
            self.modes.prune_target_block_receipts(tip_block_number)?
        {
            self.prune_part_until_completion(
                PrunePart::Receipts,
                to_block,
                prune_mode,
                Self::prune_receipts,
            )?;
        }

        if let Some((to_block, prune_mode)) =
            self.modes.prune_target_block_transaction_lookup(tip_block_number)?
        {
            self.prune_part_until_completion(
                PrunePart::TransactionLookup,
                to_block,
                prune_mode,
                Self::prune_transaction_lookup,
            )?;
        }

        if let Some((to_block, prune_mode)) =
            self.modes.prune_target_block_sender_recovery(tip_block_number)?
        {
            self.prune_part_until_completion(
                PrunePart::SenderRecovery,
                to_block,
                prune_mode,
                Self::prune_transaction_senders,
            )?;
        }

        if let Some((to_block, prune_mode)) =
            self.modes.prune_target_block_account_history(tip_block_number)?
        {
            self.prune_part_until_completion(
                PrunePart::AccountHistory,
                to_block,
                prune_mode,
                Self::prune_account_history,
            )?;
        }

        if let Some((to_block, prune_mode)) =
            self.modes.prune_target_block_storage_history(tip_block_number)?
        {
            self.prune_part_until_completion(
                PrunePart::StorageHistory,
                to_block,
                prune_mode,
                Self::prune_storage_history,
            )?;
        }

        self.last_pruned_block_number = Some(tip_block_number);

        let elapsed = start.elapsed();
        self.metrics.duration_seconds.record(elapsed);

        trace!(
            target: "pruner",
            %tip_block_number,
            ?elapsed,
            "Pruner finished"
        );
        Ok(())
    }

    /// Returns `true` if the pruning is needed at the provided tip block number.
    /// This determined by the check against minimum pruning interval and last pruned block number.
    pub fn is_pruning_needed(&self, tip_block_number: BlockNumber) -> bool {
        if self.last_pruned_block_number.map_or(true, |last_pruned_block_number| {
            // Saturating subtraction is needed for the case when the chain was reverted, meaning
            // current block number might be less than the previously pruned block number. If
            // that's the case, no pruning is needed as outdated data is also reverted.
            tip_block_number.saturating_sub(last_pruned_block_number) >= self.min_block_interval
        }) {
            debug!(
                target: "pruner",
                last_pruned_block_number = ?self.last_pruned_block_number,
                %tip_block_number,
                "Minimum pruning interval reached"
            );
            true
        } else {
            false
        }
    }

    /// Prunes the specified [PrunePart] until the provided `prune` function returns
    /// `Some((_, true))` or `None`, meaning it's a final range.
    ///
    /// This method executes the `prune` function inside a loop, starting a new database transaction
    /// for each `prune` call, and committing it at the end of the loop iteration.
    /// The prune part duration metrics are recorded in the same way: inside each loop iteration.
    fn prune_part_until_completion(
        &mut self,
        prune_part: PrunePart,
        to_block: BlockNumber,
        prune_mode: PruneMode,
        prune: impl Fn(
            &Pruner<DB>,
            &DatabaseProviderRW<'_, DB>,
            BlockNumber,
            PruneMode,
        ) -> PrunerPartResult,
    ) -> PrunerResult {
        let mut done = false;
        while !done {
            let provider = self.provider_factory.provider_rw()?;

            let start = Instant::now();
            let rows;
            (rows, done) = match prune(self, &provider, to_block, prune_mode)? {
                Some((rows, done)) => (rows, done),
                None => {
                    trace!(target: "pruner", part = %prune_part, "Nothing to prune");
                    return Ok(())
                }
            };
            self.metrics
                .get_prune_part_metrics(prune_part)
                .duration_seconds
                .record(start.elapsed());

            trace!(target: "pruner", part = %prune_part, %rows, %done, "Pruner iteration completed");

            provider.commit()?;
        }

        Ok(())
    }

    /// Get next inclusive tx number range to prune according to the checkpoint, `to_block` block
    /// number and `limit`.
    ///
    /// To get the range start (`from_block`):
    /// 1. If checkpoint exists, get body for the next block and return its first tx number.
    /// 2. If checkpoint doesn't exist, get body for the block 0 and returns it first tx number.
    ///
    /// To get the range end: get last tx number for block `min(to_block, from_block + limit - 1)`.
    fn get_next_tx_num_range_from_checkpoint(
        &self,
        provider: &DatabaseProviderRW<'_, DB>,
        prune_part: PrunePart,
        to_block: BlockNumber,
        limit: usize,
    ) -> ResultWithDone<(RangeInclusive<BlockNumber>, Option<RangeInclusive<TxNumber>>)> {
        let from_block = provider
            .get_prune_checkpoint(prune_part)?
            // Checkpoint exists, prune from the next block after the highest pruned one
            .map(|checkpoint| checkpoint.block_number + 1)
            // No checkpoint exists, prune from genesis
            .unwrap_or(0);

        // Get first transaction. If no block body index is found, the DB is either corrupted or
        // we've already pruned up to the latest block, so there's no thing to prune now.
        let from_tx_num = match provider.block_body_indices(from_block)? {
            Some(body) => body,
            None => return Ok(None),
        }
        .first_tx_num;

        let block_range = from_block..=to_block.min(from_block + limit as u64 - 1);
        // Get last transaction. If no block body index is found, the DB is either corrupted or
        // we've already pruned up to the latest block, so there's no thing to prune now.
        let to_tx_num = match provider.block_body_indices(*block_range.end())? {
            Some(body) => body,
            None => return Ok(None),
        }
        .last_tx_num();

        let is_final_range = *block_range.end() == to_block;

        let range = from_tx_num..=to_tx_num;
        if range.is_empty() {
            return Ok(Some(((block_range, None), is_final_range)))
        }

        Ok(Some(((block_range, Some(range)), is_final_range)))
    }

    /// Get next inclusive block range to prune according to the checkpoint, `to_block` block
    /// number and `limit`.
    ///
    /// To get the range start (`from_block`):
    /// 1. If checkpoint exists, use next block.
    /// 2. If checkpoint doesn't exist, use block 0.
    ///
    /// To get the range end: use block `min(to_block, from_block + limit - 1)`.
    fn get_next_block_range_from_checkpoint(
        &self,
        provider: &DatabaseProviderRW<'_, DB>,
        prune_part: PrunePart,
        to_block: BlockNumber,
        limit: usize,
    ) -> ResultWithDone<RangeInclusive<BlockNumber>> {
        let from_block = provider
            .get_prune_checkpoint(prune_part)?
            // Checkpoint exists, prune from the next block after the highest pruned one
            .map(|checkpoint| checkpoint.block_number + 1)
            // No checkpoint exists, prune from genesis
            .unwrap_or(0);

        let range = from_block..=to_block.min(from_block + limit as u64 - 1);
        if range.is_empty() {
            return Ok(None)
        }

        let is_final_range = *range.end() == to_block;
        Ok(Some((range, is_final_range)))
    }

    /// Prune receipts up to the provided block, inclusive.
    #[instrument(level = "trace", skip(self, provider), target = "pruner")]
    fn prune_receipts(
        &self,
        provider: &DatabaseProviderRW<'_, DB>,
        to_block: BlockNumber,
        prune_mode: PruneMode,
    ) -> PrunerPartResult {
        let ((block_range, range), is_final_range) = match self
            .get_next_tx_num_range_from_checkpoint(
                provider,
                PrunePart::Receipts,
                to_block,
                self.commit_thresholds.receipts,
            )? {
            Some(result) => result,
            None => return Ok(None),
        };

        let rows = if let Some(range) = range {
            let (_, rows) = provider.prune_table_with_range::<tables::Receipts>(range)?;
            rows
        } else {
            0
        };

        provider.save_prune_checkpoint(
            PrunePart::Receipts,
            PruneCheckpoint { block_number: *block_range.end(), prune_mode },
        )?;

        Ok(Some((rows, is_final_range)))
    }

    /// Prune transaction lookup entries up to the provided block, inclusive.
    #[instrument(level = "trace", skip(self, provider), target = "pruner")]
    fn prune_transaction_lookup(
        &self,
        provider: &DatabaseProviderRW<'_, DB>,
        to_block: BlockNumber,
        prune_mode: PruneMode,
    ) -> PrunerPartResult {
        let ((block_range, range), is_final_range) = match self
            .get_next_tx_num_range_from_checkpoint(
                provider,
                PrunePart::TransactionLookup,
                to_block,
                self.commit_thresholds.transaction_lookup,
            )? {
            Some(result) => result,
            None => return Ok(None),
        };

        let rows = if let Some(range) = range {
            // Retrieve transactions in the range and calculate their hashes in parallel
            let mut hashes = provider
                .transactions_by_tx_range(range.clone())?
                .into_par_iter()
                .map(|transaction| transaction.hash())
                .collect::<Vec<_>>();

            // Number of transactions retrieved from the database should match the tx range count
            let tx_count = range.clone().count();
            if hashes.len() != tx_count {
                return Err(PrunerError::InconsistentData(
                    "Unexpected number of transaction hashes retrieved by transaction number range",
                ))
            }

            // Pre-sort hashes to prune them in order
            hashes.sort_unstable();

            provider.prune_table_with_iterator::<tables::TxHashNumber>(hashes)?
        } else {
            0
        };

        provider.save_prune_checkpoint(
            PrunePart::TransactionLookup,
            PruneCheckpoint { block_number: *block_range.end(), prune_mode },
        )?;

        Ok(Some((rows, is_final_range)))
    }

    /// Prune transaction senders up to the provided block, inclusive.
    #[instrument(level = "trace", skip(self, provider), target = "pruner")]
    fn prune_transaction_senders(
        &self,
        provider: &DatabaseProviderRW<'_, DB>,
        to_block: BlockNumber,
        prune_mode: PruneMode,
    ) -> PrunerPartResult {
        let ((block_range, range), is_final_range) = match self
            .get_next_tx_num_range_from_checkpoint(
                provider,
                PrunePart::SenderRecovery,
                to_block,
                self.commit_thresholds.transaction_senders,
            )? {
            Some(result) => result,
            None => return Ok(None),
        };

        let rows = if let Some(range) = range {
            let (_, rows) = provider.prune_table_with_range::<tables::TxSenders>(range)?;
            rows
        } else {
            0
        };

        provider.save_prune_checkpoint(
            PrunePart::SenderRecovery,
            PruneCheckpoint { block_number: *block_range.end(), prune_mode },
        )?;

        Ok(Some((rows, is_final_range)))
    }

    /// Prune account history up to the provided block, inclusive.
    #[instrument(level = "trace", skip(self, provider), target = "pruner")]
    fn prune_account_history(
        &self,
        provider: &DatabaseProviderRW<'_, DB>,
        to_block: BlockNumber,
        prune_mode: PruneMode,
    ) -> PrunerPartResult {
        let (range, is_final_range) = match self.get_next_block_range_from_checkpoint(
            provider,
            PrunePart::AccountHistory,
            to_block,
            self.commit_thresholds.account_history,
        )? {
            Some(result) => result,
            None => return Ok(None),
        };

        let (_, rows) =
            provider.prune_table_with_range::<tables::AccountChangeSet>(range.clone())?;

        self.prune_history_indices::<tables::AccountHistory, _>(
            provider,
            *range.end(),
            |a, b| a.key == b.key,
            |key| ShardedKey::last(key.key),
        )?;

        provider.save_prune_checkpoint(
            PrunePart::AccountHistory,
            PruneCheckpoint { block_number: *range.end(), prune_mode },
        )?;

        Ok(Some((rows, is_final_range)))
    }

    /// Prune storage history up to the provided block, inclusive.
    #[instrument(level = "trace", skip(self, provider), target = "pruner")]
    fn prune_storage_history(
        &self,
        provider: &DatabaseProviderRW<'_, DB>,
        to_block: BlockNumber,
        prune_mode: PruneMode,
    ) -> PrunerPartResult {
        let (range, is_final_range) = match self.get_next_block_range_from_checkpoint(
            provider,
            PrunePart::StorageHistory,
            to_block,
            self.commit_thresholds.storage_history,
        )? {
            Some(result) => result,
            None => return Ok(None),
        };

        let (keys, _) = provider.prune_table_with_range::<tables::StorageChangeSet>(
            BlockNumberAddress::range(range.clone()),
        )?;

        self.prune_history_indices::<tables::StorageHistory, _>(
            provider,
            *range.end(),
            |a, b| a.address == b.address && a.sharded_key.key == b.sharded_key.key,
            |key| StorageShardedKey::last(key.address, key.sharded_key.key),
        )?;

        provider.save_prune_checkpoint(
            PrunePart::StorageHistory,
            PruneCheckpoint { block_number: *range.end(), prune_mode },
        )?;

        Ok(Some((keys, is_final_range)))
    }

    /// Prune history indices up to the provided block, inclusive.
    fn prune_history_indices<T, SK>(
        &self,
        provider: &DatabaseProviderRW<'_, DB>,
        to_block: BlockNumber,
        key_matches: impl Fn(&T::Key, &T::Key) -> bool,
        last_key: impl Fn(&T::Key) -> T::Key,
    ) -> PrunerResult
    where
        T: Table<Value = BlockNumberList>,
        T::Key: AsRef<ShardedKey<SK>>,
    {
        let mut cursor = provider.tx_ref().cursor_write::<T>()?;

        // Prune history table:
        // 1. If the shard has `highest_block_number` less than or equal to the target block number
        // for pruning, delete the shard completely.
        // 2. If the shard has `highest_block_number` greater than the target block number for
        // pruning, filter block numbers inside the shard which are less than the target
        // block number for pruning.
        while let Some(result) = cursor.next()? {
            let (key, blocks): (T::Key, BlockNumberList) = result;

            if key.as_ref().highest_block_number <= to_block {
                // If shard consists only of block numbers less than the target one, delete shard
                // completely.
                cursor.delete_current()?;
                if key.as_ref().highest_block_number == to_block {
                    // Shard contains only block numbers up to the target one, so we can skip to the
                    // next sharded key. It is guaranteed that further shards for this sharded key
                    // will not contain the target block number, as it's in this shard.
                    cursor.seek_exact(last_key(&key))?;
                }
            } else {
                // Shard contains block numbers that are higher than the target one, so we need to
                // filter it. It is guaranteed that further shards for this sharded key will not
                // contain the target block number, as it's in this shard.
                let new_blocks = blocks
                    .iter(0)
                    .skip_while(|block| *block <= to_block as usize)
                    .collect::<Vec<_>>();

                if blocks.len() != new_blocks.len() {
                    // If there were blocks less than or equal to the target one
                    // (so the shard has changed), update the shard.
                    if new_blocks.is_empty() {
                        // If there are no more blocks in this shard, we need to remove it, as empty
                        // shards are not allowed.
                        if key.as_ref().highest_block_number == u64::MAX {
                            if let Some(prev_value) = cursor
                                .prev()?
                                .filter(|(prev_key, _)| key_matches(prev_key, &key))
                                .map(|(_, prev_value)| prev_value)
                            {
                                // If current shard is the last shard for the sharded key that has
                                // previous shards, replace it with the previous shard.
                                cursor.delete_current()?;
                                // Upsert will replace the last shard for this sharded key with the
                                // previous value.
                                cursor.upsert(key.clone(), prev_value)?;
                            } else {
                                // If there's no previous shard for this sharded key,
                                // just delete last shard completely.

                                // Jump back to the original last shard.
                                cursor.next()?;
                                // Delete shard.
                                cursor.delete_current()?;
                            }
                        } else {
                            // If current shard is not the last shard for this sharded key,
                            // just delete it.
                            cursor.delete_current()?;
                        }
                    } else {
                        cursor.upsert(key.clone(), BlockNumberList::new_pre_sorted(new_blocks))?;
                    }
                }

                // Jump to the next address.
                cursor.seek_exact(last_key(&key))?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{pruner::CommitThresholds, Pruner};
    use reth_db::{tables, test_utils::create_test_rw_db, BlockNumberList};
    use reth_interfaces::test_utils::{
        generators,
        generators::{
            random_block_range, random_changeset_range, random_eoa_account_range, random_receipt,
        },
    };
    use reth_primitives::{
        BlockNumber, PruneCheckpoint, PruneMode, PruneModes, PrunePart, H256, MAINNET,
    };
    use reth_provider::PruneCheckpointReader;
    use reth_stages::test_utils::TestTransaction;
    use std::{
        collections::BTreeMap,
        ops::{AddAssign, RangeInclusive},
    };

    #[test]
    fn is_pruning_needed() {
        let db = create_test_rw_db();
        let pruner =
            Pruner::new(db, MAINNET.clone(), 5, PruneModes::default(), CommitThresholds::default());

        // No last pruned block number was set before
        let first_block_number = 1;
        assert!(pruner.is_pruning_needed(first_block_number));

        // Delta is not less than min block interval
        let second_block_number = first_block_number + pruner.min_block_interval;
        assert!(pruner.is_pruning_needed(second_block_number));

        // Delta is less than min block interval
        let third_block_number = second_block_number;
        assert!(pruner.is_pruning_needed(third_block_number));
    }

    #[test]
    fn prune_receipts() {
        let tx = TestTransaction::default();
        let mut rng = generators::rng();

        let blocks = random_block_range(&mut rng, 0..=100, H256::zero(), 0..10);
        tx.insert_blocks(blocks.iter(), None).expect("insert blocks");

        let mut receipts = Vec::new();
        for block in &blocks {
            for transaction in &block.body {
                receipts
                    .push((receipts.len() as u64, random_receipt(&mut rng, transaction, Some(0))));
            }
        }
        tx.insert_receipts(receipts).expect("insert receipts");

        assert_eq!(
            tx.table::<tables::Transactions>().unwrap().len(),
            blocks.iter().map(|block| block.body.len()).sum::<usize>()
        );
        assert_eq!(
            tx.table::<tables::Transactions>().unwrap().len(),
            tx.table::<tables::Receipts>().unwrap().len()
        );

        let test_prune = |to_block, pruned_blocks: RangeInclusive<usize>, is_final_range| {
            let prune_mode = PruneMode::Before(to_block);
            let pruner = Pruner::new(
                tx.inner_raw(),
                MAINNET.clone(),
                5,
                PruneModes { receipts: Some(prune_mode), ..Default::default() },
                CommitThresholds { receipts: 10, ..Default::default() },
            );

            let last_pruned_block = *pruned_blocks.end();
            let pruned = blocks[pruned_blocks].iter().map(|block| block.body.len()).sum::<usize>();

            let provider = tx.inner_rw();
            assert_eq!(
                pruner.prune_receipts(&provider, to_block, prune_mode),
                Ok(Some((pruned, is_final_range)))
            );
            provider.commit().expect("commit");

            assert_eq!(
                tx.table::<tables::Receipts>().unwrap().len(),
                blocks[last_pruned_block + 1..].iter().map(|block| block.body.len()).sum::<usize>()
            );
            assert_eq!(
                tx.inner().get_prune_checkpoint(PrunePart::Receipts).unwrap(),
                Some(PruneCheckpoint {
                    block_number: last_pruned_block as BlockNumber,
                    prune_mode
                })
            );
        };

        // Pruning first time ever, no previous checkpoint is present
        test_prune(11, 0..=9, false);
        // Prune second time, previous checkpoint is present, should continue pruning from where
        // ended last time
        test_prune(11, 10..=11, true);
    }

    #[test]
    fn prune_transaction_lookup() {
        let tx = TestTransaction::default();
        let mut rng = generators::rng();

        let blocks = random_block_range(&mut rng, 0..=100, H256::zero(), 0..10);
        tx.insert_blocks(blocks.iter(), None).expect("insert blocks");

        let mut tx_hash_numbers = Vec::new();
        for block in &blocks {
            for transaction in &block.body {
                tx_hash_numbers.push((transaction.hash, tx_hash_numbers.len() as u64));
            }
        }
        tx.insert_tx_hash_numbers(tx_hash_numbers).expect("insert tx hash numbers");

        assert_eq!(
            tx.table::<tables::Transactions>().unwrap().len(),
            blocks.iter().map(|block| block.body.len()).sum::<usize>()
        );
        assert_eq!(
            tx.table::<tables::Transactions>().unwrap().len(),
            tx.table::<tables::TxHashNumber>().unwrap().len()
        );

        let test_prune = |to_block: BlockNumber,
                          pruned_blocks: RangeInclusive<usize>,
                          is_final_range| {
            let prune_mode = PruneMode::Before(to_block);
            let pruner = Pruner::new(
                tx.inner_raw(),
                MAINNET.clone(),
                5,
                PruneModes { transaction_lookup: Some(prune_mode), ..Default::default() },
                CommitThresholds { transaction_lookup: 10, ..Default::default() },
            );

            let last_pruned_block = *pruned_blocks.end();
            let pruned = blocks[pruned_blocks].iter().map(|block| block.body.len()).sum::<usize>();

            let provider = tx.inner_rw();
            assert_eq!(
                pruner.prune_transaction_lookup(&provider, to_block, prune_mode),
                Ok(Some((pruned, is_final_range)))
            );
            provider.commit().expect("commit");

            assert_eq!(
                tx.table::<tables::TxHashNumber>().unwrap().len(),
                blocks[last_pruned_block + 1..].iter().map(|block| block.body.len()).sum::<usize>()
            );
            assert_eq!(
                tx.inner().get_prune_checkpoint(PrunePart::TransactionLookup).unwrap(),
                Some(PruneCheckpoint {
                    block_number: last_pruned_block as BlockNumber,
                    prune_mode
                })
            );
        };

        // Pruning first time ever, no previous checkpoint is present
        test_prune(11, 0..=9, false);
        // Prune second time, previous checkpoint is present, should continue pruning from where
        // ended last time
        test_prune(11, 10..=11, true);
    }

    #[test]
    fn prune_transaction_senders() {
        let tx = TestTransaction::default();
        let mut rng = generators::rng();

        let blocks = random_block_range(&mut rng, 0..=100, H256::zero(), 0..10);
        tx.insert_blocks(blocks.iter(), None).expect("insert blocks");

        let mut transaction_senders = Vec::new();
        for block in &blocks {
            for transaction in &block.body {
                transaction_senders.push((
                    transaction_senders.len() as u64,
                    transaction.recover_signer().expect("recover signer"),
                ));
            }
        }
        tx.insert_transaction_senders(transaction_senders).expect("insert transaction senders");

        assert_eq!(
            tx.table::<tables::Transactions>().unwrap().len(),
            blocks.iter().map(|block| block.body.len()).sum::<usize>()
        );
        assert_eq!(
            tx.table::<tables::Transactions>().unwrap().len(),
            tx.table::<tables::TxSenders>().unwrap().len()
        );

        let test_prune = |to_block: BlockNumber,
                          pruned_blocks: RangeInclusive<usize>,
                          is_final_range| {
            let prune_mode = PruneMode::Before(to_block);
            let pruner = Pruner::new(
                tx.inner_raw(),
                MAINNET.clone(),
                5,
                PruneModes { sender_recovery: Some(prune_mode), ..Default::default() },
                CommitThresholds { transaction_senders: 10, ..Default::default() },
            );

            let last_pruned_block = *pruned_blocks.end();
            let pruned = blocks[pruned_blocks].iter().map(|block| block.body.len()).sum::<usize>();

            let provider = tx.inner_rw();
            assert_eq!(
                pruner.prune_transaction_senders(&provider, to_block, prune_mode),
                Ok(Some((pruned, is_final_range)))
            );
            provider.commit().expect("commit");

            assert_eq!(
                tx.table::<tables::TxSenders>().unwrap().len(),
                blocks[last_pruned_block + 1..].iter().map(|block| block.body.len()).sum::<usize>()
            );
            assert_eq!(
                tx.inner().get_prune_checkpoint(PrunePart::SenderRecovery).unwrap(),
                Some(PruneCheckpoint {
                    block_number: last_pruned_block as BlockNumber,
                    prune_mode
                })
            );
        };

        // Pruning first time ever, no previous checkpoint is present
        test_prune(11, 0..=9, false);
        // Prune second time, previous checkpoint is present, should continue pruning from where
        // ended last time
        test_prune(11, 10..=11, true);
    }

    #[test]
    fn prune_account_history() {
        let tx = TestTransaction::default();
        let mut rng = generators::rng();

        let block_num = 7000;
        let blocks = random_block_range(&mut rng, 0..=block_num, H256::zero(), 0..1);
        tx.insert_blocks(blocks.iter(), None).expect("insert blocks");

        let accounts =
            random_eoa_account_range(&mut rng, 0..3).into_iter().collect::<BTreeMap<_, _>>();

        let (changesets, _) = random_changeset_range(
            &mut rng,
            blocks.iter(),
            accounts.into_iter().map(|(addr, acc)| (addr, (acc, Vec::new()))),
            0..0,
            0..0,
        );
        tx.insert_changesets(changesets.clone(), None).expect("insert changesets");
        tx.insert_history(changesets.clone(), None).expect("insert history");

        let account_occurrences = tx.table::<tables::AccountHistory>().unwrap().into_iter().fold(
            BTreeMap::<_, usize>::new(),
            |mut map, (key, _)| {
                map.entry(key.key).or_default().add_assign(1);
                map
            },
        );
        assert!(account_occurrences.into_iter().any(|(_, occurrences)| occurrences > 1));

        assert_eq!(
            tx.table::<tables::AccountChangeSet>().unwrap().len(),
            changesets.iter().flatten().count()
        );

        let original_shards = tx.table::<tables::AccountHistory>().unwrap();

        let test_prune =
            |to_block: BlockNumber, pruned_blocks: RangeInclusive<usize>, is_final_range| {
                let prune_mode = PruneMode::Before(to_block);
                let pruner = Pruner::new(
                    tx.inner_raw(),
                    MAINNET.clone(),
                    5,
                    PruneModes { account_history: Some(prune_mode), ..Default::default() },
                    CommitThresholds {
                        // Less than total amount of blocks to prune to test the batching logic
                        account_history: 3000,
                        ..Default::default()
                    },
                );

                let last_pruned_block = *pruned_blocks.end();

                let provider = tx.inner_rw();
                assert_eq!(
                    pruner.prune_account_history(&provider, to_block, prune_mode),
                    Ok(Some((changesets[pruned_blocks].iter().flatten().count(), is_final_range)))
                );
                provider.commit().expect("commit");

                assert_eq!(
                    tx.table::<tables::AccountChangeSet>().unwrap().len(),
                    changesets[last_pruned_block + 1..].iter().flatten().count()
                );

                let actual_shards = tx.table::<tables::AccountHistory>().unwrap();

                let expected_shards = original_shards
                    .iter()
                    .filter(|(key, _)| key.highest_block_number > last_pruned_block as u64)
                    .map(|(key, blocks)| {
                        let new_blocks = blocks
                            .iter(0)
                            .skip_while(|block| *block <= last_pruned_block)
                            .collect::<Vec<_>>();
                        (key.clone(), BlockNumberList::new_pre_sorted(new_blocks))
                    })
                    .collect::<Vec<_>>();

                assert_eq!(actual_shards, expected_shards);

                assert_eq!(
                    tx.inner().get_prune_checkpoint(PrunePart::AccountHistory).unwrap(),
                    Some(PruneCheckpoint {
                        block_number: last_pruned_block as BlockNumber,
                        prune_mode
                    })
                );
            };

        // Prune first time: no previous checkpoint is present
        test_prune(4000, 0..=2999, false);
        // Prune second time: previous checkpoint is present, should continue pruning from where
        // ended last time
        test_prune(4000, 3000..=4000, true);
    }

    #[test]
    fn prune_storage_history() {
        let tx = TestTransaction::default();
        let mut rng = generators::rng();

        let block_num = 7000;
        let blocks = random_block_range(&mut rng, 0..=block_num, H256::zero(), 0..1);
        tx.insert_blocks(blocks.iter(), None).expect("insert blocks");

        let accounts =
            random_eoa_account_range(&mut rng, 0..3).into_iter().collect::<BTreeMap<_, _>>();

        let (changesets, _) = random_changeset_range(
            &mut rng,
            blocks.iter(),
            accounts.into_iter().map(|(addr, acc)| (addr, (acc, Vec::new()))),
            1..2,
            1..2,
        );
        tx.insert_changesets(changesets.clone(), None).expect("insert changesets");
        tx.insert_history(changesets.clone(), None).expect("insert history");

        let storage_occurences = tx.table::<tables::StorageHistory>().unwrap().into_iter().fold(
            BTreeMap::<_, usize>::new(),
            |mut map, (key, _)| {
                map.entry((key.address, key.sharded_key.key)).or_default().add_assign(1);
                map
            },
        );
        assert!(storage_occurences.into_iter().any(|(_, occurrences)| occurrences > 1));

        assert_eq!(
            tx.table::<tables::StorageChangeSet>().unwrap().len(),
            changesets.iter().flatten().flat_map(|(_, _, entries)| entries).count()
        );

        let original_shards = tx.table::<tables::StorageHistory>().unwrap();

        let test_prune = |to_block: BlockNumber,
                          pruned_blocks: RangeInclusive<usize>,
                          is_final_range| {
            let prune_mode = PruneMode::Before(to_block);
            let pruner = Pruner::new(
                tx.inner_raw(),
                MAINNET.clone(),
                5,
                PruneModes { storage_history: Some(prune_mode), ..Default::default() },
                CommitThresholds {
                    // Less than total amount of blocks to prune to test the batching logic
                    storage_history: 3000,
                    ..Default::default()
                },
            );

            let last_pruned_block = *pruned_blocks.end();

            let provider = tx.inner_rw();
            assert_eq!(
                pruner.prune_storage_history(&provider, to_block, prune_mode),
                Ok(Some((
                    changesets[pruned_blocks]
                        .iter()
                        .flatten()
                        .flat_map(|(_, _, entries)| entries)
                        .count(),
                    is_final_range
                )))
            );
            provider.commit().expect("commit");

            assert_eq!(
                tx.table::<tables::StorageChangeSet>().unwrap().len(),
                changesets[last_pruned_block + 1..]
                    .iter()
                    .flatten()
                    .flat_map(|(_, _, entries)| entries)
                    .count()
            );

            let actual_shards = tx.table::<tables::StorageHistory>().unwrap();

            let expected_shards = original_shards
                .iter()
                .filter(|(key, _)| key.sharded_key.highest_block_number > last_pruned_block as u64)
                .map(|(key, blocks)| {
                    let new_blocks = blocks
                        .iter(0)
                        .skip_while(|block| *block <= last_pruned_block)
                        .collect::<Vec<_>>();
                    (key.clone(), BlockNumberList::new_pre_sorted(new_blocks))
                })
                .collect::<Vec<_>>();

            assert_eq!(actual_shards, expected_shards);

            assert_eq!(
                tx.inner().get_prune_checkpoint(PrunePart::StorageHistory).unwrap(),
                Some(PruneCheckpoint {
                    block_number: last_pruned_block as BlockNumber,
                    prune_mode
                })
            );
        };

        // Prune first time: no previous checkpoint is present
        test_prune(4000, 0..=2999, false);
        // Prune second time: previous checkpoint is present, should continue pruning from where
        // ended last time
        test_prune(4000, 3000..=4000, true);
    }
}
