---
id: mempool
title: Mempool
custom_edit_url: https://github.com/libra/libra/edit/master/mempool/README.md
---
# Mempool

Mempool is a memory-buffer that holds the transactions that are waiting to be executed.
1. 内存池是一个缓冲区，用于保证正等待执行的交易
2. 当新交易添加到验证器节点的内存池时，此验证器节点的内存池与系统中其他验证器的内存池共享此交易

1. 接收本地收到的Tx并验证
2. 和其他节点之间互相同步Tx

因为Libra使用的是不会分叉的PBFT，所以缓冲池的实现以及管理要简单许多。

交易主要是seq_number连起来的，同时Libra中也有和以太坊一样的GasPrice概念，因此如果对于同一账号，seq_number相同的情况下，会选择GasPrice高的那个，
可以看出实际上Libra唯一的ID可以不认为是交易数据的哈希值，可以把(Address, seq_number)作为唯一的ID，这个在比特币以太坊等公链中也行得通，
因为Libra中把(Address, seq_number)二元组作为Tx唯一的ID，所以其代码设计中对于Tx的管理和以太坊也不太一样

mempool可以通俗的认为就是一个HashMap<AccountAddress, BTreeMap<u64, MempoolTransaction>>,其中这里的u64就是对应账户的seq_number.其所有
功能都是围绕这个数据结构展开。
## Overview

Admission control (AC) module sends transactions to mempool. Mempool holds the transactions for a period of time, before consensus commits them. When a new transaction is added, mempool shares this transaction with other validators (validator nodes) in the system. Mempool is a “shared mempool,” as transactions between mempools are shared with other validators. This helps maintain a pseudo-global ordering.

When a validator receives a transaction from another mempool, the transaction is ordered when it’s added to the ordered queue of the recipient validator. To reduce network consumption in the shared mempool, each validator is responsible for the delivery of its own transactions. We don't rebroadcast transactions originating from a peer validator.

We only broadcast transactions that have some probability of being included in the next block. This means that either the sequence number of the transaction is the next sequence number of the sender account, or it is sequential to it. For example, if the current sequence number for an account is 2 and local mempool contains transactions with sequence numbers 2, 3, 4, 7, 8, then only transactions 2, 3, and 4 will be broadcast.

The consensus module pulls transactions from mempool, mempool does not push transactions into consensus. This is to ensure that while consensus is not ready for transactions:

* Mempool can continue ordering transactions based on gas; and
* Consensus can allow transactions to build up in the mempool.

This allows transactions to be grouped into a single consensus block, and prioritized by gas price.

Mempool doesn't keep track of transactions sent to consensus. On each get_block request (to pull a block of transaction from mempool), consensus sends a set of transactions that were pulled from mempool, but not committed. This allows the mempool to stay agnostic about different consensus proposal branches.

When a transaction is fully executed and written to storage, consensus notifies mempool. Mempool then drops this transaction from its internal state.

## Implementation Details

Internally, mempool is modeled as `HashMap<AccountAddress, AccountTransactions>` with various indexes built on top of it.

The main index - PriorityIndex is an ordered queue of transactions that are “ready” to be included in the next block (i.e., they have a sequence number which is sequential to the current sequence number for the account). This queue is ordered by gas price so that if a client is willing to pay more (than other clients) per unit of execution, then they can enter consensus earlier.

Note that, even though global ordering is maintained by gas price, for a single account, transactions are ordered by sequence number. All transactions that are not ready to be included in the next block are part of a separate ParkingLotIndex. They are moved to the ordered queue once some event unblocks them.

Here is an example: mempool has a transaction with sequence number 4, while the current sequence number for that account is 3. This transaction is considered “non-ready.” Callback from consensus notifies that transaction was committed (i.e., transaction 3 was submitted to a different node and has hence been committed on chain). This event “unblocks” the local transaction, and transaction #4 is moved to the OrderedQueue.

Mempool only holds a limited number of transactions to avoid overwhelming the system and to prevent abuse and attack. Transactions in Mempool have two types of expirations: systemTTL and client-specified expiration. When either of these is reached, the transaction is removed from Mempool.

SystemTTL is checked periodically in the background, while the expiration specified by the client is checked on every Consensus commit request. We use a separate system TTL to ensure that a transaction doesn’t remain stuck in the Mempool forever, even if Consensus doesn't make progress.

## How is this module organized?
```
    mempool/src
    ├── core_mempool             # main in memory data structure
    ├── proto                    # protobuf definitions for interactions with mempool
    ├── lib.rs
    ├── mempool_service.rs       # gRPC service
    ├── runtime.rs               # bundle of shared mempool and gRPC service
    └── shared_mempool.rs        # shared mempool
```
