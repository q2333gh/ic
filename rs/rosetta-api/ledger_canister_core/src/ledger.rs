use crate::{archive::ArchiveCanisterWasm, blockchain::Blockchain, range_utils, runtime::Runtime};
use ic_base_types::CanisterId;
use ic_canister_log::{log, Sink};
use ic_ledger_core::approvals::{Approvals, ApproveError, InsufficientAllowance};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::ops::Range;
use std::time::Duration;

use ic_ledger_core::balances::{BalanceError, Balances, BalancesStore, InspectableBalancesStore};
use ic_ledger_core::block::{BlockIndex, BlockType, EncodedBlock, FeeCollector};
use ic_ledger_core::timestamp::TimeStamp;
use ic_ledger_core::tokens::Tokens;
use ic_ledger_hash_of::HashOf;

/// The memo to use for balances burned during trimming
const TRIMMED_MEMO: u64 = u64::MAX;

#[derive(Serialize, Deserialize, Debug)]
pub struct TransactionInfo<TransactionType> {
    pub block_timestamp: TimeStamp,
    pub transaction_hash: HashOf<TransactionType>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum TxApplyError {
    InsufficientFunds { balance: Tokens },
    InsufficientAllowance { allowance: Tokens },
    ExpiredApproval { now: TimeStamp },
    AllowanceChanged { current_allowance: Tokens },
    SelfApproval,
}

impl From<BalanceError> for TxApplyError {
    fn from(e: BalanceError) -> Self {
        match e {
            BalanceError::InsufficientFunds { balance } => Self::InsufficientFunds { balance },
        }
    }
}

impl From<InsufficientAllowance> for TxApplyError {
    fn from(e: InsufficientAllowance) -> Self {
        Self::InsufficientAllowance { allowance: e.0 }
    }
}

impl From<ApproveError> for TxApplyError {
    fn from(ae: ApproveError) -> Self {
        match ae {
            ApproveError::ExpiredApproval { now } => Self::ExpiredApproval { now },
            ApproveError::AllowanceChanged { current_allowance } => {
                Self::AllowanceChanged { current_allowance }
            }
            ApproveError::SelfApproval => Self::SelfApproval,
        }
    }
}

pub trait LedgerContext {
    type AccountId: std::hash::Hash + Ord + Eq + Clone;
    type Approvals: Approvals<AccountId = Self::AccountId>;
    type BalancesStore: BalancesStore<AccountId = Self::AccountId> + Default;

    fn balances(&self) -> &Balances<Self::BalancesStore>;
    fn balances_mut(&mut self) -> &mut Balances<Self::BalancesStore>;

    fn approvals(&self) -> &Self::Approvals;
    fn approvals_mut(&mut self) -> &mut Self::Approvals;

    fn fee_collector(&self) -> Option<&FeeCollector<Self::AccountId>>;
}

pub trait LedgerTransaction: Sized {
    type AccountId: Clone;

    /// Constructs a new "burn" transaction that removes the specified `amount` of tokens from the
    /// `from` account.
    fn burn(
        from: Self::AccountId,
        amount: Tokens,
        at: Option<TimeStamp>,
        memo: Option<u64>,
    ) -> Self;

    /// Returns the time at which the transaction was constructed.
    fn created_at_time(&self) -> Option<TimeStamp>;

    /// Returns the hash of this transaction.
    fn hash(&self) -> HashOf<Self>;

    /// Applies this transaction to the balance book.
    fn apply<C>(
        &self,
        context: &mut C,
        now: TimeStamp,
        effective_fee: Tokens,
    ) -> Result<(), TxApplyError>
    where
        C: LedgerContext<AccountId = Self::AccountId>;
}

pub trait LedgerAccess {
    type Ledger: LedgerData;

    /// Executes a function on a ledger reference.
    ///
    /// # Panics
    ///
    /// Panics if `f` tries to call `with_ledger` or `with_ledger_mut` recursively.
    fn with_ledger<R>(f: impl FnOnce(&Self::Ledger) -> R) -> R;

    /// Executes a function on a mutable ledger reference.
    ///
    /// # Panics
    ///
    /// Panics if `f` tries to call `with_ledger` or `with_ledger_mut` recursively.
    fn with_ledger_mut<R>(f: impl FnOnce(&mut Self::Ledger) -> R) -> R;
}

pub trait LedgerData: LedgerContext {
    type ArchiveWasm: ArchiveCanisterWasm;
    type Runtime: Runtime;
    type Block: BlockType<Transaction = Self::Transaction, AccountId = Self::AccountId>;
    type Transaction: LedgerTransaction<AccountId = Self::AccountId> + Ord + Clone;

    // Purge configuration

    /// How long the ledger needs to remembered transactions to detect duplicates.
    fn transaction_window(&self) -> Duration;

    /// Maximum number of transactions that this ledger will accept
    /// within the [transaction_window].
    fn max_transactions_in_window(&self) -> usize;

    /// The maximum number of transactions that we attempt to purge in one go.
    fn max_transactions_to_purge(&self) -> usize;

    /// The maximum size of the balances map.
    fn max_number_of_accounts(&self) -> usize;

    /// How many accounts with lowest balances to purge when the number of accounts exceeds
    /// [LedgerData::max_number_of_accounts].
    fn accounts_overflow_trim_quantity(&self) -> usize;

    // Token configuration

    /// Token name (e.g., Bitcoin).
    fn token_name(&self) -> &str;

    /// Token symbol (e.g., BTC).
    fn token_symbol(&self) -> &str;

    // Ledger data structures

    fn blockchain(&self) -> &Blockchain<Self::Runtime, Self::ArchiveWasm>;
    fn blockchain_mut(&mut self) -> &mut Blockchain<Self::Runtime, Self::ArchiveWasm>;

    fn transactions_by_hash(&self) -> &BTreeMap<HashOf<Self::Transaction>, BlockIndex>;
    fn transactions_by_hash_mut(&mut self) -> &mut BTreeMap<HashOf<Self::Transaction>, BlockIndex>;

    fn transactions_by_height(&self) -> &VecDeque<TransactionInfo<Self::Transaction>>;
    fn transactions_by_height_mut(&mut self) -> &mut VecDeque<TransactionInfo<Self::Transaction>>;

    /// The callback that the ledger framework calls when it purges a transaction.
    fn on_purged_transaction(&mut self, height: BlockIndex);

    fn fee_collector_mut(&mut self) -> Option<&mut FeeCollector<Self::AccountId>>;
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum TransferError {
    BadFee { expected_fee: Tokens },
    InsufficientFunds { balance: Tokens },
    InsufficientAllowance { allowance: Tokens },
    ExpiredApproval { ledger_time: TimeStamp },
    TxTooOld { allowed_window_nanos: u64 },
    TxCreatedInFuture { ledger_time: TimeStamp },
    TxThrottled,
    TxDuplicate { duplicate_of: BlockIndex },
    AllowanceChanged { current_allowance: Tokens },
    SelfApproval,
}

/// Adds a new block with the specified transaction to the ledger.
pub fn apply_transaction<L>(
    ledger: &mut L,
    transaction: L::Transaction,
    now: TimeStamp,
    effective_fee: Tokens,
) -> Result<(BlockIndex, HashOf<EncodedBlock>), TransferError>
where
    L: LedgerData,
    L::BalancesStore: InspectableBalancesStore,
{
    let num_pruned = purge_old_transactions(ledger, now);

    // If we pruned some transactions, let this one through
    // otherwise throttle if there are too many
    if num_pruned == 0 && throttle(ledger, now) {
        return Err(TransferError::TxThrottled);
    }

    let maybe_time_and_hash = transaction
        .created_at_time()
        .map(|created_at_time| (created_at_time, transaction.hash()));

    if let Some((created_at_time, tx_hash)) = maybe_time_and_hash {
        // The caller requested deduplication.
        if created_at_time + ledger.transaction_window() < now {
            return Err(TransferError::TxTooOld {
                allowed_window_nanos: ledger.transaction_window().as_nanos() as u64,
            });
        }

        if created_at_time > now + ic_constants::PERMITTED_DRIFT {
            return Err(TransferError::TxCreatedInFuture { ledger_time: now });
        }

        if let Some(block_height) = ledger.transactions_by_hash().get(&tx_hash) {
            return Err(TransferError::TxDuplicate {
                duplicate_of: *block_height,
            });
        }
    }

    transaction
        .apply(ledger, now, effective_fee)
        .map_err(|e| match e {
            TxApplyError::InsufficientFunds { balance } => {
                TransferError::InsufficientFunds { balance }
            }
            TxApplyError::InsufficientAllowance { allowance } => {
                TransferError::InsufficientAllowance { allowance }
            }
            TxApplyError::ExpiredApproval { now } => {
                TransferError::ExpiredApproval { ledger_time: now }
            }
            TxApplyError::AllowanceChanged { current_allowance } => {
                TransferError::AllowanceChanged { current_allowance }
            }
            TxApplyError::SelfApproval => TransferError::SelfApproval,
        })?;

    let fee_collector = ledger.fee_collector().cloned();
    let block = L::Block::from_transaction(
        ledger.blockchain().last_hash,
        transaction,
        now,
        effective_fee,
        fee_collector,
    );
    let block_timestamp = block.timestamp();

    let height = ledger
        .blockchain_mut()
        .add_block(block)
        .expect("failed to add block");
    if let Some(fee_collector) = ledger.fee_collector_mut().as_mut() {
        if fee_collector.block_index.is_none() {
            fee_collector.block_index = Some(height);
        }
    }

    if let Some((_, tx_hash)) = maybe_time_and_hash {
        // The caller requested deduplication, so we have to remember this
        // transaction within the dedup window.
        ledger.transactions_by_hash_mut().insert(tx_hash, height);

        ledger
            .transactions_by_height_mut()
            .push_back(TransactionInfo {
                block_timestamp,
                transaction_hash: tx_hash,
            });
    }

    let to_trim = if ledger.balances().store.len()
        >= ledger.max_number_of_accounts() + ledger.accounts_overflow_trim_quantity()
    {
        select_accounts_to_trim(ledger)
    } else {
        vec![]
    };

    for (balance, account) in to_trim {
        let burn_tx = L::Transaction::burn(account, balance, Some(now), Some(TRIMMED_MEMO));

        burn_tx
            .apply(ledger, now, Tokens::ZERO)
            .expect("failed to burn funds that must have existed");

        let parent_hash = ledger.blockchain().last_hash;
        let fee_collector = ledger.fee_collector().cloned();

        ledger
            .blockchain_mut()
            .add_block(L::Block::from_transaction(
                parent_hash,
                burn_tx,
                now,
                Tokens::from_e8s(0),
                fee_collector,
            ))
            .unwrap();
    }

    Ok((height, ledger.blockchain().last_hash.unwrap()))
}

/// Finds the archive canister that contains the block with the specified height.
pub fn find_block_in_archive<L: LedgerData>(ledger: &L, block_height: u64) -> Option<CanisterId> {
    let index = ledger
        .blockchain()
        .archive
        .try_read()
        .expect("Failed to get lock on archive")
        .as_ref()
        .expect("archiving not enabled")
        .index();

    let result = index.binary_search_by(|((from, to), _)| {
        // If within the range we've found the right node
        if *from <= block_height && block_height <= *to {
            std::cmp::Ordering::Equal
        } else if *from < block_height {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Greater
        }
    });

    match result {
        Ok(i) => Some(index[i].1),
        Err(_) => None,
    }
}

/// Returns true if the next transaction should be throttled due to high
/// load on the ledger.
fn throttle<L: LedgerData>(ledger: &L, now: TimeStamp) -> bool {
    let num_in_window = ledger.transactions_by_height().len();
    // We admit the first half of max_transactions_in_window freely.
    // After that we start throttling on per-second basis.
    // This way we guarantee that at most max_transactions_in_window will
    // get through within the transaction window.
    if num_in_window >= ledger.max_transactions_in_window() / 2 {
        // max num of transactions allowed per second
        let max_rate = (0.5 * ledger.max_transactions_in_window() as f64
            / ledger.transaction_window().as_secs_f64())
        .ceil() as usize;

        if ledger
            .transactions_by_height()
            .get(num_in_window.saturating_sub(max_rate))
            .map(|tx| tx.block_timestamp)
            .unwrap_or_else(|| TimeStamp::from_nanos_since_unix_epoch(0))
            + Duration::from_secs(1)
            > now
        {
            return true;
        }
    }
    false
}

/// Removes at most [LedgerData::max_transactions_to_purge] transactions older
/// than `now - Ledger::transaction_window` and returns the number of purged
/// transactions.
pub fn purge_old_transactions<L: LedgerData>(ledger: &mut L, now: TimeStamp) -> usize {
    let max_tx_to_purge = ledger.max_transactions_to_purge();
    let mut num_tx_purged = 0usize;

    while let Some(tx_info) = ledger.transactions_by_height().front() {
        if tx_info.block_timestamp + ledger.transaction_window() + ic_constants::PERMITTED_DRIFT
            >= now
        {
            // Stop at a sufficiently recent block.
            break;
        }

        let transaction_hash = tx_info.transaction_hash;

        match ledger.transactions_by_hash_mut().remove(&transaction_hash) {
            None => unreachable!(
                concat!(
                    "invariant violation: transaction with hash {} ",
                    "is in transaction_by_height but not in transactions_by_hash"
                ),
                transaction_hash
            ),
            Some(block_height) => ledger.on_purged_transaction(block_height),
        }

        ledger.transactions_by_height_mut().pop_front();

        num_tx_purged += 1;
        if num_tx_purged >= max_tx_to_purge {
            break;
        }
    }
    num_tx_purged
}

// Find the specified number of accounts with lowest balances so that their
// balances can be reclaimed.
fn select_accounts_to_trim<L>(ledger: &L) -> Vec<(Tokens, L::AccountId)>
where
    L: LedgerData,
    L::BalancesStore: InspectableBalancesStore,
{
    let mut to_trim: std::collections::BinaryHeap<(Tokens, L::AccountId)> =
        std::collections::BinaryHeap::new();

    let num_accounts = ledger.accounts_overflow_trim_quantity();
    let mut iter = ledger.balances().store.iter();

    // Accumulate up to `trim_quantity` accounts
    for (account, balance) in iter.by_ref().take(num_accounts) {
        to_trim.push((*balance, account.clone()));
    }

    for (account, balance) in iter {
        // If any account's balance is lower than the maximum in our set,
        // include that account, and remove the current maximum
        if let Some((greatest_balance, _)) = to_trim.peek() {
            if balance < greatest_balance {
                to_trim.push((*balance, account.clone()));
                to_trim.pop();
            }
        }
    }

    to_trim.into_vec()
}

/// Asynchronously archives a suffix of the locally available blockchain.
///
/// NOTE: only one archiving task can run at each point in time.
/// If archiving is already in process, this function returns immediately.
pub async fn archive_blocks<LA: LedgerAccess>(sink: impl Sink + Clone, max_message_size: u64) {
    use crate::archive::{
        send_blocks_to_archive, ArchivingGuard, ArchivingGuardError, FailedToArchiveBlocks,
    };
    use std::sync::Arc;

    let archive_arc = LA::with_ledger(|ledger| ledger.blockchain().archive.clone());

    // NOTE: this guard will prevent another logical thread to start the archiving process.
    let _archiving_guard = match ArchivingGuard::new(Arc::clone(&archive_arc)) {
        Ok(guard) => guard,
        Err(ArchivingGuardError::NoArchive) => {
            return; // Archiving not enabled
        }
        Err(ArchivingGuardError::AlreadyArchiving) => {
            return; // Ledger is currently archiving, skipping archive_blocks.
        }
    };

    let blocks_to_archive = LA::with_ledger(|ledger| {
        let archive_guard = ledger.blockchain().archive.read().unwrap();
        let archive = archive_guard.as_ref().unwrap();
        ledger
            .blockchain()
            .get_blocks_for_archiving(archive.trigger_threshold, archive.num_blocks_to_archive)
    });

    if blocks_to_archive.is_empty() {
        return;
    }

    let num_blocks = blocks_to_archive.len();
    log!(sink, "[ledger] archiving {} blocks", num_blocks);

    let result = send_blocks_to_archive(
        sink.clone(),
        archive_arc,
        blocks_to_archive,
        max_message_size,
    )
    .await;

    LA::with_ledger_mut(|ledger| match result {
        Ok(num_sent_blocks) => ledger
            .blockchain_mut()
            .remove_archived_blocks(num_sent_blocks),
        Err((num_sent_blocks, FailedToArchiveBlocks(err))) => {
            ledger
                .blockchain_mut()
                .remove_archived_blocks(num_sent_blocks);
            log!(
                sink,
                "[ledger] archived only {} out of {} blocks; error: {}",
                num_sent_blocks,
                num_blocks,
                err
            );
        }
    });
}

/// The distribution of a block range across canisters.
pub struct BlockLocations {
    /// Blocks currently owned by the main ledger canister.
    pub local_blocks: Range<u64>,
    /// Blocks stored in the archive canisters.
    pub archived_blocks: Vec<(CanisterId, Range<u64>)>,
}

/// Returns the locations of the specified block range.
pub fn block_locations<L: LedgerData>(ledger: &L, start: u64, length: usize) -> BlockLocations {
    let requested_range = range_utils::make_range(start, length);
    let local_range = ledger.blockchain().local_block_range();
    let local_blocks = range_utils::intersect(&requested_range, &local_range)
        .unwrap_or_else(|_| range_utils::make_range(local_range.start, 0));

    let archive = ledger.blockchain().archive.read().unwrap();

    let archived_blocks: Vec<_> = archive
        .iter()
        .flat_map(|archive| archive.index().into_iter())
        .filter_map(|((from, to), canister_id)| {
            let slice = range_utils::intersect(&(from..to + 1), &requested_range).ok()?;
            (!slice.is_empty()).then_some((canister_id, slice))
        })
        .collect();

    BlockLocations {
        local_blocks,
        archived_blocks,
    }
}
