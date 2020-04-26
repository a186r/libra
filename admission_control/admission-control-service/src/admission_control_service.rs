// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

//! Admission Control (AC) is a module acting as the only public end point. It receives api requests
//! from external clients (such as wallets) and performs necessary processing before sending them to
//! next step.

use admission_control_proto::proto::admission_control::{
    admission_control_server::AdmissionControl, SubmitTransactionRequest, SubmitTransactionResponse,
};
use anyhow::Result;
use futures::{
    channel::{mpsc, oneshot},
    SinkExt,
};
use libra_logger::prelude::*;
use libra_types::proto::types::{UpdateToLatestLedgerRequest, UpdateToLatestLedgerResponse};
use std::convert::TryFrom;
use std::sync::Arc;
use storage_client::StorageRead;
use admission_control_proto::SubmitTransactionResponse;
use libra_types::get_with_proof::{UpdateToLatestLedgerRequest, UpdateToLatestLedgerResponse};

/// Struct implementing trait (service handle) AdmissionControlService.
#[derive(Clone)]
pub struct AdmissionControlService {
    ac_sender: mpsc::Sender<(
        SubmitTransactionRequest,
        oneshot::Sender<Result<SubmitTransactionResponse>>,
    )>,
    /// gRPC client to send read requests to Storage.
    storage_read_client: Arc<dyn StorageRead>,
}

impl AdmissionControlService {
    /// Constructs a new AdmissionControlService instance.
    pub fn new(
        ac_sender: mpsc::Sender<(
            SubmitTransactionRequest,
            oneshot::Sender<Result<SubmitTransactionResponse>>,
        )>,
        storage_read_client: Arc<dyn StorageRead>,
    ) -> Self {
        AdmissionControlService {
            ac_sender,
            storage_read_client,
        }
    }

    /// Pass the UpdateToLatestLedgerRequest to Storage for read query.
    async fn update_to_latest_ledger_inner(
        &self,
        req: UpdateToLatestLedgerRequest,
    ) -> Result<UpdateToLatestLedgerResponse> {
        let rust_req = libra_types::get_with_proof::UpdateToLatestLedgerRequest::try_from(req)?;
        let (
            response_items,
            ledger_info_with_sigs,
            validator_change_proof,
            ledger_consistency_proof,
        ) = self
            .storage_read_client
            .update_to_latest_ledger_async(rust_req.client_known_version, rust_req.requested_items)
            .await?;
        let rust_resp = libra_types::get_with_proof::UpdateToLatestLedgerResponse::new(
            response_items,
            ledger_info_with_sigs,
            validator_change_proof,
            ledger_consistency_proof,
        );
        Ok(rust_resp.into())
    }
}

/// AC基本就干了两件事，提供了两个接口，一个是submit_transaction，另一个是update_to_latest_ledger
#[tonic::async_trait]
impl AdmissionControl for AdmissionControlService {
    /// Submit a transaction to the validator this AC instance connecting to.
    /// The specific transaction will be first validated by VM and then passed
    /// to Mempool for further processing.
    /// 这个模块主要是接受来自用户的Tx，如果合理有效则提交给mempool模块，最终会进入block中
    /// 工作流程如下：
    /// 1.校验Tx，包括三个部分，一个是签名是否有效，另一个则是gas是否有效，第三个是执行Tx中的code是否能够通过
    /// 2.校验账户余额是否足够，然后通过grpc链接发送给mempool模块
    /// 3.将mempool结果返回给用户
    async fn submit_transaction(
        &self,
        request: tonic::Request<SubmitTransactionRequest>,
    ) -> Result<tonic::Response<SubmitTransactionResponse>, tonic::Status> {
        debug!("[GRPC] AdmissionControl::submit_transaction");
        let req = request.into_inner();

        let (req_sender, res_receiver) = oneshot::channel();
        self.ac_sender
            .clone()
            .send((req, req_sender))
            .await
            .map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Internal,
                    format!(
                        "[admission-control] Failed to submit write request with error: {:?}",
                        e
                    ),
                )
            })?;

        let resp = res_receiver.await.unwrap().map_err(|e| {
            tonic::Status::new(
                tonic::Code::Internal,
                format!(
                    "[admission-control] Submitting transaction failed with error: {:?}",
                    e
                ),
            )
        })?;

        Ok(tonic::Response::new(resp))
    }

    /// This API is used to update the client to the latest ledger version and optionally also
    /// request 1..n other pieces of data.  This allows for batch queries.  All queries return
    /// proofs that a client should check to validate the data.
    /// Note that if a client only wishes to update to the latest LedgerInfo and receive the proof
    /// of this latest version, they can simply omit the requested_items (or pass an empty list).
    /// AC will not directly process this request but pass it to Storage instead.
    async fn update_to_latest_ledger(
        &self,
        request: tonic::Request<UpdateToLatestLedgerRequest>,
    ) -> Result<tonic::Response<UpdateToLatestLedgerResponse>, tonic::Status> {
        debug!("[GRPC] AdmissionControl::update_to_latest_ledger");
        let req = request.into_inner();
        let resp = self
            .update_to_latest_ledger_inner(req)
            .await
            .map_err(|e| tonic::Status::new(tonic::Code::InvalidArgument, e.to_string()))?;
        Ok(tonic::Response::new(resp))
    }
}
