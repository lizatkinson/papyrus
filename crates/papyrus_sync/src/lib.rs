#[cfg(test)]
#[path = "sync_test.rs"]
mod sync_test;

mod sources;

use std::sync::Arc;
use std::time::Duration;

use async_stream::stream;
use futures_util::{pin_mut, select, Stream, StreamExt};
use log::{error, info};
use papyrus_storage::{
    BodyStorageReader, BodyStorageWriter, HeaderStorageReader, HeaderStorageWriter,
    OmmerStorageReader, OmmerStorageWriter, StateStorageReader, StateStorageWriter, StorageError,
    StorageReader, StorageWriter, TransactionIndex,
};
use serde::{Deserialize, Serialize};
use starknet_api::block::{Block, BlockHash, BlockNumber};
use starknet_api::core::ClassHash;
use starknet_api::state::{ContractClass, StateDiff};
use starknet_api::transaction::TransactionOffsetInBlock;

pub use self::sources::{CentralError, CentralSource, CentralSourceConfig, CentralSourceTrait};

#[derive(Clone, Copy, Serialize, Deserialize)]
pub struct SyncConfig {
    pub block_propagation_sleep_duration: Duration,
}

// Orchestrates specific network interfaces (e.g. central, p2p, l1) and writes to Storage.
pub struct GenericStateSync<TCentralSource: CentralSourceTrait + Sync + Send> {
    config: SyncConfig,
    central_source: Arc<TCentralSource>,
    reader: StorageReader,
    writer: StorageWriter,
}

pub type StateSyncResult = Result<(), StateSyncError>;

#[derive(thiserror::Error, Debug)]
pub enum StateSyncError {
    #[error(transparent)]
    StorageError(#[from] StorageError),
    #[error(transparent)]
    CentralSourceError(#[from] CentralError),
    #[error(
        "Parent block hash of block {block_number:?} is not consistent with the stored block. \
         Expected {expected_parent_block_hash:?}, found {stored_parent_block_hash:?}."
    )]
    ParentBlockHashMismatch {
        block_number: BlockNumber,
        expected_parent_block_hash: BlockHash,
        stored_parent_block_hash: BlockHash,
    },
    #[error(
        "Received state diff of block {block_number:?} and block hash {block_hash:?}, didn't find \
         a matching header (neither in the ommer headers)."
    )]
    StateDiffWithoutMatchingHeader { block_number: BlockNumber, block_hash: BlockHash },
}

#[allow(clippy::large_enum_variant)]
pub enum SyncEvent {
    BlockAvailable {
        block_number: BlockNumber,
        block: Block,
    },
    StateDiffAvailable {
        block_number: BlockNumber,
        block_hash: BlockHash,
        state_diff: StateDiff,
        // TODO(anatg): Remove once there are no more deployed contracts with undeclared classes.
        // Class definitions of deployed contracts with classes that were not declared in this
        // state diff.
        deployed_contract_class_definitions: Vec<(ClassHash, ContractClass)>,
    },
}

#[allow(clippy::new_without_default)]
impl<TCentralSource: CentralSourceTrait + Sync + Send + 'static> GenericStateSync<TCentralSource> {
    pub async fn run(&mut self) -> StateSyncResult {
        info!("State sync started.");
        loop {
            match self.handle_block_reverts().await {
                Ok(_) => {}
                Err(err) if is_recoverable(&err) => {
                    error!("{}", err);
                    // TODO: change sleep duration.
                    tokio::time::sleep(self.config.block_propagation_sleep_duration).await;
                    continue;
                }
                Err(err) => return Err(err),
            }

            let block_stream = stream_new_blocks(
                self.reader.clone(),
                self.central_source.clone(),
                self.config.block_propagation_sleep_duration,
            )
            .fuse();
            let state_diff_stream = stream_new_state_diffs(
                self.reader.clone(),
                self.central_source.clone(),
                self.config.block_propagation_sleep_duration,
            )
            .fuse();
            pin_mut!(block_stream, state_diff_stream);

            loop {
                let sync_event = match select! {
                  res = block_stream.next() => res,
                  res = state_diff_stream.next() => res,
                  complete => break,
                } {
                    Some(Ok(sync_event)) => sync_event,
                    Some(Err(err)) if is_recoverable(&err) => {
                        error!("{}", err);
                        // TODO: change sleep duration.
                        tokio::time::sleep(self.config.block_propagation_sleep_duration).await;
                        break;
                    }
                    Some(Err(err)) => {
                        error!("{}", err);
                        return Err(err);
                    }
                    None => {
                        unreachable!("Received None as a sync event.");
                    }
                };

                match self.process_sync_event(sync_event).await {
                    Ok(_) => {}
                    Err(StateSyncError::ParentBlockHashMismatch {
                        block_number,
                        expected_parent_block_hash: _,
                        stored_parent_block_hash: _,
                    }) => {
                        // A revert detected, log and restart main sync loop.
                        info!("Detected revert while processing block {}", block_number);
                        break;
                    }
                    // A recoverable error occured, break the loop and create new streams.
                    Err(err) if is_recoverable(&err) => {
                        error!("{}", err);
                        break;
                    }
                    // Unrecoverable errors.
                    Err(err) => return Err(err),
                }
            }
        }

        // Whitelisting of errors from which we might be able to recover.
        fn is_recoverable(_err: &StateSyncError) -> bool {
            false
        }
    }

    // Tries to store the incoming data.
    async fn process_sync_event(&mut self, sync_event: SyncEvent) -> StateSyncResult {
        match sync_event {
            SyncEvent::BlockAvailable { block_number, block } => {
                self.store_block(block_number, block)
            }
            SyncEvent::StateDiffAvailable {
                block_number,
                block_hash,
                state_diff,
                deployed_contract_class_definitions,
            } => {
                if !self.is_reverted_state_diff(block_number, block_hash)? {
                    self.writer
                        .begin_rw_txn()?
                        .append_state_diff(
                            block_number,
                            state_diff,
                            deployed_contract_class_definitions,
                        )?
                        .commit()?;
                } else {
                    self.writer
                        .begin_rw_txn()?
                        .insert_ommer_state_diff(
                            block_hash,
                            &state_diff.into(),
                            &deployed_contract_class_definitions,
                        )?
                        .commit()?;
                }
                Ok(())
            }
        }
    }

    fn store_block(&mut self, block_number: BlockNumber, block: Block) -> StateSyncResult {
        // Assuming the central source is trusted, detect reverts by comparing the incoming block's
        // parent hash to the current hash.
        self.verify_parent_block_hash(block_number, &block)?;

        self.writer
            .begin_rw_txn()?
            .append_header(block_number, &block.header)?
            .append_body(block_number, block.body)?
            .commit()?;
        Ok(())
    }

    // Compares the block's parent hash to the stored block.
    fn verify_parent_block_hash(
        &self,
        block_number: BlockNumber,
        block: &Block,
    ) -> StateSyncResult {
        let prev_block_number = match block_number.prev() {
            None => return Ok(()),
            Some(bn) => bn,
        };
        let prev_hash = self
            .reader
            .begin_ro_txn()?
            .get_block_header(prev_block_number)?
            .ok_or(StorageError::DBInconsistency {
                msg: format!(
                    "Missing block {} in the storage (for verifing block {}).",
                    prev_block_number, block_number
                ),
            })?
            .block_hash;

        if prev_hash != block.header.parent_hash {
            return Err(StateSyncError::ParentBlockHashMismatch {
                block_number,
                expected_parent_block_hash: block.header.parent_hash,
                stored_parent_block_hash: prev_hash,
            });
        }

        Ok(())
    }

    // Reverts data if needed.
    async fn handle_block_reverts(&mut self) -> Result<(), StateSyncError> {
        let header_marker = self.reader.begin_ro_txn()?.get_header_marker()?;

        // Revert last blocks if needed.
        let mut last_block_in_storage = header_marker.prev();
        while let Some(block_number) = last_block_in_storage {
            if self.should_revert_block(block_number).await? {
                self.revert_block(block_number)?;
                last_block_in_storage = block_number.prev();
            } else {
                break;
            }
        }
        Ok(())
    }

    // Deletes the block data from the storage, moving it to the ommer tables.
    #[allow(clippy::expect_fun_call)]
    fn revert_block(&mut self, block_number: BlockNumber) -> StateSyncResult {
        // TODO: Modify revert functions so they return the deleted data, and use it for inserting
        // to the ommer tables.
        let mut txn = self.writer.begin_rw_txn()?;
        let header = txn
            .get_block_header(block_number)?
            .expect(format!("Tried to revert a missing header of block {block_number}").as_str());
        let transactions = txn.get_block_transactions(block_number)?.expect(
            format!("Tried to revert a missing transactions of block {block_number}").as_str(),
        );
        let transaction_outputs = txn.get_block_transaction_outputs(block_number)?.expect(
            format!("Tried to revert a missing transaction outputs of block {}", block_number)
                .as_str(),
        );

        // TODO: use iter_events of EventsReader once it supports RW transactions.
        let mut events: Vec<_> = vec![];
        for idx in 0..transactions.len() {
            let tx_idx = TransactionIndex(block_number, TransactionOffsetInBlock(idx));
            events.push(txn.get_transaction_events(tx_idx)?.unwrap_or_default());
        }

        txn = txn
            .revert_header(block_number)?
            .insert_ommer_header(header.block_hash, &header)?
            .revert_body(block_number)?
            .insert_ommer_body(
                header.block_hash,
                &transactions,
                &transaction_outputs,
                events.as_slice(),
            )?;

        let (txn, maybe_deleted_data) = txn.revert_state_diff(block_number)?;
        if let Some((thin_state_diff, declared_classes)) = maybe_deleted_data {
            txn.insert_ommer_state_diff(header.block_hash, &thin_state_diff, &declared_classes)?
                .commit()?;
        } else {
            txn.commit()?;
        }
        Ok(())
    }

    /// Checks if centrals block hash at the block number is different from ours (or doesn't exist).
    /// If so, a revert is required.
    async fn should_revert_block(&self, block_number: BlockNumber) -> Result<bool, StateSyncError> {
        if let Some(central_block_hash) = self.central_source.get_block_hash(block_number).await? {
            let storage_block_header =
                self.reader.begin_ro_txn()?.get_block_header(block_number)?;

            match storage_block_header {
                Some(block_header) => Ok(block_header.block_hash != central_block_hash),
                None => Ok(false),
            }
        } else {
            // Block number doesn't exist in central, revert.
            Ok(true)
        }
    }

    fn is_reverted_state_diff(
        &self,
        block_number: BlockNumber,
        block_hash: BlockHash,
    ) -> Result<bool, StateSyncError> {
        let txn = self.reader.begin_ro_txn()?;
        let storage_header = txn.get_block_header(block_number)?;
        match storage_header {
            Some(storage_header) if storage_header.block_hash == block_hash => Ok(false),
            _ => {
                // No matching header, check in the ommer headers.
                match txn.get_ommer_header(block_hash)? {
                    Some(_) => Ok(true),
                    None => Err(StateSyncError::StateDiffWithoutMatchingHeader {
                        block_number,
                        block_hash,
                    }),
                }
            }
        }
    }
}

fn stream_new_blocks<TCentralSource: CentralSourceTrait + Sync + Send>(
    reader: StorageReader,
    central_source: Arc<TCentralSource>,
    block_propation_sleep_duration: Duration,
) -> impl Stream<Item = Result<SyncEvent, StateSyncError>> {
    stream! {
        loop {
            let header_marker = reader.begin_ro_txn()?
            .get_header_marker()?;

            let last_block_number = central_source
                .get_block_marker()
                .await?;

            info!(
                "Downloading blocks [{} - {}).",
                header_marker, last_block_number
            );
            if header_marker == last_block_number {
                tokio::time::sleep(block_propation_sleep_duration).await;
                continue;
            }
            let block_stream = central_source
                .stream_new_blocks(header_marker, last_block_number)
                .fuse();
            pin_mut!(block_stream);
            while let Some(Ok((block_number, block))) = block_stream.next().await {
                yield Ok(SyncEvent::BlockAvailable { block_number, block });
            }
        }
    }
}

fn stream_new_state_diffs<TCentralSource: CentralSourceTrait + Sync + Send>(
    reader: StorageReader,
    central_source: Arc<TCentralSource>,
    block_propation_sleep_duration: Duration,
) -> impl Stream<Item = Result<SyncEvent, StateSyncError>> {
    stream! {
        loop {
            let txn = reader.begin_ro_txn()?;
            let state_marker = txn.get_state_marker()?;
            let last_block_number = txn.get_header_marker()?;
            drop(txn);
            info!(
                "Downloading state diffs [{} - {}).",
                state_marker, last_block_number
            );
            if state_marker == last_block_number {
                tokio::time::sleep(block_propation_sleep_duration).await;
                continue;
            }
            let state_diff_stream = central_source
                .stream_state_updates(state_marker, last_block_number)
                .fuse();
            pin_mut!(state_diff_stream);
            while let Some(maybe_state_diff) = state_diff_stream.next().await {
                match maybe_state_diff {
                    Ok((block_number, block_hash, mut state_diff, deployed_contract_class_definitions)) => {
                        sort_state_diff(&mut state_diff);
                        yield Ok(SyncEvent::StateDiffAvailable {
                            block_number,
                            block_hash,
                            state_diff,
                            deployed_contract_class_definitions,
                        })
                    }
                    Err(err) => {
                        error!("{}", err);
                        break;
                    }
                }
            }
        }
    }
}

pub fn sort_state_diff(diff: &mut StateDiff) {
    diff.declared_classes.sort_unstable_keys();
    diff.deployed_contracts.sort_unstable_keys();
    diff.nonces.sort_unstable_keys();
    diff.storage_diffs.sort_unstable_keys();
    for storage_entries in diff.storage_diffs.values_mut() {
        storage_entries.sort_unstable_keys();
    }
}

pub type StateSync = GenericStateSync<CentralSource>;

impl StateSync {
    pub fn new(
        config: SyncConfig,
        central_source: CentralSource,
        reader: StorageReader,
        writer: StorageWriter,
    ) -> Self {
        Self { config, central_source: Arc::new(central_source), reader, writer }
    }
}
