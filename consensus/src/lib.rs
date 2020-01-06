// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

#![forbid(unsafe_code)]

//! Consensus for the Libra Core blockchain
//!
//! Encapsulates public consensus traits and any implementations of those traits.
//! Currently, the only consensus protocol supported is LibraBFT (based on
//! [HotStuff](https://arxiv.org/pdf/1803.05069.pdf)).

#![cfg_attr(not(feature = "fuzzing"), deny(missing_docs))]
#![cfg_attr(feature = "fuzzing", allow(dead_code))]
#![recursion_limit = "512"]

#[macro_use]
extern crate prometheus;

mod chained_bft; // LibraBFT的具体实现模块

mod util;

#[cfg(feature = "fuzzing")]
pub use chained_bft::event_processor_fuzzing;

/// Defines the public consensus provider traits to implement for
/// use in the Libra Core blockchain.
pub mod consensus_provider; // 实现了Libra共识的接口，pub关键字声明该接口可以被外界访问

mod counters;

mod state_computer; // 状态计算模块，与执行模块相互通信，实现状态计算接口
mod state_replication; // 状态复制模块，定义了SMR接口和StateComputer接口
mod txn_manager;
