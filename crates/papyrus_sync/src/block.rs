use std::sync::Arc;

use futures_util::{pin_mut, StreamExt};
use papyrus_storage::body::{BodyStorageReader, BodyStorageWriter};
use papyrus_storage::db::RW;
use papyrus_storage::header::{HeaderStorageReader, HeaderStorageWriter};
use papyrus_storage::ommer::OmmerStorageWriter;
use papyrus_storage::state::StateStorageWriter;
use papyrus_storage::{StorageReader, StorageTxn, TransactionIndex};
use starknet_api::block::{Block, BlockNumber};
use starknet_api::transaction::TransactionOffsetInBlock;
use tokio::sync::mpsc;
use tracing::{debug, info, trace};

use crate::sources::CentralSourceTrait;
use crate::{StateSyncError, StateSyncResult, SyncConfig, SyncEvent};

pub struct BlockSync<TCentralSource: CentralSourceTrait + Sync + Send> {
    pub config: SyncConfig,
    pub central_source: Arc<TCentralSource>,
    pub reader: StorageReader,
    pub sender: mpsc::Sender<SyncEvent>,
}

pub async fn run_block_sync<TCentralSource: CentralSourceTrait + Sync + Send>(
    config: SyncConfig,
    central_source: Arc<TCentralSource>,
    reader: StorageReader,
    sender: mpsc::Sender<SyncEvent>,
) -> StateSyncResult {
    let block_sync = BlockSync { config, central_source, reader, sender };
    block_sync.stream_new_blocks().await
}

pub(crate) async fn store_block<TCentralSource: CentralSourceTrait + Sync + Send>(
    reader: StorageReader,
    txn: StorageTxn<'_, RW>,
    block_number: BlockNumber,
    block: Block,
    central_source: Arc<TCentralSource>,
) -> StateSyncResult {
    trace!("Block data: {block:#?}");
    if verify_parent_block_hash(reader.clone(), block_number, &block)? {
        handle_block_reverts(reader, txn, central_source).await?;
        return Ok(());
    }

    if let Ok(txn) = txn.append_header(block_number, &block.header) {
        debug!("Storing block {block_number} with hash {}.", block.header.block_hash);
        txn.append_body(block_number, block.body)?.commit()?;
    }
    Ok(())
}

// Compares the block's parent hash to the stored block.
fn verify_parent_block_hash(
    reader: StorageReader,
    block_number: BlockNumber,
    block: &Block,
) -> Result<bool, StateSyncError> {
    let prev_block_number = match block_number.prev() {
        None => return Ok(false),
        Some(bn) => bn,
    };
    let prev_header = reader.begin_ro_txn()?.get_block_header(prev_block_number)?;
    match prev_header {
        Some(prev_header) if prev_header.block_hash == block.header.parent_hash => Ok(false),
        _ => Ok(true),
    }
}

// Reverts data if needed.
pub(crate) async fn handle_block_reverts<TCentralSource: CentralSourceTrait + Sync + Send>(
    reader: StorageReader,
    txn: StorageTxn<'_, RW>,
    central_source: Arc<TCentralSource>,
) -> Result<(), StateSyncError> {
    debug!("Handling block reverts.");
    let header_marker = reader.begin_ro_txn()?.get_header_marker()?;

    // Revert last blocks if needed.
    let last_block_in_storage = header_marker.prev();
    // while let Some(block_number) = last_block_in_storage {
    if let Some(block_number) = last_block_in_storage {
        if should_revert_block(reader, central_source, block_number).await? {
            info!("Reverting block {}.", block_number);
            revert_block(txn, block_number)?;
            // last_block_in_storage = block_number.prev();
        }
        // } else {
        //     break;
        // }
    }
    Ok(())
}

// Deletes the block data from the storage, moving it to the ommer tables.
#[allow(clippy::expect_fun_call)]
fn revert_block(mut txn: StorageTxn<'_, RW>, block_number: BlockNumber) -> StateSyncResult {
    // TODO: Modify revert functions so they return the deleted data, and use it for inserting
    // to the ommer tables.
    let header = txn
        .get_block_header(block_number)?
        .expect(format!("Tried to revert a missing header of block {block_number}").as_str());
    let transactions = txn
        .get_block_transactions(block_number)?
        .expect(format!("Tried to revert a missing transactions of block {block_number}").as_str());
    let transaction_outputs = txn.get_block_transaction_outputs(block_number)?.expect(
        format!("Tried to revert a missing transaction outputs of block {block_number}").as_str(),
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
async fn should_revert_block<TCentralSource: CentralSourceTrait + Sync + Send>(
    reader: StorageReader,
    central_source: Arc<TCentralSource>,
    block_number: BlockNumber,
) -> Result<bool, StateSyncError> {
    if let Some(central_block_hash) = central_source.get_block_hash(block_number).await? {
        let storage_block_header = reader.begin_ro_txn()?.get_block_header(block_number)?;

        match storage_block_header {
            Some(block_header) => Ok(block_header.block_hash != central_block_hash),
            None => Ok(false),
        }
    } else {
        // Block number doesn't exist in central, revert.
        Ok(true)
    }
}

impl<TCentralSource: CentralSourceTrait + Sync + Send> BlockSync<TCentralSource> {
    async fn stream_new_blocks(&self) -> StateSyncResult {
        let header_marker = self.reader.begin_ro_txn()?.get_header_marker()?;
        let last_block_number = self.central_source.get_block_marker().await?;
        if header_marker == last_block_number {
            debug!("Waiting for more blocks.");
            tokio::time::sleep(self.config.block_propagation_sleep_duration).await;
            return Ok(());
        }

        debug!("Downloading blocks [{} - {}).", header_marker, last_block_number);
        let block_stream =
            self.central_source.stream_new_blocks(header_marker, last_block_number).fuse();
        pin_mut!(block_stream);

        while let Some(maybe_block) = block_stream.next().await {
            let (block_number, block) = maybe_block?;
            self.sender
                .send(SyncEvent::BlockAvailable { block_number, block: block.clone() })
                .await?;
            if verify_parent_block_hash_if_exists(self.reader.clone(), block_number, &block)? {
                debug!("Waiting for blocks to revert.");
                tokio::time::sleep(self.config.recoverable_error_sleep_duration).await;
                break;
            }
        }

        Ok(())
    }
}

fn verify_parent_block_hash_if_exists(
    reader: StorageReader,
    block_number: BlockNumber,
    block: &Block,
) -> Result<bool, StateSyncError> {
    let prev_block_number = match block_number.prev() {
        None => return Ok(false),
        Some(bn) => bn,
    };
    let prev_header = reader.begin_ro_txn()?.get_block_header(prev_block_number)?;
    match prev_header {
        Some(prev_header) if prev_header.block_hash != block.header.parent_hash => {
            debug!("Detected a possible revert while processing block {block_number}.");
            Ok(true)
        }
        _ => Ok(false),
    }
}
