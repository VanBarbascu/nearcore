use crate::{
    ChainAccess, ChainError, LatestTargetNonce, MappedBlock, MappedChunk, MappedTx,
    MappedTxProvenance, NonceKind, NonceLookupKey, NonceUpdater, TargetChainTx, TargetNonce,
    TxBatch, TxRef,
};
use anyhow::Context;
use near_async::multithread::MultithreadRuntimeHandle;
use near_client::ViewClientActor;
use near_crypto::{PublicKey, SecretKey};
use near_indexer::StreamerMessage;
use near_indexer_primitives::{IndexerExecutionOutcomeWithReceipt, IndexerTransactionWithOutcome};
use near_primitives::hash::CryptoHash;
use near_primitives::transaction::Transaction;
use near_primitives::types::{AccountId, Balance, BlockHeight};
use near_primitives::views::{ActionView, ExecutionStatusView, ReceiptEnumView};
use near_primitives_core::types::{Nonce, NonceIndex, ShardId};
use parking_lot::Mutex;
use rocksdb::DB;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::collections::hash_map;
use std::collections::{BTreeSet, HashSet, VecDeque};
use std::fmt::Write;
use std::time::{Duration, Instant};

// Information related to a single transaction that we sent in the past.
// We could just forget it and not save any of this, but keeping this info
// makes it easy to print out human-friendly info later on when we find this
// transaction on chain.
struct TxSendInfo {
    sent_at: Instant,
    source_height: Option<BlockHeight>,
    provenance: MappedTxProvenance,
    source_signer_id: AccountId,
    source_receiver_id: AccountId,
    target_signer_id: Option<AccountId>,
    target_receiver_id: Option<AccountId>,
    actions: Vec<String>,
    sent_at_target_height: BlockHeight,
}

impl TxSendInfo {
    fn new(
        tx: &MappedTx,
        source_height: Option<BlockHeight>,
        target_height: BlockHeight,
        now: Instant,
    ) -> Self {
        let target_signer_id = if &tx.source_signer_id != tx.target_tx.transaction.signer_id() {
            Some(tx.target_tx.transaction.signer_id().clone())
        } else {
            None
        };
        let target_receiver_id = if &tx.source_receiver_id != tx.target_tx.transaction.receiver_id()
        {
            Some(tx.target_tx.transaction.receiver_id().clone())
        } else {
            None
        };
        Self {
            source_height,
            provenance: tx.provenance,
            source_signer_id: tx.source_signer_id.clone(),
            source_receiver_id: tx.source_receiver_id.clone(),
            target_signer_id,
            target_receiver_id,
            sent_at: now,
            sent_at_target_height: target_height,
            actions: tx
                .target_tx
                .transaction
                .actions()
                .iter()
                .map(|a| a.as_ref().to_string())
                .collect::<Vec<_>>(),
        }
    }
}

#[derive(PartialEq, Eq, Debug)]
struct TxId {
    hash: CryptoHash,
    nonce: Nonce,
}

impl PartialOrd for TxId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TxId {
    fn cmp(&self, other: &Self) -> Ordering {
        self.nonce.cmp(&other.nonce).then_with(|| self.hash.cmp(&other.hash))
    }
}

#[derive(Clone, Debug)]
struct NonceInfo {
    target_nonce: TargetNonce,
    // the last height we have queued that references this access key. After
    // we send the txs at that height, we'll delete this from memory so that
    // the amount of memory we're using for these doesn't keep growing as we run for a while
    last_height: Option<BlockHeight>,
    txs_awaiting_nonce: BTreeSet<TxRef>,
    queued_txs: BTreeSet<TxRef>,
}

pub(crate) enum SentBatch {
    MappedBlock(TxBatch),
    ExtraTxs(Vec<TargetChainTx>),
}

// a nonce lookup key along with the id of the tx or receipt that might have updated it
pub(crate) struct UpdatedKey {
    pub(crate) nonce_key: NonceLookupKey,
    pub(crate) id: CryptoHash,
}

// return value of on_target_block()
pub(crate) struct TargetBlockInfo {
    // these accounts need to be unstaked
    pub(crate) staked_accounts: HashMap<(AccountId, PublicKey), AccountId>,
    // these access keys that were previously unavailable may now be available
    pub(crate) access_key_updates: Vec<UpdatedKey>,
}
// Keeps the queue of upcoming transactions and provides them in regular intervals via next_batch()
// Also keeps track of txs we've sent so far and looks for them on chain, for metrics/logging purposes.

// TODO: the separation between what's in here and what's in the main file with struct TxMirror is not
// that clear and doesn't make that much sense. Should refactor
pub(crate) struct TxTracker {
    sent_txs: HashMap<CryptoHash, TxSendInfo>,
    txs_by_signer: HashMap<NonceLookupKey, BTreeSet<TxId>>,
    // for each updater (a tx or receipt hash, or a queued transaction we haven't sent yet), keeps
    // a set of nonce lookup keys who might be updated by it
    updater_to_keys: HashMap<NonceUpdater, HashSet<NonceLookupKey>>,
    nonces: HashMap<NonceLookupKey, NonceInfo>,
    next_heights: VecDeque<BlockHeight>,
    height_queued: Option<BlockHeight>,
    // the reason we have these (nonempty_height_queued, height_seen, etc) is so that we can
    // exit after we receive the target block containing the txs we sent for the last source block.
    // It's a minor thing, but otherwise if we just exit after sending the last source block's txs,
    // we won't get to see the resulting txs on chain in the debug logs from log_target_block()
    nonempty_height_queued: Option<BlockHeight>,
    height_popped: Option<BlockHeight>,
    height_seen: Option<BlockHeight>,
    // Config value in the target chain, used to judge how long to wait before sending a new batch of txs
    min_block_production_delay: Duration,
    // optional specific tx send delay
    tx_batch_interval: Option<Duration>,
    // timestamps in the target chain, used to judge how long to wait before sending a new batch of txs
    recent_block_timestamps: VecDeque<u64>,
    // last source block we'll be sending transactions for
    stop_height: Option<BlockHeight>,
    // tracks how many times each source_height has been re-queued
    requeue_attempts: HashMap<BlockHeight, usize>,
}

impl TxTracker {
    // `next_heights` should show the next several valid heights in the chain, starting from
    // the first block we want to send txs for. Right now we are assuming this arg is not empty when
    // we unwrap() self.height_queued() in Self::next_heights()
    pub(crate) fn new<'a, I>(
        min_block_production_delay: Duration,
        tx_batch_interval: Option<Duration>,
        next_heights: I,
        stop_height: Option<BlockHeight>,
    ) -> Self
    where
        I: IntoIterator<Item = &'a BlockHeight>,
    {
        let next_heights = next_heights.into_iter().map(Clone::clone).collect();
        Self {
            min_block_production_delay,
            next_heights,
            stop_height,
            tx_batch_interval,
            sent_txs: HashMap::new(),
            txs_by_signer: HashMap::new(),
            updater_to_keys: HashMap::new(),
            nonces: HashMap::new(),
            height_queued: None,
            nonempty_height_queued: None,
            height_popped: None,
            height_seen: None,
            recent_block_timestamps: VecDeque::new(),
            requeue_attempts: HashMap::new(),
        }
    }

    pub(crate) async fn next_heights<T: ChainAccess>(
        me: &Mutex<Self>,
        source_chain: &T,
    ) -> anyhow::Result<(Option<BlockHeight>, Option<BlockHeight>)> {
        let (mut next_heights, height_queued) = {
            let t = me.lock();
            (t.next_heights.clone(), t.height_queued)
        };
        while next_heights.len() <= crate::CREATE_ACCOUNT_DELTA {
            // we unwrap() the height_queued because Self::new() should have been called with
            // nonempty next_heights.
            let h =
                next_heights.iter().next_back().cloned().unwrap_or_else(|| height_queued.unwrap());
            match source_chain.get_next_block_height(h).await {
                Ok(h) => next_heights.push_back(h),
                Err(ChainError::Unknown) => break,
                Err(ChainError::Other(e)) => {
                    return Err(e)
                        .with_context(|| format!("failed fetching next height after {}", h));
                }
            };
        }
        let mut t = me.lock();
        t.next_heights = next_heights;
        let next_height = t.next_heights.get(0).cloned();
        let create_account_height = t.next_heights.get(crate::CREATE_ACCOUNT_DELTA).cloned();
        Ok((next_height, create_account_height))
    }

    pub(crate) fn finished(&self) -> bool {
        match self.stop_height {
            Some(_) => {
                self.height_popped >= self.stop_height
                    && self.height_seen >= self.nonempty_height_queued
            }
            None => false,
        }
    }

    // Makes sure that there's something written in the DB for this access key.
    // This function is called before calling initialize_target_nonce(), which sets
    // in-memory data associated with this nonce. It would make sense to do this part at the same time,
    // But since we can't hold the lock across awaits, we need to do this separately first if we want to
    // keep the lock on Self for the entirety of sections of code that make updates to it.
    //
    // So this function must be called before calling initialize_target_nonce() for a given `NonceLookupKey`
    async fn store_target_nonce(
        target_view_client: &MultithreadRuntimeHandle<ViewClientActor>,
        db: &DB,
        nonce_key: &NonceLookupKey,
    ) -> anyhow::Result<()> {
        if crate::read_target_nonce(db, nonce_key)?.is_some() {
            return Ok(());
        }
        match &nonce_key.kind {
            NonceKind::AccessKey => {
                let nonce = crate::fetch_access_key_nonce(
                    target_view_client,
                    &nonce_key.account_id,
                    &nonce_key.public_key,
                )
                .await?;
                let t = LatestTargetNonce { nonce, pending_outcomes: HashSet::new() };
                crate::put_target_nonce(db, nonce_key, &t)?;
            }
            NonceKind::GasKey(_) => {
                // Bulk fetch all gas key nonces at once and write them all to DB.
                // Subsequent calls for other indices of the same key will find
                // their entry in DB and skip the RPC.
                let nonces = crate::fetch_gas_key_nonces(
                    target_view_client,
                    &nonce_key.account_id,
                    &nonce_key.public_key,
                )
                .await?;
                if let Some(nonces) = nonces {
                    for (i, nonce) in nonces.iter().enumerate() {
                        let key = NonceLookupKey {
                            account_id: nonce_key.account_id.clone(),
                            public_key: nonce_key.public_key.clone(),
                            kind: NonceKind::GasKey(i as NonceIndex),
                        };
                        let t = LatestTargetNonce {
                            nonce: Some(*nonce),
                            pending_outcomes: HashSet::new(),
                        };
                        crate::put_target_nonce(db, &key, &t)?;
                    }
                } else {
                    // Gas key doesn't exist on target chain yet
                    let t = LatestTargetNonce { nonce: None, pending_outcomes: HashSet::new() };
                    crate::put_target_nonce(db, nonce_key, &t)?;
                }
            }
        }
        Ok(())
    }

    fn initialize_target_nonce(
        &mut self,
        db: &DB,
        nonce_key: &NonceLookupKey,
        source_height: Option<BlockHeight>,
    ) -> anyhow::Result<()> {
        // We unwrap() because store_target_nonce() must be called before calling this function.
        let t = crate::read_target_nonce(db, nonce_key)?.unwrap();
        let info = NonceInfo {
            target_nonce: TargetNonce {
                nonce: t.nonce,
                pending_outcomes: t
                    .pending_outcomes
                    .into_iter()
                    .map(NonceUpdater::ChainObjectId)
                    .collect(),
            },
            last_height: source_height,
            txs_awaiting_nonce: BTreeSet::new(),
            queued_txs: BTreeSet::new(),
        };
        self.nonces.insert(nonce_key.clone(), info);
        Ok(())
    }

    fn get_target_nonce<'a>(
        &'a mut self,
        db: &DB,
        nonce_key: &NonceLookupKey,
        source_height: Option<BlockHeight>,
    ) -> anyhow::Result<&'a mut NonceInfo> {
        if !self.nonces.contains_key(nonce_key) {
            self.initialize_target_nonce(db, nonce_key, source_height)?;
        }
        Ok(self.nonces.get_mut(nonce_key).unwrap())
    }

    pub(crate) async fn next_nonce(
        lock: &Mutex<Self>,
        target_view_client: &MultithreadRuntimeHandle<ViewClientActor>,
        db: &DB,
        nonce_key: &NonceLookupKey,
        source_height: BlockHeight,
    ) -> anyhow::Result<TargetNonce> {
        let source_height = Some(source_height);
        Self::store_target_nonce(target_view_client, db, nonce_key).await?;
        let mut me = lock.lock();
        let info = me.get_target_nonce(db, nonce_key, source_height).unwrap();
        if source_height > info.last_height {
            info.last_height = source_height;
        }
        if let Some(nonce) = &mut info.target_nonce.nonce {
            *nonce += 1;
        }
        Ok(info.target_nonce.clone())
    }

    // normally when we're adding txs, we're adding a tx that
    // wants to be sent after all the previous ones for that signer that
    // we've already prepared. So next_nonce() returns the biggest nonce
    // we've used so far + 1. But if we want to add a tx at the beginning,
    // we need to shift all the bigger nonces by one.
    pub(crate) async fn insert_nonce(
        lock: &Mutex<Self>,
        tx_block_queue: &Mutex<VecDeque<MappedBlock>>,
        target_view_client: &MultithreadRuntimeHandle<ViewClientActor>,
        db: &DB,
        nonce_key: &NonceLookupKey,
        secret_key: &SecretKey,
    ) -> anyhow::Result<TargetNonce> {
        Self::store_target_nonce(target_view_client, db, nonce_key).await?;
        let mut me = lock.lock();
        if !me.nonces.contains_key(nonce_key) {
            me.initialize_target_nonce(db, nonce_key, None)?;
            let info = me.nonces.get_mut(nonce_key).unwrap();
            if let Some(nonce) = &mut info.target_nonce.nonce {
                *nonce += 1;
            }
            return Ok(info.target_nonce.clone());
        }
        let mut first_nonce = None;
        let txs = me.nonces.get(nonce_key).unwrap().queued_txs.clone();
        if !txs.is_empty() {
            let mut tx_block_queue = tx_block_queue.lock();
            for tx_ref in txs {
                let tx = Self::get_tx(&mut tx_block_queue, &tx_ref);
                if first_nonce.is_none() {
                    first_nonce = Some(tx.target_nonce());
                }
                tx.inc_target_nonce(secret_key)
            }
        }
        match first_nonce {
            Some(n) => {
                if let Some(nonce) = &mut me.nonces.get_mut(nonce_key).unwrap().target_nonce.nonce {
                    *nonce += 1;
                }
                Ok(n)
            }
            None => {
                tracing::warn!(target: "mirror", ?nonce_key, "info for nonce key was cached but there are no upcoming transactions queued for it");
                Ok(TargetNonce::default())
            }
        }
    }

    fn get_tx<'a>(
        tx_block_queue: &'a mut VecDeque<MappedBlock>,
        tx_ref: &TxRef,
    ) -> &'a mut TargetChainTx {
        let block_idx = tx_block_queue
            .binary_search_by(|b| b.source_height.cmp(&tx_ref.source_height))
            .unwrap();
        let block = &mut tx_block_queue[block_idx];
        let chunk = block.chunks.iter_mut().find(|c| c.shard_id == tx_ref.shard_id).unwrap();
        &mut chunk.txs[tx_ref.tx_idx]
    }

    // This function sets in-memory info for any access keys that will be touched by this transaction (`tx_ref`).
    // store_target_nonce() must have been called beforehand for each of these.
    fn insert_access_key_updates(
        &mut self,
        db: &DB,
        tx_ref: &TxRef,
        nonce_updates: &HashSet<NonceLookupKey>,
        source_height: BlockHeight,
    ) -> anyhow::Result<()> {
        let source_height = Some(source_height);
        for nonce_key in nonce_updates {
            let info = self.get_target_nonce(db, nonce_key, source_height).unwrap();

            if info.last_height < source_height {
                info.last_height = source_height;
            }
            info.target_nonce.pending_outcomes.insert(NonceUpdater::TxRef(tx_ref.clone()));
        }
        if !nonce_updates.is_empty() {
            assert!(
                self.updater_to_keys
                    .insert(NonceUpdater::TxRef(tx_ref.clone()), nonce_updates.clone())
                    .is_none()
            );
        }
        Ok(())
    }

    async fn store_access_key_updates(
        block: &MappedBlock,
        target_view_client: &MultithreadRuntimeHandle<ViewClientActor>,
        db: &DB,
    ) -> anyhow::Result<()> {
        for c in &block.chunks {
            for tx in &c.txs {
                let updates = match tx {
                    crate::TargetChainTx::Ready(tx) => &tx.nonce_updates,
                    crate::TargetChainTx::AwaitingNonce(tx) => &tx.nonce_updates,
                };
                for nonce_key in updates {
                    Self::store_target_nonce(target_view_client, db, nonce_key).await?;
                }
            }
        }
        Ok(())
    }

    fn queue_txs(&mut self, block: &MappedBlock, db: &DB) -> anyhow::Result<()> {
        self.height_queued = Some(block.source_height);
        self.next_heights.pop_front().unwrap();

        for c in &block.chunks {
            if !c.txs.is_empty() {
                self.nonempty_height_queued = Some(block.source_height);
            }
            for (tx_idx, tx) in c.txs.iter().enumerate() {
                let tx_ref =
                    TxRef { source_height: block.source_height, shard_id: c.shard_id, tx_idx };
                match tx {
                    crate::TargetChainTx::Ready(tx) => {
                        let nonce_key = NonceLookupKey::from_tx(&tx.target_tx.transaction);
                        let info = self.nonces.get_mut(&nonce_key).unwrap();
                        info.queued_txs.insert(tx_ref.clone());
                        self.insert_access_key_updates(
                            db,
                            &tx_ref,
                            &tx.nonce_updates,
                            block.source_height,
                        )?;
                    }
                    crate::TargetChainTx::AwaitingNonce(tx) => {
                        let nonce_key = NonceLookupKey::from_tx(&tx.target_tx);
                        let info = self.nonces.get_mut(&nonce_key).unwrap();
                        info.txs_awaiting_nonce.insert(tx_ref.clone());
                        info.queued_txs.insert(tx_ref.clone());
                        self.insert_access_key_updates(
                            db,
                            &tx_ref,
                            &tx.nonce_updates,
                            block.source_height,
                        )?;
                    }
                };
            }
        }
        Ok(())
    }

    pub(crate) async fn queue_block(
        lock: &Mutex<Self>,
        tx_block_queue: &Mutex<VecDeque<MappedBlock>>,
        block: MappedBlock,
        target_view_client: &MultithreadRuntimeHandle<ViewClientActor>,
        db: &DB,
    ) -> anyhow::Result<()> {
        Self::store_access_key_updates(&block, target_view_client, db).await?;
        let mut me = lock.lock();
        me.queue_txs(&block, db)?;
        tx_block_queue.lock().push_back(block);
        Ok(())
    }

    fn remove_tx(&mut self, tx: &IndexerTransactionWithOutcome) {
        let nonce_key = NonceLookupKey::from_tx_view(&tx.transaction);
        match self.txs_by_signer.entry(nonce_key.clone()) {
            hash_map::Entry::Occupied(mut e) => {
                let txs = e.get_mut();
                if !txs.remove(&TxId { hash: tx.transaction.hash, nonce: tx.transaction.nonce }) {
                    tracing::warn!(target: "mirror", tx_hash = %tx.transaction.hash, "tried to remove nonexistent tx from txs_by_signer");
                }
                // split off from hash: default() since that's the smallest hash, which will leave us with every tx with nonce
                // greater than this one in txs_left.
                let txs_left = txs.split_off(&TxId {
                    hash: CryptoHash::default(),
                    nonce: tx.transaction.nonce + 1,
                });
                if !txs.is_empty() {
                    tracing::debug!(
                        target: "mirror",
                        tx_signer_id = %tx.transaction.signer_id,
                        tx_public_key = ?tx.transaction.public_key,
                        tx_nonce = tx.transaction.nonce,
                        skip_txs = ?txs,
                        "skip txs by tx inclusion",
                    );
                    for t in txs.iter() {
                        if self.sent_txs.remove(&t.hash).is_none() {
                            tracing::warn!(
                                target: "mirror",
                                tx_hash = %t.hash,
                                "tx with hash that we thought was skipped is not in the set of sent txs",
                            );
                        }
                    }
                }
                *txs = txs_left;
                if txs.is_empty() {
                    self.txs_by_signer.remove(&nonce_key);
                }
            }
            hash_map::Entry::Vacant(_) => {
                tracing::warn!(
                    target: "mirror",
                    tx_hash = ?tx.transaction.hash,
                    ?nonce_key,
                    "recently removed tx, but nonce key not in txs_by_signer"
                );
                return;
            }
        };
    }

    fn record_block_timestamp(&mut self, msg: &StreamerMessage) {
        self.recent_block_timestamps.push_back(msg.block.header.timestamp_nanosec);
        if self.recent_block_timestamps.len() > 10 {
            self.recent_block_timestamps.pop_front();
        }
    }

    fn log_target_block(&self, msg: &StreamerMessage) {
        // don't do any work here if we're definitely not gonna log it
        if tracing::level_filters::LevelFilter::current()
            > tracing::level_filters::LevelFilter::DEBUG
        {
            return;
        }

        // right now we're just logging this, but it would be nice to collect/index this
        // and have some HTTP debug page where you can see how close the target chain is
        // to the source chain
        let mut log_message = String::new();
        let now = Instant::now();

        for s in &msg.shards {
            let mut other_txs = 0;
            if let Some(c) = &s.chunk {
                if c.header.height_included == msg.block.header.height {
                    write!(
                        log_message,
                        "-------- shard {} gas used: {} ---------\n",
                        s.shard_id, c.header.gas_used
                    )
                    .unwrap();
                    for tx in &c.transactions {
                        if let Some(info) = self.sent_txs.get(&tx.transaction.hash) {
                            write!(
                            log_message,
                            "{} signer: \"{}\"{} receiver: \"{}\"{} actions: <{}> sent {:?} ago @ target #{}\n",
                            info.provenance,
                            info.source_signer_id,
                            info.target_signer_id.as_ref().map_or(String::new(), |s| format!(" (mapped to \"{}\")", s)),
                            info.source_receiver_id,
                            info.target_receiver_id.as_ref().map_or(String::new(), |s| format!(" (mapped to \"{}\")", s)),
                            info.actions.join(", "),
                            now - info.sent_at,
                            info.sent_at_target_height,
                        ).unwrap();
                        } else {
                            other_txs += 1;
                        }
                    }
                } else {
                    write!(
                        log_message,
                        "-------- shard {} old chunk (#{}) ---------\n",
                        s.shard_id, c.header.height_included
                    )
                    .unwrap();
                }
            } else {
                write!(log_message, "-------- shard {} chunk missing ---------\n", s.shard_id)
                    .unwrap();
            }
            if other_txs > 0 {
                write!(log_message, "    ...    \n").unwrap();
                write!(
                    log_message,
                    "{} other txs (not ours, or sent before a restart)\n",
                    other_txs
                )
                .unwrap();
                write!(log_message, "    ...    \n").unwrap();
            }
        }
        tracing::debug!(target: "mirror", height = %msg.block.header.height, %log_message, "received target block");
    }

    fn tx_to_receipt(
        &mut self,
        tx_block_queue: &Mutex<VecDeque<MappedBlock>>,
        db: &DB,
        tx_hash: &CryptoHash,
        receipt_id: &CryptoHash,
        nonce_keys: HashSet<NonceLookupKey>,
    ) -> anyhow::Result<()> {
        crate::delete_pending_outcome(db, tx_hash)?;

        let updater = NonceUpdater::ChainObjectId(*tx_hash);
        if let Some(keys) = self.updater_to_keys.remove(&updater) {
            assert!(nonce_keys == keys);
        }

        let new_updater = NonceUpdater::ChainObjectId(*receipt_id);

        for nonce_key in &nonce_keys {
            let mut n = crate::read_target_nonce(db, nonce_key)?.unwrap();
            assert!(n.pending_outcomes.remove(tx_hash));
            n.pending_outcomes.insert(*receipt_id);
            crate::put_target_nonce(db, nonce_key, &n)?;

            if let Some(info) = self.nonces.get(nonce_key) {
                let txs_awaiting_nonce = info.txs_awaiting_nonce.clone();

                if !txs_awaiting_nonce.is_empty() {
                    let mut tx_block_queue = tx_block_queue.lock();
                    for r in &txs_awaiting_nonce {
                        let tx = Self::get_tx(&mut tx_block_queue, r);

                        match tx {
                            TargetChainTx::AwaitingNonce(t) => {
                                assert!(t.target_nonce.pending_outcomes.remove(&updater));
                                t.target_nonce.pending_outcomes.insert(new_updater.clone());
                            }
                            TargetChainTx::Ready(_) => unreachable!(),
                        };
                    }
                }

                let info = self.nonces.get_mut(nonce_key).unwrap();
                assert!(info.target_nonce.pending_outcomes.remove(&updater));
                info.target_nonce.pending_outcomes.insert(new_updater.clone());
            }
        }
        crate::put_pending_outcome(db, *receipt_id, nonce_keys.clone())?;
        if !nonce_keys.is_empty() {
            self.updater_to_keys.insert(new_updater, nonce_keys);
        }
        Ok(())
    }

    pub(crate) fn try_set_nonces(
        &mut self,
        tx_block_queue: &Mutex<VecDeque<MappedBlock>>,
        db: &DB,
        updated_key: UpdatedKey,
        nonce: Option<Nonce>,
    ) -> anyhow::Result<()> {
        let mut n = crate::read_target_nonce(db, &updated_key.nonce_key)?.unwrap();
        n.pending_outcomes.remove(&updated_key.id);
        n.nonce = std::cmp::max(n.nonce, nonce);

        crate::put_target_nonce(db, &updated_key.nonce_key, &n)?;

        let updater = NonceUpdater::ChainObjectId(updated_key.id);
        let nonce_key = updated_key.nonce_key;

        if let Some(info) = self.nonces.get_mut(&nonce_key) {
            info.target_nonce.pending_outcomes.remove(&updater);
            info.target_nonce.nonce = std::cmp::max(info.target_nonce.nonce, nonce);
            let txs_awaiting_nonce = info.txs_awaiting_nonce.clone();
            let mut to_remove = Vec::new();

            // Don't convert AwaitingNonce→Ready here. Re-queued blocks are
            // sent after all new blocks, so nonces assigned now would be
            // stale by send time. Just update pending_outcomes and nonce
            // info on each tx. resolve_block_nonces() assigns fresh nonces
            // just before sending.
            if !txs_awaiting_nonce.is_empty() {
                let mut tx_block_queue = tx_block_queue.lock();
                for r in &txs_awaiting_nonce {
                    let tx = Self::get_tx(&mut tx_block_queue, r);

                    match tx {
                        TargetChainTx::AwaitingNonce(t) => {
                            t.target_nonce.pending_outcomes.remove(&updater);
                            t.target_nonce.nonce = std::cmp::max(t.target_nonce.nonce, nonce);
                        }
                        TargetChainTx::Ready(_) => {
                            // resolve_block_nonces may have already converted this tx.
                            to_remove.push(r.clone());
                        }
                    };
                }
            }

            let info = self.nonces.get_mut(&nonce_key).unwrap();
            for r in &to_remove {
                info.txs_awaiting_nonce.remove(r);
            }
        }
        Ok(())
    }

    fn on_outcome_finished(
        &mut self,
        db: &DB,
        id: &CryptoHash,
        nonce_keys: &HashSet<NonceLookupKey>,
    ) -> anyhow::Result<()> {
        let updater = NonceUpdater::ChainObjectId(*id);
        if let Some(keys) = self.updater_to_keys.remove(&updater) {
            assert!(nonce_keys == &keys);
        }

        crate::delete_pending_outcome(db, id)
    }

    fn on_target_block_tx(
        &mut self,
        tx_block_queue: &Mutex<VecDeque<MappedBlock>>,
        db: &DB,
        tx: IndexerTransactionWithOutcome,
    ) -> anyhow::Result<()> {
        if let Some(info) = self.sent_txs.remove(&tx.transaction.hash) {
            crate::metrics::TRANSACTIONS_INCLUDED.inc();
            self.remove_tx(&tx);
            if info.source_height > self.height_seen {
                self.height_seen = info.source_height;
            }
        }
        if let Some(nonce_keys) = crate::read_pending_outcome(db, &tx.transaction.hash)? {
            match tx.outcome.execution_outcome.outcome.status {
                ExecutionStatusView::SuccessReceiptId(receipt_id) => {
                    self.tx_to_receipt(
                        tx_block_queue,
                        db,
                        &tx.transaction.hash,
                        &receipt_id,
                        nonce_keys,
                    )?;
                }
                ExecutionStatusView::SuccessValue(_) | ExecutionStatusView::Unknown => {
                    unreachable!()
                }
                ExecutionStatusView::Failure(_) => {
                    self.on_outcome_finished(db, &tx.transaction.hash, &nonce_keys)?;
                }
            };
        }
        Ok(())
    }

    fn on_target_block_applied_receipt(
        &mut self,
        db: &DB,
        outcome: IndexerExecutionOutcomeWithReceipt,
        staked_accounts: &mut HashMap<(AccountId, PublicKey), AccountId>,
        access_key_updates: &mut Vec<UpdatedKey>,
    ) -> anyhow::Result<()> {
        let nonce_keys = match crate::read_pending_outcome(db, &outcome.execution_outcome.id)? {
            Some(a) => a,
            None => return Ok(()),
        };

        self.on_outcome_finished(db, &outcome.execution_outcome.id, &nonce_keys)?;
        access_key_updates.extend(
            nonce_keys
                .into_iter()
                .map(|nonce_key| UpdatedKey { nonce_key, id: outcome.execution_outcome.id }),
        );

        for receipt_id in outcome.execution_outcome.outcome.receipt_ids {
            // we don't carry over the access keys here, because we set pending access keys when we send a tx with
            // an add key action, which should be applied after one receipt. Setting empty access keys here allows us
            // to keep track of which receipts are descendants of our transactions, so that we can reverse any stake actions
            crate::put_pending_outcome(db, receipt_id, HashSet::default())?;
        }
        if !crate::execution_status_good(&outcome.execution_outcome.outcome.status) {
            return Ok(());
        }
        match outcome.receipt.receipt {
            ReceiptEnumView::Action { actions, .. } => {
                // since this receipt was recorded in the DB, it means that this stake action
                // resulted from one of our txs (not an external/manual stake action by the test operator),
                // so we want to reverse it.
                for a in actions {
                    if let ActionView::Stake { public_key, stake } = a {
                        if stake > Balance::ZERO {
                            staked_accounts.insert(
                                (outcome.receipt.receiver_id.clone(), public_key),
                                outcome.receipt.predecessor_id.clone(),
                            );
                        }
                    }
                }
            }
            ReceiptEnumView::Data { .. } | ReceiptEnumView::GlobalContractDistribution { .. } => {}
        };
        Ok(())
    }

    // return value maps (receiver_id, staked public key) to the predecessor_id in the
    // receipt for any receipts that contain stake actions (w/ nonzero stake) that were
    // generated by our transactions. Then the caller will send extra stake transactions
    // to reverse those.
    pub(crate) fn on_target_block(
        &mut self,
        tx_block_queue: &Mutex<VecDeque<MappedBlock>>,
        db: &DB,
        msg: StreamerMessage,
    ) -> anyhow::Result<TargetBlockInfo> {
        self.record_block_timestamp(&msg);
        self.log_target_block(&msg);

        let mut access_key_updates = Vec::new();
        let mut staked_accounts = HashMap::new();
        for s in msg.shards {
            if let Some(c) = s.chunk {
                for tx in c.transactions {
                    self.on_target_block_tx(tx_block_queue, db, tx)?;
                }
                for outcome in s.receipt_execution_outcomes {
                    self.on_target_block_applied_receipt(
                        db,
                        outcome,
                        &mut staked_accounts,
                        &mut access_key_updates,
                    )?;
                }
            }
        }
        Ok(TargetBlockInfo { staked_accounts, access_key_updates })
    }

    fn on_tx_sent(
        &mut self,
        tx_block_queue: &Mutex<VecDeque<MappedBlock>>,
        db: &DB,
        tx_ref: Option<TxRef>,
        tx: MappedTx,
        target_height: BlockHeight,
        now: Instant,
        keys_to_remove: &mut HashSet<NonceLookupKey>,
    ) -> anyhow::Result<()> {
        let hash = tx.target_tx.get_hash();
        if self.sent_txs.contains_key(&hash) {
            tracing::warn!(target: "mirror", ?hash, "transaction sent twice");
            return Ok(());
        }

        if tx.provenance.is_add_key()
            || tx.provenance.is_create_account()
            || tx.provenance.is_unstake()
        {
            tracing::debug!(
                target: "mirror",
                ?hash,
                provenance = %tx.provenance,
                actions = ?tx.target_tx.transaction.actions(),
                "successfully sent transaction"
            );
        }
        let nonce_key = NonceLookupKey::from_tx(&tx.target_tx.transaction);
        let source_height = tx_ref.as_ref().map(|t| t.source_height);
        // TODO: don't keep adding txs if we're not ever finding them on chain, since we'll OOM eventually
        // if that happens.
        self.sent_txs.insert(hash, TxSendInfo::new(&tx, source_height, target_height, now));
        let txs = self.txs_by_signer.entry(nonce_key.clone()).or_default();

        if let Some(highest_nonce) = txs.iter().next_back() {
            if highest_nonce.nonce > tx.target_tx.transaction.nonce().nonce() {
                tracing::warn!(
                    target: "mirror",
                    ?hash,
                    nonce = %tx.target_tx.transaction.nonce().nonce(),
                    ?txs,
                    "transaction sent with out of order nonce, sent so far"
                );
            }
        }
        if !txs.insert(TxId { hash, nonce: tx.target_tx.transaction.nonce().nonce() }) {
            tracing::warn!(target: "mirror", ?hash, "inserted tx twice into txs_by_signer");
        }

        match &tx_ref {
            Some(tx_ref) => {
                let updater = NonceUpdater::TxRef(tx_ref.clone());
                let new_updater = NonceUpdater::ChainObjectId(hash);
                assert!(
                    &tx.nonce_updates == &self.updater_to_keys.remove(&updater).unwrap_or_default()
                );
                for nonce_key in &tx.nonce_updates {
                    let mut t = crate::read_target_nonce(db, nonce_key)?.unwrap();
                    t.pending_outcomes.insert(hash);
                    crate::put_target_nonce(db, nonce_key, &t)?;

                    let info = self.nonces.get_mut(nonce_key).unwrap();
                    assert!(info.target_nonce.pending_outcomes.remove(&updater));
                    info.target_nonce.pending_outcomes.insert(new_updater.clone());
                    let txs_awaiting_nonce = info.txs_awaiting_nonce.clone();
                    if info.last_height <= source_height {
                        keys_to_remove.insert(nonce_key.clone());
                    }

                    if !txs_awaiting_nonce.is_empty() {
                        let mut tx_block_queue = tx_block_queue.lock();
                        for r in &txs_awaiting_nonce {
                            let t = Self::get_tx(&mut tx_block_queue, r);

                            match t {
                                TargetChainTx::AwaitingNonce(t) => {
                                    assert!(t.target_nonce.pending_outcomes.remove(&updater));
                                    t.target_nonce.pending_outcomes.insert(new_updater.clone());
                                }
                                TargetChainTx::Ready(_) => unreachable!(),
                            };
                        }
                    }
                }
            }
            None => {
                // if tx_ref is None, it was an extra tx we sent to unstake an unwanted validator,
                // and there should be no access key updates
                assert!(tx.nonce_updates.is_empty());
            }
        }

        crate::put_pending_outcome(db, hash, tx.nonce_updates)?;

        let mut t = crate::read_target_nonce(db, &nonce_key)?.unwrap();
        t.nonce = std::cmp::max(t.nonce, Some(tx.target_tx.transaction.nonce().nonce()));
        crate::put_target_nonce(db, &nonce_key, &t)?;
        let info = self.nonces.get_mut(&nonce_key).unwrap();
        if info.last_height <= source_height {
            keys_to_remove.insert(nonce_key);
        }
        if let Some(tx_ref) = tx_ref {
            assert!(info.queued_txs.remove(&tx_ref));
        }
        Ok(())
    }

    // among the last 10 blocks, what's the second longest time between their timestamps?
    // probably there's a better heuristic to use than that but this will do for now.
    // TODO: it's possible these timestamps are just increasing by one nanosecond each time
    // if block producers' clocks are off. should handle that case
    fn second_longest_recent_block_delay(&self) -> Option<Duration> {
        if self.recent_block_timestamps.len() < 5 {
            return None;
        }
        let mut last = *self.recent_block_timestamps.front().unwrap();
        let mut longest = None;
        let mut second_longest = None;

        for timestamp in self.recent_block_timestamps.iter().skip(1) {
            let delay = timestamp - last;

            match longest {
                Some(l) => match second_longest {
                    Some(s) => {
                        if delay > l {
                            second_longest = longest;
                            longest = Some(delay);
                        } else if delay > s {
                            second_longest = Some(delay);
                        }
                    }
                    None => {
                        if delay > l {
                            second_longest = longest;
                            longest = Some(delay);
                        } else {
                            second_longest = Some(delay);
                        }
                    }
                },
                None => {
                    longest = Some(delay);
                }
            }
            last = *timestamp;
        }
        let delay = Duration::from_nanos(second_longest.unwrap());
        if delay > 2 * self.min_block_production_delay {
            tracing::warn!(
                target: "mirror",
                ?delay,
                longest_delay = ?Duration::from_nanos(longest.unwrap()),
                min_delay = ?self.min_block_production_delay,
                "target chain blocks are taking longer than expected to be produced, observing delays vs min_block_production_delay"
            )
        }
        Some(delay)
    }

    fn on_tx_skipped(
        &mut self,
        tx_block_queue: &Mutex<VecDeque<MappedBlock>>,
        tx_ref: &Option<TxRef>,
        tx: &Transaction,
        nonce_updates: &HashSet<NonceLookupKey>,
        keys_to_remove: &mut HashSet<NonceLookupKey>,
    ) -> anyhow::Result<()> {
        let tx_ref = match tx_ref {
            Some(t) => t,
            None => return Ok(()),
        };
        let updater = NonceUpdater::TxRef(tx_ref.clone());
        if let Some(keys) = self.updater_to_keys.remove(&updater) {
            assert!(&keys == nonce_updates);

            for nonce_key in keys {
                if let Some(info) = self.nonces.get(&nonce_key) {
                    let txs_awaiting_nonce = info.txs_awaiting_nonce.clone();
                    let mut to_remove = Vec::new();
                    if !txs_awaiting_nonce.is_empty() {
                        let mut tx_block_queue = tx_block_queue.lock();
                        for r in &txs_awaiting_nonce {
                            let target_tx = Self::get_tx(&mut tx_block_queue, r);
                            match target_tx {
                                TargetChainTx::AwaitingNonce(tx) => {
                                    assert!(tx.target_nonce.pending_outcomes.remove(&updater));
                                    // Don't resolve here; resolve_block_nonces
                                    // handles it at send time.
                                }
                                TargetChainTx::Ready(_) => {
                                    to_remove.push(r.clone());
                                }
                            }
                        }
                    }

                    let info = self.nonces.get_mut(&nonce_key).unwrap();
                    for r in &to_remove {
                        info.txs_awaiting_nonce.remove(r);
                    }

                    if info.last_height <= Some(tx_ref.source_height) {
                        keys_to_remove.insert(nonce_key);
                    }
                }
            }
        }
        let nonce_key = NonceLookupKey::from_tx(tx);
        let info = self.nonces.get_mut(&nonce_key).unwrap();
        if info.last_height <= Some(tx_ref.source_height) {
            keys_to_remove.insert(nonce_key);
        }
        assert!(info.queued_txs.remove(tx_ref));
        Ok(())
    }

    // We just successfully sent some transactions. Remember them so we can see if they really show up on chain.
    // Returns (next_send_delay, nonce_keys_needing_resolution).
    // The caller should fetch nonces for the returned keys from the target chain
    // and call force_resolve_nonces() for each one that is available.
    pub(crate) fn on_txs_sent(
        &mut self,
        tx_block_queue: &Mutex<VecDeque<MappedBlock>>,
        db: &DB,
        sent_batch: SentBatch,
        target_height: BlockHeight,
    ) -> anyhow::Result<(Duration, Vec<NonceLookupKey>)> {
        let mut total_sent = 0;
        let now = Instant::now();
        let mut keys_to_remove = HashSet::new();
        let mut nonce_keys_to_resolve = Vec::new();

        let (txs_sent, deferred_updater_inserts, provenance, was_requeued) = match sent_batch {
            SentBatch::MappedBlock(b) => {
                let source_height = b.source_height;
                let source_hash = b.source_hash;

                // Split txs into ready (sent or skipped) and still awaiting nonce.
                let mut requeue = Vec::new();
                let mut ready_txs = Vec::new();
                for (tx_ref, tx) in b.txs {
                    match tx {
                        TargetChainTx::AwaitingNonce(_) => requeue.push((tx_ref, tx)),
                        TargetChainTx::Ready(_) => ready_txs.push((Some(tx_ref), tx)),
                    }
                }

                // Re-queue AwaitingNonce txs. The new block is pushed to the
                // front of the queue so that on_tx_sent (below) can find
                // the awaiting-nonce txs via get_tx when updating their
                // pending_outcomes.
                //
                // We must be careful: the re-queued txs get new TxRef values
                // (tx_idx renumbered from 0) that may collide with ready txs'
                // original TxRefs in updater_to_keys. We defer such inserts
                // and apply them after the ready txs have been processed.
                let mut deferred_updater_inserts = Vec::new();
                let ready_refs: HashSet<TxRef> =
                    ready_txs.iter().filter_map(|(ref_opt, _)| ref_opt.clone()).collect();

                // Drop AwaitingNonce txs that have exceeded the max re-queue attempts.
                const MAX_REQUEUE_ATTEMPTS: usize = 5;
                let attempt = self.requeue_attempts.entry(source_height).or_insert(0);
                *attempt += 1;

                if !requeue.is_empty() && *attempt > MAX_REQUEUE_ATTEMPTS {
                    tracing::warn!(
                        target: "mirror",
                        count = requeue.len(),
                        source_height,
                        attempts = *attempt,
                        "dropping awaiting-nonce txs after max re-queue attempts",
                    );
                    // Move to ready_txs so they go through on_tx_skipped.
                    for (tx_ref, tx) in requeue {
                        ready_txs.push((Some(tx_ref), tx));
                    }
                    requeue = Vec::new();
                    self.requeue_attempts.remove(&source_height);
                }

                if !requeue.is_empty() {
                    crate::metrics::TRANSACTIONS_REQUEUED.inc_by(requeue.len() as u64);
                    tracing::info!(
                        target: "mirror",
                        count = requeue.len(),
                        source_height,
                        "re-queuing transactions still awaiting nonce",
                    );

                    // Build chunks grouped by shard_id, assigning new tx indices.
                    let mut chunks_map: HashMap<ShardId, Vec<TargetChainTx>> = HashMap::new();
                    let mut ref_updates = Vec::new(); // (old_ref, new_ref)

                    for (old_ref, tx) in requeue {
                        let shard_id = old_ref.shard_id;
                        let chunk_txs = chunks_map.entry(shard_id).or_insert_with(Vec::new);
                        let new_ref = TxRef { source_height, shard_id, tx_idx: chunk_txs.len() };
                        // Collect nonce keys so the caller can actively fetch nonces.
                        if let TargetChainTx::AwaitingNonce(ref t) = tx {
                            nonce_keys_to_resolve.push(NonceLookupKey::from_tx(&t.target_tx));
                        }
                        chunk_txs.push(tx);
                        ref_updates.push((old_ref, new_ref));
                    }

                    let chunks: Vec<MappedChunk> = chunks_map
                        .into_iter()
                        .map(|(shard_id, txs)| MappedChunk { shard_id, txs })
                        .collect();

                    let new_block = MappedBlock { source_height, source_hash, chunks };

                    // Update all tracking structures for the ref changes.
                    for (old_ref, new_ref) in &ref_updates {
                        // Find the nonce key for this tx.
                        let nonce_key = {
                            let chunk = new_block
                                .chunks
                                .iter()
                                .find(|c| c.shard_id == new_ref.shard_id)
                                .unwrap();
                            match &chunk.txs[new_ref.tx_idx] {
                                TargetChainTx::AwaitingNonce(t) => {
                                    NonceLookupKey::from_tx(&t.target_tx)
                                }
                                TargetChainTx::Ready(_) => unreachable!(),
                            }
                        };

                        // Update txs_awaiting_nonce and queued_txs.
                        if let Some(info) = self.nonces.get_mut(&nonce_key) {
                            info.txs_awaiting_nonce.remove(old_ref);
                            info.txs_awaiting_nonce.insert(new_ref.clone());
                            info.queued_txs.remove(old_ref);
                            info.queued_txs.insert(new_ref.clone());
                        }

                        // Update updater_to_keys.
                        let old_updater = NonceUpdater::TxRef(old_ref.clone());
                        let new_updater = NonceUpdater::TxRef(new_ref.clone());
                        if let Some(keys) = self.updater_to_keys.remove(&old_updater) {
                            for key in &keys {
                                if let Some(info) = self.nonces.get_mut(key) {
                                    if info.target_nonce.pending_outcomes.remove(&old_updater) {
                                        info.target_nonce
                                            .pending_outcomes
                                            .insert(new_updater.clone());
                                    }
                                }
                            }
                            // If the new ref collides with a ready tx's TxRef,
                            // defer the insert until after ready txs are processed.
                            if ready_refs.contains(&new_ref)
                                || self.updater_to_keys.contains_key(&new_updater)
                            {
                                deferred_updater_inserts.push((new_updater.clone(), keys));
                            } else {
                                self.updater_to_keys.insert(new_updater.clone(), keys);
                            }
                        }
                    }

                    {
                        let mut q = tx_block_queue.lock();
                        let pos = q
                            .binary_search_by(|b| b.source_height.cmp(&source_height))
                            .expect_err("block at this height was just removed");
                        q.insert(pos, new_block);
                    }

                    // Update pending_outcomes inside the re-queued txs themselves.
                    {
                        let mut tx_block_queue = tx_block_queue.lock();
                        for (old_ref, new_ref) in &ref_updates {
                            let old_updater = NonceUpdater::TxRef(old_ref.clone());
                            let new_updater = NonceUpdater::TxRef(new_ref.clone());
                            let tx = Self::get_tx(&mut tx_block_queue, new_ref);
                            if let TargetChainTx::AwaitingNonce(t) = tx {
                                if t.target_nonce.pending_outcomes.remove(&old_updater) {
                                    t.target_nonce.pending_outcomes.insert(new_updater);
                                }
                            }
                        }
                    }
                }

                self.height_popped = match self.height_popped {
                    Some(h) => Some(std::cmp::max(h, source_height)),
                    None => Some(source_height),
                };

                let was_requeued = self.requeue_attempts.contains_key(&source_height);
                (
                    ready_txs,
                    deferred_updater_inserts,
                    format!("source #{}", source_height),
                    was_requeued,
                )
            }
            SentBatch::ExtraTxs(txs) => (
                txs.into_iter().map(|tx| (None, tx)).collect::<Vec<_>>(),
                Vec::new(),
                String::from("extra unstake transactions"),
                false,
            ),
        };
        for (tx_ref, tx) in txs_sent {
            match tx {
                crate::TargetChainTx::Ready(t) => {
                    if t.sent_successfully {
                        self.on_tx_sent(
                            tx_block_queue,
                            db,
                            tx_ref,
                            t,
                            target_height,
                            now,
                            &mut keys_to_remove,
                        )?;
                        total_sent += 1;
                    } else {
                        self.on_tx_skipped(
                            tx_block_queue,
                            &tx_ref,
                            &t.target_tx.transaction,
                            &t.nonce_updates,
                            &mut keys_to_remove,
                        )?;
                    }
                }
                crate::TargetChainTx::AwaitingNonce(t) => {
                    self.on_tx_skipped(
                        tx_block_queue,
                        &tx_ref,
                        &t.target_tx,
                        &t.nonce_updates,
                        &mut keys_to_remove,
                    )?;
                }
            }
        }

        // Apply deferred updater inserts now that ready txs are processed.
        for (updater, keys) in deferred_updater_inserts {
            self.updater_to_keys.insert(updater, keys);
        }

        for nonce_key in keys_to_remove {
            assert!(self.nonces.remove(&nonce_key).is_some());
        }
        tracing::info!(
            target: "mirror",
            tx_count = total_sent,
            %provenance,
            ?target_height,
            "sent txs",
        );

        nonce_keys_to_resolve.dedup();

        let normal_delay = self.tx_batch_interval.unwrap_or_else(|| {
            self.second_longest_recent_block_delay()
                .unwrap_or(self.min_block_production_delay + Duration::from_millis(100))
        });
        let next_delay = if was_requeued || !nonce_keys_to_resolve.is_empty() {
            Duration::from_millis(100)
        } else {
            normal_delay
        };
        Ok((next_delay, nonce_keys_to_resolve))
    }

    pub(crate) fn force_resolve_nonces(
        &mut self,
        _tx_block_queue: &Mutex<VecDeque<MappedBlock>>,
        db: &DB,
        nonce_key: &NonceLookupKey,
        source_height: BlockHeight,
        nonce: Nonce,
    ) -> anyhow::Result<()> {
        if let Some(mut n) = crate::read_target_nonce(db, nonce_key)? {
            n.nonce = std::cmp::max(n.nonce, Some(nonce));
            n.pending_outcomes.clear();
            crate::put_target_nonce(db, nonce_key, &n)?;
        }

        let info = match self.nonces.get_mut(nonce_key) {
            Some(info) => info,
            None => return Ok(()),
        };
        info.target_nonce.nonce = std::cmp::max(info.target_nonce.nonce, Some(nonce));
        info.target_nonce.pending_outcomes.clear();

        // Don't assign nonces to individual txs here. Re-queued blocks are
        // sent after all new blocks, so nonces assigned now would be stale
        // by send time. Instead, resolve_block_nonces() is called just
        // before sending to assign fresh nonces from info.target_nonce.

        tracing::debug!(
            target: "mirror",
            ?nonce_key,
            source_height,
            nonce,
            "recorded discovered nonce for key",
        );

        Ok(())
    }

    /// Resolve AwaitingNonce txs in a specific block just before sending.
    /// Must be called while holding the tracker lock to coordinate with
    /// next_nonce(). Uses info.target_nonce.nonce as the base, which
    /// reflects all nonces assigned to date (including future mapped blocks).
    pub(crate) fn resolve_block_nonces(
        &mut self,
        tx_block_queue: &mut VecDeque<MappedBlock>,
        block_idx: usize,
    ) {
        let block = match tx_block_queue.get(block_idx) {
            Some(b) => b,
            None => return,
        };
        // Collect (chunk_idx, tx_idx, nonce_key) for AwaitingNonce txs.
        let mut to_resolve: Vec<(usize, usize, NonceLookupKey)> = Vec::new();
        for (ci, chunk) in block.chunks.iter().enumerate() {
            for (ti, tx) in chunk.txs.iter().enumerate() {
                if let TargetChainTx::AwaitingNonce(t) = tx {
                    to_resolve.push((ci, ti, NonceLookupKey::from_tx(&t.target_tx)));
                }
            }
        }
        if to_resolve.is_empty() {
            return;
        }
        let source_height = block.source_height;
        let block = tx_block_queue.get_mut(block_idx).unwrap();
        let mut resolved: u64 = 0;
        for (ci, ti, nonce_key) in &to_resolve {
            let info = match self.nonces.get_mut(nonce_key) {
                Some(info) => info,
                None => continue,
            };
            let nonce = match info.target_nonce.nonce {
                Some(n) => n + 1,
                None => continue,
            };
            info.target_nonce.nonce = Some(nonce);
            let tx_ref = TxRef { source_height, shard_id: block.chunks[*ci].shard_id, tx_idx: *ti };
            info.txs_awaiting_nonce.remove(&tx_ref);
            let tx = &mut block.chunks[*ci].txs[*ti];
            if let TargetChainTx::AwaitingNonce(_) = tx {
                tx.set_nonce(nonce);
                resolved += 1;
                tracing::debug!(
                    target: "mirror",
                    ?nonce_key,
                    %tx_ref,
                    nonce,
                    "resolved nonce at send time",
                );
            }
        }
        if resolved > 0 {
            crate::metrics::NONCES_FORCE_RESOLVED.with_label_values(&["success"]).inc_by(resolved);
            tracing::info!(
                target: "mirror",
                source_height,
                resolved,
                total = to_resolve.len(),
                "resolved awaiting-nonce txs at send time",
            );
        }
    }
}
