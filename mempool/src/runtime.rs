// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    core_mempool::CoreMempool, mempool_service::MempoolService, proto::mempool,
    shared_mempool::start_shared_mempool,
};
use grpcio::EnvBuilder;
use libra_config::config::NodeConfig;
use network::validator_network::{MempoolNetworkEvents, MempoolNetworkSender};
use std::{
    net::ToSocketAddrs,
    sync::{Arc, Mutex},
};
use storage_client::{StorageRead, StorageReadServiceClient};
use tokio::runtime::{Builder, Runtime};
use vm_validator::vm_validator::VMValidator;

/// Handle for Mempool Runtime
/// 内部使用了tokio的异步编程框架
pub struct MempoolRuntime {
    /// mempool service runtime
    pub mempool_service_rt: Runtime,
    /// separate shared mempool runtime
    pub shared_mempool: Runtime,
}

impl MempoolRuntime {
    /// setup Mempool runtime
    ///
    pub fn bootstrap(
        config: &NodeConfig,
        network_sender: MempoolNetworkSender,
        network_events: MempoolNetworkEvents,
    ) -> Self {
        let mempool_service_rt = Builder::new()
            .thread_name("mempool-service-")
            .threaded_scheduler()
            .enable_all()
            .build()
            .unwrap();
        // 访问是加锁的，这个mempool就是内部缓冲池管理模块
        let mempool = Arc::new(Mutex::new(CoreMempool::new(&config)));
        let mempool_service = MempoolService {
            core_mempool: Arc::clone(&mempool),
        };
        // setup shared mempool
        // mempool要访问DB，也是通过grpc接口访问，没有直接访问DB
        let storage_client: Arc<dyn StorageRead> = Arc::new(StorageReadServiceClient::new(
            Arc::new(EnvBuilder::new().name_prefix("grpc-mem-sto-").build()),
            "localhost",
            config.storage.port,
        ));

        // 验证Tx合法性的工具
        let vm_validator = Arc::new(VMValidator::new(
            &config,
            Arc::clone(&storage_client),
            mempool_service_rt.handle().clone(),
        ));

        // 如何与其他节点之间进行交互全发生在SharedMempool
        // 这里实际上就返回了一个Runtion，这时候tokio的调度器已经启动完成了
        let shared_mempool = start_shared_mempool(
            config,
            mempool,
            network_sender,
            network_events,
            storage_client,
            vm_validator,
            vec![],
            None,
        );

        let addr = format!(
            "{}:{}",
            config.mempool.address, config.mempool.mempool_service_port,
        )
        .to_socket_addrs()
        .unwrap()
        .next()
        .unwrap();
        mempool_service_rt.spawn(
            tonic::transport::Server::builder()
                .add_service(mempool::mempool_server::MempoolServer::new(mempool_service))
                .serve(addr),
        );

        Self {
            mempool_service_rt,
            shared_mempool,
        }
    }
}
