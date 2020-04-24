// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

//! This file defines transaction store APIs that are related to committed signed transactions.

use crate::schema::transaction::TransactionSchema;
use crate::{
    change_set::ChangeSet, errors::LibraDbError,
    schema::transaction_by_account::TransactionByAccountSchema,
};
use anyhow::Result;
use libra_types::{
    account_address::AccountAddress,
    transaction::{Transaction, Version},
};
use schemadb::DB;
use std::sync::Arc;

/**
    mempool模块对于Tx的管理核心都集中在TransactionStore这个结构，他对外对接的是CoreMemPool结构，从TransactionStore
    可以清楚的看出缓冲池中Tx增删改查的逻辑
*/

pub(crate) struct TransactionStore {
    db: Arc<DB>,
}

impl TransactionStore {
    pub fn new(db: Arc<DB>) -> Self {
        Self { db }
    }

    /// Gets the version of a transaction by the sender `address` and `sequence_number`.
    pub fn lookup_transaction_by_account(
        &self,
        address: AccountAddress,
        sequence_number: u64,
        ledger_version: Version,
    ) -> Result<Option<Version>> {
        if let Some(version) = self
            .db
            .get::<TransactionByAccountSchema>(&(address, sequence_number))?
        {
            if version <= ledger_version {
                return Ok(Some(version));
            }
        }

        Ok(None)
    }

    /// Get signed transaction given `version`
    pub fn get_transaction(&self, version: Version) -> Result<Transaction> {
        self.db
            .get::<TransactionSchema>(&version)?
            .ok_or_else(|| LibraDbError::NotFound(format!("Txn {}", version)).into())
    }

    /// Save signed transaction at `version`
    pub fn put_transaction(
        &self,
        version: Version,
        transaction: &Transaction,
        cs: &mut ChangeSet,
    ) -> Result<()> {
        if let Transaction::UserTransaction(txn) = transaction {
            cs.batch.put::<TransactionByAccountSchema>(
                &(txn.sender(), txn.sequence_number()),
                &version,
            )?;
        }
        cs.batch.put::<TransactionSchema>(&version, &transaction)?;

        Ok(())
    }
}

#[cfg(test)]
mod test;
