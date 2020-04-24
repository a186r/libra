// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

//! mempool is used to track transactions which have been submitted but not yet
//! agreed upon.
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::{
    core_mempool::{
        index::TxnPointer,
        transaction::{MempoolTransaction, TimelineState},
        transaction_store::TransactionStore,
    },
    OP_COUNTERS,
};
use chrono::Utc;
use libra_config::config::NodeConfig;
use libra_logger::prelude::*;
use libra_mempool_shared_proto::{
    proto::mempool_status::MempoolAddTransactionStatusCode, MempoolAddTransactionStatus,
};
use libra_types::{account_address::AccountAddress, transaction::SignedTransaction};
use lru_cache::LruCache;
use std::{cmp::max, collections::HashSet, convert::TryFrom};
use ttl_cache::TtlCache;

pub struct Mempool {
    // stores metadata of all transactions in mempool (of all states)
    // 所有交易的元数据存储在mempool中，这里是系统的核心
    transactions: TransactionStore,

    // u64保存的是AccountAddress对应的下一个可以打包的Tx对应的seq_number
    sequence_number_cache: LruCache<AccountAddress, u64>,
    // temporary DS. TODO: eventually retire it
    // for each transaction, entry with timestamp is added when transaction enters mempool
    // used to measure e2e latency of transaction in system, as well as time it takes to pick it up
    // by consensus
    pub(crate) metrics_cache: TtlCache<(AccountAddress, u64), i64>,
    // 一个交易不能再缓存池中待的太久，如果迟迟不能被打包会被定期清理掉，这个时间就是其在缓冲池中呆的最长时间。
    pub system_transaction_timeout: Duration,
}

impl Mempool {
    pub(crate) fn new(config: &NodeConfig) -> Self {
        Mempool {
            transactions: TransactionStore::new(&config.mempool),
            sequence_number_cache: LruCache::new(config.mempool.capacity),
            metrics_cache: TtlCache::new(config.mempool.capacity),
            system_transaction_timeout: Duration::from_secs(
                config.mempool.system_transaction_timeout_secs,
            ),
        }
    }

    /// This function will be called once the transaction has been stored
    /// 移除打包的交易
    /// 共识模块确定Tx被打包了，那么缓冲池中的Tx就可以移除了，is_rejected表示是否被打包
    /// 同时is_rejected为false的时候，sequence_number也告诉mempool目前sender之前的Tx都被打包了。
    /// 本地的seqence_number也要更新到这里了
    pub(crate) fn remove_transaction(
        &mut self,
        sender: &AccountAddress,
        sequence_number: u64,
        is_rejected: bool,
    ) {
        debug!(
            "[Mempool] Removing transaction from mempool: {}:{}:{}",
            sender, sequence_number, is_rejected
        );
        self.log_latency(sender.clone(), sequence_number, "e2e.latency");
        self.metrics_cache.remove(&(*sender, sequence_number));
        OP_COUNTERS.inc(&format!("remove_transaction.{}", is_rejected));

        if is_rejected {
            debug!(
                "[Mempool] transaction is rejected: {}:{}",
                sender, sequence_number
            );
            self.transactions
                .reject_transaction(&sender, sequence_number);
        } else {
            // update current cached sequence number for account
            let current_seq_number = self
                .sequence_number_cache
                .remove(&sender)
                .unwrap_or_default();
            // new_seq_number保存的就是下一个有效的seq_number
            let new_seq_number = max(current_seq_number, sequence_number + 1);
            self.sequence_number_cache
                .insert(sender.clone(), new_seq_number);
            // 核心处理其实还在TransactionStore中
            self.transactions
                .commit_transaction(&sender, new_seq_number);
        }
    }

    fn log_latency(&mut self, account: AccountAddress, sequence_number: u64, metric: &str) {
        if let Some(&creation_time) = self.metrics_cache.get(&(account, sequence_number)) {
            if let Ok(time_delta_ms) = u64::try_from(Utc::now().timestamp_millis() - creation_time)
            {
                OP_COUNTERS.observe_duration(metric, Duration::from_millis(time_delta_ms));
            }
        }
    }

    fn get_required_balance(&mut self, txn: &SignedTransaction, gas_amount: u64) -> u64 {
        txn.gas_unit_price() * gas_amount + self.transactions.get_required_balance(&txn.sender())
    }

    /// Used to add a transaction to the Mempool
    /// Performs basic validation: checks account's balance and sequence number
    /// 用来新增交易到mempool中，执行基本验证：检查用户余额和nonce
    pub(crate) fn add_txn(
        &mut self,
        txn: SignedTransaction,
        gas_amount: u64,
        db_sequence_number: u64, // 已经确认的txn's sender的seq_number
        balance: u64, // 这个账户的余额
        timeline_state: TimelineState,
    ) -> MempoolAddTransactionStatus {
        debug!(
            "[Mempool] Adding transaction to mempool: {}:{}:{}",
            &txn.sender(),
            txn.sequence_number(),
            db_sequence_number,
        );

        // 账户余额不够，直接打出GG
        let required_balance = self.get_required_balance(&txn, gas_amount);
        if balance < required_balance {
            return MempoolAddTransactionStatus::new(
                MempoolAddTransactionStatusCode::InsufficientBalance,
                format!(
                    "balance: {}, required_balance: {}, gas_amount: {}",
                    balance, required_balance, gas_amount
                ),
            );
        }

        let cached_value = self.sequence_number_cache.get_mut(&txn.sender());
        let sequence_number =
            cached_value.map_or(db_sequence_number, |value| max(*value, db_sequence_number));
        self.sequence_number_cache
            .insert(txn.sender(), sequence_number);

        // don't accept old transactions (e.g. seq is less than account's current seq_number)
        // 不接受旧的交易，也就是nonce如果比当前nonce还小，直接打出GG
        if txn.sequence_number() < sequence_number {
            return MempoolAddTransactionStatus::new(
                MempoolAddTransactionStatusCode::InvalidSeqNumber,
                format!(
                    "transaction sequence number is {}, current sequence number is  {}",
                    txn.sequence_number(),
                    sequence_number,
                ),
            );
        }

        // 交易在缓冲池中的过期时间
        let expiration_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("init timestamp failure")
            + self.system_transaction_timeout;
        if timeline_state != TimelineState::NonQualified {
            self.metrics_cache.insert(
                (txn.sender(), txn.sequence_number()),
                Utc::now().timestamp_millis(),
                Duration::from_secs(100),
            );
        }
        // MempoolTransaction指的就是缓冲池中的Tx，为了缓冲池管理方便，增加了过期时间和TimelineState还有gasAmount
        let txn_info = MempoolTransaction::new(txn, expiration_time, gas_amount, timeline_state);

        // 真正的Tx，无论能否被打包，都在TransactionStore中保存着。
        let status = self.transactions.insert(txn_info, sequence_number);
        OP_COUNTERS.inc(&format!("insert.{:?}", status));
        status
    }

    /// Fetches next block of transactions for consensus
    /// `batch_size` - size of requested block
    /// `seen_txns` - transactions that were sent to Consensus but were not committed yet
    ///  Mempool should filter out such transactions
    /// get_block为consensus提供下一块交易数据
    /// 功能非常简单，就是挑出来下一块可以打包的交易，主要就是seq_number连起来的交易，因为不合法的交易早已经被踢了。
    /// 共识模块需要从mempool中拉去下一个块可用的Tx集合
    pub(crate) fn get_block(
        &mut self,
        batch_size: u64,
        mut seen: HashSet<TxnPointer>,
    ) -> Vec<SignedTransaction> {

        /**
            get_block实际上是寻找可以进入下一块的交易
            1、已经送到共识模块中，但是还没有确认（确认后会从缓冲池中移除）
            2、这个Tx的seq刚好就是下一个可以打包的，比如上一块中AccountA的seq是3，那么现在seq=4的Tx就可以进入block
            3、或者当前块中已经包含了seq=4，那么seq=5就可以进入
        */
        let mut result = vec![];
        // Helper DS. Helps to mitigate scenarios where account submits several transactions
        // with increasing gas price (e.g. user submits transactions with sequence number 1, 2
        // and gas_price 1, 10 respectively)
        // Later txn has higher gas price and will be observed first in priority index iterator,
        // but can't be executed before first txn. Once observed, such txn will be saved in
        // `skipped` DS and rechecked once it's ancestor becomes available
        let mut skipped = HashSet::new();

        // iterate over the queue of transactions based on gas price
        // 带标签的break用法
        'main: for txn in self.transactions.iter_queue() {
            if seen.contains(&TxnPointer::from(txn)) {
                continue;
            }
            let mut seq = txn.sequence_number;
            /**
                这里打包是按照地址选，尽可能把同一个地址的Tx都打包到一个block中去
            */
            let account_sequence_number = self.sequence_number_cache.get_mut(&txn.address);
            let seen_previous = seq > 0 && seen.contains(&(txn.address, seq - 1));
            // include transaction if it's "next" for given account or
            // we've already sent its ancestor to Consensus
            if seen_previous || account_sequence_number == Some(&mut seq) {
                let ptr = TxnPointer::from(txn);
                seen.insert(ptr);
                result.push(ptr);
                if (result.len() as u64) == batch_size {
                    // batch_size表示这块最多有多少个交易
                    break;
                }

                // check if we can now include some transactions
                // that were skipped before for given account
                // 回头遍历，比如先走过了seq=7的交易，那么发现seq=6合适的时候，就还可以吧seq=7加入
                let mut skipped_txn = (txn.address, seq + 1);
                while skipped.contains(&skipped_txn) {
                    seen.insert(skipped_txn);
                    result.push(skipped_txn);
                    if (result.len() as u64) == batch_size {
                        break 'main;
                    }
                    skipped_txn = (txn.address, skipped_txn.1 + 1);
                }
            } else {
                skipped.insert(TxnPointer::from(txn));
            }
        }
        // convert transaction pointers to real values
        let block: Vec<_> = result
            .into_iter()
            .filter_map(|(address, seq)| self.transactions.get(&address, seq))
            .collect();
        for transaction in &block {
            self.log_latency(
                transaction.sender(),
                transaction.sequence_number(),
                "txn_pre_consensus_s",
            );
        }
        // 一个Tx的集合，没有任何附加信息
        block
    }

    /// TTL based garbage collection. Remove all transactions that got expired
    /// 清除在mempool中待的太久的交易，否则mempool因为空间已满而无法进来有效的交易
    pub(crate) fn gc_by_system_ttl(&mut self) {
        self.transactions.gc_by_system_ttl();
    }

    /// Garbage collection based on client-specified expiration time
    /// 在新的一个块来临的时候，依据新块 时间可以非常确定那些用户指定的在这个时间 之前必须打包的交易必须被清理掉，因为再也不可能被打包了。
    pub(crate) fn gc_by_expiration_time(&mut self, block_time: Duration) {
        self.transactions.gc_by_expiration_time(block_time);
    }

    /// Read `count` transactions from timeline since `timeline_id`
    /// Returns block of transactions and new last_timeline_id
    /// 主要用于节点间mempool中的Tx同步用，就是为每一个Tx都给一个本地唯一的单增的编号，这样推送的时候就知道推送到哪里了，避免重复
    pub(crate) fn read_timeline(
        &mut self,
        timeline_id: u64,
        count: usize,
    ) -> (Vec<SignedTransaction>, u64) {
        self.transactions.read_timeline(timeline_id, count)
    }

    /// Check the health of core mempool.
    pub(crate) fn health_check(&self) -> bool {
        self.transactions.health_check()
    }
}
