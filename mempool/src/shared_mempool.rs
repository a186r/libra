// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    core_mempool::{CoreMempool, TimelineState},
    OP_COUNTERS,
};
use bounded_executor::BoundedExecutor;
use futures::{channel::mpsc::UnboundedSender, future::join_all, Stream, StreamExt};
use libra_config::config::{MempoolConfig, NodeConfig};
use libra_logger::prelude::*;
use libra_types::{transaction::SignedTransaction, PeerId};
use network::{
    proto::MempoolSyncMsg,
    validator_network::{Event, MempoolNetworkEvents, MempoolNetworkSender},
};
use std::{
    collections::HashMap,
    convert::{TryFrom, TryInto},
    ops::Deref,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};
use storage_client::StorageRead;
use tokio::{
    runtime::{Builder, Handle, Runtime},
    time::interval,
};
use vm_validator::vm_validator::{get_account_state, TransactionValidation};
use futures::task::SpawnExt;

/// state of last sync with peer
/// `timeline_id` is position in log of ready transactions
/// `is_alive` - is connection healthy
#[derive(Clone)]
struct PeerSyncState {
    timeline_id: u64,
    is_alive: bool,
}

type PeerInfo = HashMap<PeerId, PeerSyncState>;

/// Outbound peer syncing event emitted by [`IntervalStream`].
#[derive(Debug)]
pub(crate) struct SyncEvent;

type IntervalStream = Pin<Box<dyn Stream<Item = SyncEvent> + Send + 'static>>;

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum SharedMempoolNotification {
    Sync,
    PeerStateChange,
    NewTransactions,
}

/// Struct that owns all dependencies required by shared mempool routines
#[derive(Clone)]
struct SharedMempool<V>
where
    V: TransactionValidation + 'static,
{
    mempool: Arc<Mutex<CoreMempool>>,
    network_sender: MempoolNetworkSender,
    config: MempoolConfig,
    storage_read_client: Arc<dyn StorageRead>,
    validator: Arc<V>,
    peer_info: Arc<Mutex<PeerInfo>>,
    subscribers: Vec<UnboundedSender<SharedMempoolNotification>>,
}

fn notify_subscribers(
    event: SharedMempoolNotification,
    subscribers: &[UnboundedSender<SharedMempoolNotification>],
) {
    for subscriber in subscribers {
        let _ = subscriber.unbounded_send(event);
    }
}

fn default_timer(tick_ms: u64) -> IntervalStream {
    interval(Duration::from_millis(tick_ms))
        .map(|_| SyncEvent)
        .boxed()
}

/// new peer discovery handler
/// adds new entry to `peer_info`
fn new_peer(peer_info: &Mutex<PeerInfo>, peer_id: PeerId) {
    peer_info
        .lock()
        .expect("[shared mempool] failed to acquire peer_info lock")
        .entry(peer_id)
        .or_insert(PeerSyncState {
            timeline_id: 0,
            is_alive: true,
        })
        .is_alive = true;
}

/// lost peer handler. Marks connection as dead
fn lost_peer(peer_info: &Mutex<PeerInfo>, peer_id: PeerId) {
    if let Some(state) = peer_info
        .lock()
        .expect("[shared mempool] failed to acquire peer_info lock")
        .get_mut(&peer_id)
    {
        state.is_alive = false;
    }
}

/// sync routine
/// used to periodically broadcast ready to go transactions to peers
/// 针对每个节点推送所有来自自身AC模块的Tx。为了避免重复使用，使用了timeline_id这个技术
/// 也就是对每一个Tx进行编号，并且这个编号是单增的，这样在节点A推送的时候只需要记住上次推送到了第n个，下次从第n+1推送即可。
async fn sync_with_peers<'a>(
    peer_info: &'a Mutex<PeerInfo>, // 一个所有其他节点的Map
    mempool: &'a Mutex<CoreMempool>, // 自己的内存池
    network_sender: &'a mut MempoolNetworkSender,// 广播消息的通道
    batch_size: usize,
) {
    // Clone the underlying peer_info map and use this to sync and collect
    // state updates. We do this instead of holding the lock for the whole
    // function since that would hold the lock across await points which is bad.
    let peer_info_copy = peer_info
        .lock()
        .expect("[shared mempool] failed to acquire peer_info lock")
        .deref()
        .clone();

    let mut state_updates = vec![];

    for (peer_id, peer_state) in peer_info_copy.into_iter() {
        if peer_state.is_alive {
            let timeline_id = peer_state.timeline_id;

            let (transactions, new_timeline_id) = mempool
                .lock()
                .expect("[shared mempool] failed to acquire mempool lock")
                .read_timeline(timeline_id, batch_size);

            if !transactions.is_empty() {
                OP_COUNTERS.inc_by("smp.sync_with_peers", transactions.len());
                let mut msg = MempoolSyncMsg::default();
                msg.peer_id = peer_id.into();
                msg.transactions = transactions
                    .into_iter()
                    .map(|txn| txn.try_into().unwrap())
                    .collect();

                trace!(
                    "MempoolNetworkSender.send_to peer {} msg {:?}",
                    peer_id,
                    msg
                );
                // Since this is a direct-send, this will only error if the network
                // module has unexpectedly crashed or shutdown.
                // 向指定的peer_id推送transactions数组
                network_sender
                    .send_to(peer_id, msg)
                    .await
                    .expect("[shared mempool] failed to direct-send mempool sync message");
            }

            state_updates.push((peer_id, new_timeline_id));
        }
    }

    // Lock the shared peer_info and apply state updates.
    let mut peer_info = peer_info
        .lock()
        .expect("[shared mempool] failed to acquire peer_info lock");
    // 更新相应节点的timeline_id，不要重复推送了
    for (peer_id, new_timeline_id) in state_updates {
        peer_info
            .entry(peer_id)
            .and_modify(|t| t.timeline_id = new_timeline_id);
    }
}

/// used to validate incoming transactions and add them to local Mempool
async fn process_incoming_transactions<V>(
    smp: SharedMempool<V>,
    peer_id: PeerId,
    transactions: Vec<SignedTransaction>,
) where
    V: TransactionValidation,
{
    let account_states = join_all(
        transactions
            .iter()
            .map(|t| get_account_state(smp.storage_read_client.clone(), t.sender())),
    )
    .await;

    // eagerly filter out transactions that were already committed
    let transactions: Vec<_> = transactions
        .into_iter()
        .enumerate()
        .filter_map(|(idx, t)| {
            if let Ok((sequence_number, balance)) = account_states[idx] {
                if t.sequence_number() >= sequence_number {
                    return Some((t, sequence_number, balance));
                }
            }
            None
        })
        .collect();

    let validations = join_all(
        transactions
            .iter()
            .map(|t| smp.validator.validate_transaction(t.0.clone())),
    )
    .await;

    {
        let mut mempool = smp
            .mempool
            .lock()
            .expect("[shared mempool] failed to acquire mempool lock");

        for (idx, (transaction, sequence_number, balance)) in transactions.into_iter().enumerate() {
            if let Ok(None) = validations[idx] {
                let gas_cost = transaction.max_gas_amount();
                let insertion_result = mempool.add_txn(
                    transaction,
                    gas_cost,
                    sequence_number,
                    balance,
                    // tx不会再被广播给其他节点，好处是极大的降低了数据传输量，这种方式在以太坊中肯定是不会采用的，因为这很不利于Tx的快速广播
                    TimelineState::NonQualified,
                );
                OP_COUNTERS.inc(&format!(
                    "smp.transactions.status.{:?}.{:?}",
                    insertion_result.code, peer_id
                ));
            } else {
                OP_COUNTERS.inc(&format!(
                    "smp.transactions.status.validation_failed.{:?}",
                    peer_id
                ));
            }
        }
    }
    notify_subscribers(SharedMempoolNotification::NewTransactions, &smp.subscribers);
}

/// This task handles [`SyncEvent`], which is periodically emitted for us to
/// broadcast ready to go transactions to peers.
/// 向外广播来自AC的Tx
async fn outbound_sync_task<V>(smp: SharedMempool<V>, mut interval: IntervalStream)
where
    V: TransactionValidation,
{
    let peer_info = smp.peer_info;
    let mempool = smp.mempool;
    let mut network_sender = smp.network_sender;
    let batch_size = smp.config.shared_mempool_batch_size;
    let subscribers = smp.subscribers;

    // 定时死循环，这个执行到await的
    /**
        当代码执行到await里面的时候，发现异步操作还没有完成，它会直接退出当前这个函数，把CPU让给其他任务执行，
        当这个数据从网络上传输完成了，调度器会再次调用这个函数。
        它会从上次中断的地方回复执行，所以使用async/await的语法写代码，异步代码的逻辑在源码组织上跟同步代码的逻辑差别并不大，
        这里面状态保存和恢复这些琐碎的事情，由编译器帮我们完成了。
    */
    while let Some(sync_event) = interval.next().await {
        trace!("SyncEvent: {:?}", sync_event);
        sync_with_peers(&peer_info, &mempool, &mut network_sender, batch_size).await;
        notify_subscribers(SharedMempoolNotification::Sync, &subscribers);
    }

    crit!("SharedMempool outbound_sync_task terminated");
}

/// This task handles inbound network events.
/// 接收来自底层Network模块的信息推送
async fn inbound_network_task<V>(
    smp: SharedMempool<V>,
    executor: Handle,
    mut network_events: MempoolNetworkEvents,
) where
    V: TransactionValidation,
{
    let peer_info = smp.peer_info.clone();
    let subscribers = smp.subscribers.clone();

    // Use a BoundedExecutor to restrict only `workers_available` concurrent
    // worker tasks that can process incoming transactions.
    let workers_available = smp.config.shared_mempool_max_concurrent_inbound_syncs;
    let bounded_executor = BoundedExecutor::new(workers_available, executor);

    // while循环，永远不结束
    while let Some(event) = network_events.next().await {
        trace!("SharedMempoolEvent::NetworkEvent::{:?}", event);
        match event {
            Ok(network_event) => match network_event {
                Event::NewPeer(peer_id) => {
                    OP_COUNTERS.inc("smp.event.new_peer");
                    // 有新的节点上线
                    new_peer(&peer_info, peer_id);
                    notify_subscribers(SharedMempoolNotification::PeerStateChange, &subscribers);
                }
                Event::LostPeer(peer_id) => {
                    OP_COUNTERS.inc("smp.event.lost_peer");
                    // 节点下线
                    lost_peer(&peer_info, peer_id);
                    notify_subscribers(SharedMempoolNotification::PeerStateChange, &subscribers);
                }

                // message主要就是其他节点推送来的新的Tx
                Event::Message((peer_id, msg)) => {
                    OP_COUNTERS.inc("smp.event.message");
                    let transactions: Vec<_> = msg
                        .transactions
                        .clone()
                        .into_iter()
                        .filter_map(|txn| match SignedTransaction::try_from(txn) {
                            Ok(t) => Some(t),
                            Err(e) => {
                                security_log(SecurityEvent::InvalidTransactionMP)
                                    .error(&e)
                                    .data(&msg)
                                    .log();
                                None
                            }
                        })
                        .collect();
                    OP_COUNTERS.inc_by(
                        &format!("smp.transactions.received.{:?}", peer_id),
                        transactions.len(),
                    );
                    // 验证Tx的有效性然后添加到自己的缓冲池中，添加过程调用的是add_txn
                    // 和处理来自AC的Tx是一样的逻辑
                    bounded_executor
                        .spawn(process_incoming_transactions(
                            smp.clone(),
                            peer_id,
                            transactions,
                        ))
                        .await;
                }
                _ => {
                    security_log(SecurityEvent::InvalidNetworkEventMP)
                        .error("UnexpectedNetworkEvent")
                        .data(&network_event)
                        .log();
                    debug_assert!(false, "Unexpected network event");
                }
            },
            Err(e) => {
                security_log(SecurityEvent::InvalidNetworkEventMP)
                    .error(&e)
                    .log();
            }
        }
    }
    crit!("SharedMempool inbound_network_task terminated");
}

/// GC all expired transactions by SystemTTL
/// 过期交易回收机制
async fn gc_task(mempool: Arc<Mutex<CoreMempool>>, gc_interval_ms: u64) {
    let mut interval = interval(Duration::from_millis(gc_interval_ms));
    // 定期调用gc_by_system_ttl，避免Tx在缓冲池中呆过久，占用空间，从而导致可以打包的交易进不到缓冲池中。
    // 以太坊采用了不同的处理方案，以太坊中如果是直接受到的Tx会保存在transactions.rlp这个文件中，就算是发生拥堵也不会丢失。

    // 但是Libra这种设计，如果发生了拥堵，交易丢失了如何解决，也许Libra中Tx有过期机制，一旦过期，client就认为交易失败了
    // 如果想要继续，就应该重新发送
    while let Some(_interval) = interval.next().await {
        mempool
            .lock()
            .expect("[shared mempool] failed to acquire mempool lock")
            .gc_by_system_ttl();
    }

    crit!("SharedMempool gc_task terminated");
}

/// bootstrap of SharedMempool
/// creates separate Tokio Runtime that runs following routines:
///   - outbound_sync_task (task that periodically broadcasts transactions to peers)
///   - inbound_network_task (task that handles inbound mempool messages and network events)
///   - gc_task (task that performs GC of all expired transactions by SystemTTL)
pub(crate) fn start_shared_mempool<V>(
    config: &NodeConfig,
    mempool: Arc<Mutex<CoreMempool>>,
    network_sender: MempoolNetworkSender, // 向其他节点推送新发现的Tx的Channel
    network_events: MempoolNetworkEvents, // 接受来自其他节点的Mempool事件的Channel
    storage_read_client: Arc<dyn StorageRead>,
    validator: Arc<V>,
    subscribers: Vec<UnboundedSender<SharedMempoolNotification>>, // 这个是通知其他模块，mempool发生了什么他们感兴趣的事情
    timer: Option<IntervalStream>,
) -> Runtime
where
    V: TransactionValidation + 'static,
{
    let runtime = Builder::new()
        .thread_name("shared-mem-")
        .threaded_scheduler()
        .enable_all()
        .build()
        .expect("[shared mempool] failed to create runtime");

    // 获取runtime的Executor，这样后续就可以启动task了
    let executor = runtime.handle();

    let peer_info = Arc::new(Mutex::new(PeerInfo::new()));

    let smp = SharedMempool {
        mempool: mempool.clone(),
        config: config.mempool.clone(),
        network_sender,
        storage_read_client,
        validator,
        peer_info,
        subscribers,
    };

    let interval_ms = config.mempool.shared_mempool_tick_interval_ms;
    let smp_outbound = smp.clone();
    let f = async move {
        let interval = timer.unwrap_or_else(|| default_timer(interval_ms));
        outbound_sync_task(smp_outbound, interval).await
    };

    executor.spawn(f);

    executor.spawn(inbound_network_task(smp, executor.clone(), network_events));

    executor.spawn(gc_task(
        mempool,
        config.mempool.system_transaction_gc_interval_ms,
    ));

    runtime
}
