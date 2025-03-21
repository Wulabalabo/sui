// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

mod causal_order;
pub mod checkpoint_executor;
mod checkpoint_output;
mod metrics;

use crate::authority::{AuthorityState, EffectsNotifyRead};
use crate::authority_client::{make_network_authority_clients_with_network_config, AuthorityAPI};
use crate::checkpoints::causal_order::CausalOrder;
use crate::checkpoints::checkpoint_output::{CertifiedCheckpointOutput, CheckpointOutput};
pub use crate::checkpoints::checkpoint_output::{
    LogCheckpointOutput, SendCheckpointToStateSync, SubmitCheckpointToConsensus,
};
pub use crate::checkpoints::metrics::CheckpointMetrics;
use crate::stake_aggregator::{InsertResult, MultiStakeAggregator};
use crate::state_accumulator::StateAccumulator;
use diffy::create_patch;
use futures::future::{select, Either};
use futures::FutureExt;
use itertools::Itertools;
use mysten_metrics::{monitored_scope, spawn_monitored_task, MonitoredFutureExt};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sui_macros::fail_point;
use sui_network::default_mysten_network_config;
use sui_types::base_types::ConciseableName;

use crate::authority::authority_per_epoch_store::AuthorityPerEpochStore;
use crate::consensus_handler::SequencedConsensusTransactionKey;
use chrono::Utc;
use rand::rngs::OsRng;
use rand::seq::SliceRandom;
use std::collections::BTreeMap;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use sui_protocol_config::ProtocolVersion;
use sui_types::base_types::{AuthorityName, EpochId, TransactionDigest};
use sui_types::committee::StakeUnit;
use sui_types::crypto::AuthorityStrongQuorumSignInfo;
use sui_types::digests::{CheckpointContentsDigest, CheckpointDigest};
use sui_types::effects::{TransactionEffects, TransactionEffectsAPI};
use sui_types::error::{SuiError, SuiResult};
use sui_types::gas::GasCostSummary;
use sui_types::message_envelope::Message;
use sui_types::messages_checkpoint::{
    CertifiedCheckpointSummary, CheckpointContents, CheckpointResponseV2, CheckpointSequenceNumber,
    CheckpointSignatureMessage, CheckpointSummary, CheckpointSummaryResponse, CheckpointTimestamp,
    EndOfEpochData, FullCheckpointContents, TrustedCheckpoint, VerifiedCheckpoint,
    VerifiedCheckpointContents,
};
use sui_types::messages_checkpoint::{CheckpointRequestV2, SignedCheckpointSummary};
use sui_types::messages_consensus::ConsensusTransactionKey;
use sui_types::signature::GenericSignature;
use sui_types::sui_system_state::{SuiSystemState, SuiSystemStateTrait};
use sui_types::transaction::{TransactionDataAPI, TransactionKind};
use tokio::{
    sync::{watch, Notify},
    time::timeout,
};
use tracing::{debug, error, info, instrument, warn};
use typed_store::traits::{TableSummary, TypedStoreDebug};
use typed_store::Map;
use typed_store::{
    rocks::{DBMap, MetricConf},
    TypedStoreError,
};
use typed_store_derive::DBMapUtils;

pub type CheckpointCommitHeight = u64;

pub struct EpochStats {
    pub checkpoint_count: u64,
    pub transaction_count: u64,
    pub total_gas_reward: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingCheckpointInfo {
    pub timestamp_ms: CheckpointTimestamp,
    pub last_of_epoch: bool,
    pub commit_height: CheckpointCommitHeight,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingCheckpoint {
    pub roots: Vec<TransactionDigest>,
    pub details: PendingCheckpointInfo,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BuilderCheckpointSummary {
    pub summary: CheckpointSummary,
    // Commit form which this checkpoint summary was built. None for genesis checkpoint
    pub commit_height: Option<CheckpointCommitHeight>,
    pub position_in_commit: usize,
}

#[derive(DBMapUtils)]
pub struct CheckpointStore {
    /// Maps checkpoint contents digest to checkpoint contents
    pub(crate) checkpoint_content: DBMap<CheckpointContentsDigest, CheckpointContents>,

    /// Maps checkpoint contents digest to checkpoint sequence number
    pub(crate) checkpoint_sequence_by_contents_digest:
        DBMap<CheckpointContentsDigest, CheckpointSequenceNumber>,

    /// Stores entire checkpoint contents from state sync, indexed by sequence number, for
    /// efficient reads of full checkpoints. Entries from this table are deleted after state
    /// accumulation has completed.
    full_checkpoint_content: DBMap<CheckpointSequenceNumber, FullCheckpointContents>,

    /// Stores certified checkpoints
    pub(crate) certified_checkpoints: DBMap<CheckpointSequenceNumber, TrustedCheckpoint>,
    /// Map from checkpoint digest to certified checkpoint
    pub(crate) checkpoint_by_digest: DBMap<CheckpointDigest, TrustedCheckpoint>,

    /// Store locally computed checkpoint summaries so that we can detect forks and log useful
    /// information. Can be pruned as soon as we verify that we are in agreement with the latest
    /// certified checkpoint.
    pub(crate) locally_computed_checkpoints: DBMap<CheckpointSequenceNumber, CheckpointSummary>,

    /// A map from epoch ID to the sequence number of the last checkpoint in that epoch.
    epoch_last_checkpoint_map: DBMap<EpochId, CheckpointSequenceNumber>,

    /// Watermarks used to determine the highest verified, fully synced, and
    /// fully executed checkpoints
    pub(crate) watermarks: DBMap<CheckpointWatermark, (CheckpointSequenceNumber, CheckpointDigest)>,
}

impl CheckpointStore {
    pub fn new(path: &Path) -> Arc<Self> {
        Arc::new(Self::open_tables_read_write(
            path.to_path_buf(),
            MetricConf::new("checkpoint"),
            None,
            None,
        ))
    }

    pub fn open_readonly(path: &Path) -> CheckpointStoreReadOnly {
        Self::get_read_only_handle(
            path.to_path_buf(),
            None,
            None,
            MetricConf::new("checkpoint_readonly"),
        )
    }

    pub fn insert_genesis_checkpoint(
        &self,
        checkpoint: VerifiedCheckpoint,
        contents: CheckpointContents,
        epoch_store: &AuthorityPerEpochStore,
    ) {
        assert_eq!(
            checkpoint.epoch(),
            0,
            "can't call insert_genesis_checkpoint with a checkpoint not in epoch 0"
        );
        assert_eq!(
            *checkpoint.sequence_number(),
            0,
            "can't call insert_genesis_checkpoint with a checkpoint that doesn't have a sequence number of 0"
        );

        // Only insert the genesis checkpoint if the DB is empty and doesn't have it already
        if self
            .get_checkpoint_by_digest(checkpoint.digest())
            .unwrap()
            .is_none()
        {
            if epoch_store.epoch() == checkpoint.epoch {
                epoch_store
                    .put_genesis_checkpoint_in_builder(checkpoint.data(), &contents)
                    .unwrap();
            } else {
                debug!(
                    validator_epoch =% epoch_store.epoch(),
                    genesis_epoch =% checkpoint.epoch(),
                    "Not inserting checkpoint builder data for genesis checkpoint",
                );
            }
            self.insert_checkpoint_contents(contents).unwrap();
            self.insert_verified_checkpoint(&checkpoint).unwrap();
            self.update_highest_synced_checkpoint(&checkpoint).unwrap();
        }
    }

    pub fn get_checkpoint_by_digest(
        &self,
        digest: &CheckpointDigest,
    ) -> Result<Option<VerifiedCheckpoint>, TypedStoreError> {
        self.checkpoint_by_digest
            .get(digest)
            .map(|maybe_checkpoint| maybe_checkpoint.map(|c| c.into()))
    }

    pub fn get_checkpoint_by_sequence_number(
        &self,
        sequence_number: CheckpointSequenceNumber,
    ) -> Result<Option<VerifiedCheckpoint>, TypedStoreError> {
        self.certified_checkpoints
            .get(&sequence_number)
            .map(|maybe_checkpoint| maybe_checkpoint.map(|c| c.into()))
    }

    pub fn get_locally_computed_checkpoint(
        &self,
        sequence_number: CheckpointSequenceNumber,
    ) -> Result<Option<CheckpointSummary>, TypedStoreError> {
        self.locally_computed_checkpoints.get(&sequence_number)
    }

    pub fn get_sequence_number_by_contents_digest(
        &self,
        digest: &CheckpointContentsDigest,
    ) -> Result<Option<CheckpointSequenceNumber>, TypedStoreError> {
        self.checkpoint_sequence_by_contents_digest.get(digest)
    }

    pub fn delete_contents_digest_sequence_number_mapping(
        &self,
        digest: &CheckpointContentsDigest,
    ) -> Result<(), TypedStoreError> {
        self.checkpoint_sequence_by_contents_digest.remove(digest)
    }

    pub fn get_latest_certified_checkpoint(&self) -> Option<VerifiedCheckpoint> {
        self.certified_checkpoints
            .unbounded_iter()
            .skip_to_last()
            .next()
            .map(|(_, v)| v.into())
    }

    pub fn get_latest_locally_computed_checkpoint(&self) -> Option<CheckpointSummary> {
        self.locally_computed_checkpoints
            .unbounded_iter()
            .skip_to_last()
            .next()
            .map(|(_, v)| v)
    }

    pub fn multi_get_checkpoint_by_sequence_number(
        &self,
        sequence_numbers: &[CheckpointSequenceNumber],
    ) -> Result<Vec<Option<VerifiedCheckpoint>>, TypedStoreError> {
        let checkpoints = self
            .certified_checkpoints
            .multi_get(sequence_numbers)?
            .into_iter()
            .map(|maybe_checkpoint| maybe_checkpoint.map(|c| c.into()))
            .collect();

        Ok(checkpoints)
    }

    pub fn multi_get_checkpoint_content(
        &self,
        contents_digest: &[CheckpointContentsDigest],
    ) -> Result<Vec<Option<CheckpointContents>>, TypedStoreError> {
        self.checkpoint_content.multi_get(contents_digest)
    }

    pub fn get_highest_verified_checkpoint(
        &self,
    ) -> Result<Option<VerifiedCheckpoint>, TypedStoreError> {
        let highest_verified = if let Some(highest_verified) =
            self.watermarks.get(&CheckpointWatermark::HighestVerified)?
        {
            highest_verified
        } else {
            return Ok(None);
        };
        self.get_checkpoint_by_digest(&highest_verified.1)
    }

    pub fn get_highest_synced_checkpoint(
        &self,
    ) -> Result<Option<VerifiedCheckpoint>, TypedStoreError> {
        let highest_synced = if let Some(highest_synced) =
            self.watermarks.get(&CheckpointWatermark::HighestSynced)?
        {
            highest_synced
        } else {
            return Ok(None);
        };
        self.get_checkpoint_by_digest(&highest_synced.1)
    }

    pub fn get_highest_executed_checkpoint_seq_number(
        &self,
    ) -> Result<Option<CheckpointSequenceNumber>, TypedStoreError> {
        if let Some(highest_executed) =
            self.watermarks.get(&CheckpointWatermark::HighestExecuted)?
        {
            Ok(Some(highest_executed.0))
        } else {
            Ok(None)
        }
    }

    pub fn get_highest_executed_checkpoint(
        &self,
    ) -> Result<Option<VerifiedCheckpoint>, TypedStoreError> {
        let highest_executed = if let Some(highest_executed) =
            self.watermarks.get(&CheckpointWatermark::HighestExecuted)?
        {
            highest_executed
        } else {
            return Ok(None);
        };
        self.get_checkpoint_by_digest(&highest_executed.1)
    }

    pub fn get_highest_pruned_checkpoint_seq_number(
        &self,
    ) -> Result<CheckpointSequenceNumber, TypedStoreError> {
        Ok(self
            .watermarks
            .get(&CheckpointWatermark::HighestPruned)?
            .unwrap_or_default()
            .0)
    }

    pub fn get_checkpoint_contents(
        &self,
        digest: &CheckpointContentsDigest,
    ) -> Result<Option<CheckpointContents>, TypedStoreError> {
        self.checkpoint_content.get(digest)
    }

    pub fn get_full_checkpoint_contents_by_sequence_number(
        &self,
        seq: CheckpointSequenceNumber,
    ) -> Result<Option<FullCheckpointContents>, TypedStoreError> {
        self.full_checkpoint_content.get(&seq)
    }

    fn prune_local_summaries(&self) -> SuiResult {
        if let Some((last_local_summary, _)) = self
            .locally_computed_checkpoints
            .unbounded_iter()
            .skip_to_last()
            .next()
        {
            let mut batch = self.locally_computed_checkpoints.batch();
            batch.schedule_delete_range(
                &self.locally_computed_checkpoints,
                &0,
                &last_local_summary,
            )?;
            batch.write()?;
            info!("Pruned local summaries up to {:?}", last_local_summary);
        }
        Ok(())
    }

    fn check_for_checkpoint_fork(
        &self,
        local_checkpoint: &CheckpointSummary,
        verified_checkpoint: &VerifiedCheckpoint,
    ) {
        if local_checkpoint != verified_checkpoint.data() {
            let verified_contents = self
                .get_checkpoint_contents(&verified_checkpoint.content_digest)
                .map(|opt_contents| {
                    opt_contents
                        .map(|contents| format!("{:?}", contents))
                        .unwrap_or_else(|| {
                            format!(
                                "Verified checkpoint contents not found, digest: {:?}",
                                verified_checkpoint.content_digest,
                            )
                        })
                })
                .map_err(|e| {
                    format!(
                        "Failed to get verified checkpoint contents, digest: {:?} error: {:?}",
                        verified_checkpoint.content_digest, e
                    )
                })
                .unwrap_or_else(|err_msg| err_msg);

            let local_contents = self
                .get_checkpoint_contents(&local_checkpoint.content_digest)
                .map(|opt_contents| {
                    opt_contents
                        .map(|contents| format!("{:?}", contents))
                        .unwrap_or_else(|| {
                            format!(
                                "Local checkpoint contents not found, digest: {:?}",
                                local_checkpoint.content_digest
                            )
                        })
                })
                .map_err(|e| {
                    format!(
                        "Failed to get local checkpoint contents, digest: {:?} error: {:?}",
                        local_checkpoint.content_digest, e
                    )
                })
                .unwrap_or_else(|err_msg| err_msg);

            // checkpoint contents may be too large for panic message.
            error!(
                verified_checkpoint = ?verified_checkpoint.data(),
                ?verified_contents,
                ?local_checkpoint,
                ?local_contents,
                "Local checkpoint fork detected!",
            );
            panic!(
                "Local checkpoint fork detected for sequence number: {}",
                local_checkpoint.sequence_number()
            );
        }
    }

    // Called by consensus (ConsensusAggregator).
    // Different from `insert_verified_checkpoint`, it does not touch
    // the highest_verified_checkpoint watermark such that state sync
    // will have a chance to process this checkpoint and perform some
    // state-sync only things.
    pub fn insert_certified_checkpoint(
        &self,
        checkpoint: &VerifiedCheckpoint,
    ) -> Result<(), TypedStoreError> {
        let mut batch = self.certified_checkpoints.batch();
        batch
            .insert_batch(
                &self.certified_checkpoints,
                [(checkpoint.sequence_number(), checkpoint.serializable_ref())],
            )?
            .insert_batch(
                &self.checkpoint_by_digest,
                [(checkpoint.digest(), checkpoint.serializable_ref())],
            )?;
        if checkpoint.next_epoch_committee().is_some() {
            batch.insert_batch(
                &self.epoch_last_checkpoint_map,
                [(&checkpoint.epoch(), checkpoint.sequence_number())],
            )?;
        }
        batch.write()?;

        if let Some(local_checkpoint) = self
            .locally_computed_checkpoints
            .get(checkpoint.sequence_number())?
        {
            self.check_for_checkpoint_fork(&local_checkpoint, checkpoint);
        }

        Ok(())
    }

    // Called by state sync, apart from inserting the checkpoint and updating
    // related tables, it also bumps the highest_verified_checkpoint watermark.
    pub fn insert_verified_checkpoint(
        &self,
        checkpoint: &VerifiedCheckpoint,
    ) -> Result<(), TypedStoreError> {
        self.insert_certified_checkpoint(checkpoint)?;
        self.update_highest_verified_checkpoint(checkpoint)
    }

    pub fn update_highest_verified_checkpoint(
        &self,
        checkpoint: &VerifiedCheckpoint,
    ) -> Result<(), TypedStoreError> {
        if Some(*checkpoint.sequence_number())
            > self
                .get_highest_verified_checkpoint()?
                .map(|x| *x.sequence_number())
        {
            self.watermarks.insert(
                &CheckpointWatermark::HighestVerified,
                &(*checkpoint.sequence_number(), *checkpoint.digest()),
            )?;
        }

        Ok(())
    }

    pub fn update_highest_synced_checkpoint(
        &self,
        checkpoint: &VerifiedCheckpoint,
    ) -> Result<(), TypedStoreError> {
        self.watermarks.insert(
            &CheckpointWatermark::HighestSynced,
            &(*checkpoint.sequence_number(), *checkpoint.digest()),
        )
    }

    pub fn update_highest_executed_checkpoint(
        &self,
        checkpoint: &VerifiedCheckpoint,
    ) -> Result<(), TypedStoreError> {
        if let Some(seq_number) = self.get_highest_executed_checkpoint_seq_number()? {
            if seq_number >= *checkpoint.sequence_number() {
                return Ok(());
            }
            assert_eq!(seq_number + 1, *checkpoint.sequence_number(),
            "Cannot update highest executed checkpoint to {} when current highest executed checkpoint is {}",
            checkpoint.sequence_number(),
            seq_number);
        }
        self.watermarks.insert(
            &CheckpointWatermark::HighestExecuted,
            &(*checkpoint.sequence_number(), *checkpoint.digest()),
        )
    }

    pub fn update_highest_pruned_checkpoint(
        &self,
        checkpoint: &VerifiedCheckpoint,
    ) -> Result<(), TypedStoreError> {
        self.watermarks.insert(
            &CheckpointWatermark::HighestPruned,
            &(*checkpoint.sequence_number(), *checkpoint.digest()),
        )
    }

    /// Sets highest executed checkpoint to any value.
    ///
    /// WARNING: This method is very subtle and can corrupt the database if used incorrectly.
    /// It should only be used in one-off cases or tests after fully understanding the risk.
    pub fn set_highest_executed_checkpoint_subtle(
        &self,
        checkpoint: &VerifiedCheckpoint,
    ) -> Result<(), TypedStoreError> {
        self.watermarks.insert(
            &CheckpointWatermark::HighestExecuted,
            &(*checkpoint.sequence_number(), *checkpoint.digest()),
        )
    }

    pub fn insert_checkpoint_contents(
        &self,
        contents: CheckpointContents,
    ) -> Result<(), TypedStoreError> {
        self.checkpoint_content.insert(contents.digest(), &contents)
    }

    pub fn insert_verified_checkpoint_contents(
        &self,
        checkpoint: &VerifiedCheckpoint,
        full_contents: VerifiedCheckpointContents,
    ) -> Result<(), TypedStoreError> {
        let mut batch = self.full_checkpoint_content.batch();
        batch.insert_batch(
            &self.checkpoint_sequence_by_contents_digest,
            [(&checkpoint.content_digest, checkpoint.sequence_number())],
        )?;
        let full_contents = full_contents.into_inner();
        batch.insert_batch(
            &self.full_checkpoint_content,
            [(checkpoint.sequence_number(), &full_contents)],
        )?;

        let contents = full_contents.into_checkpoint_contents();
        assert_eq!(&checkpoint.content_digest, contents.digest());

        batch.insert_batch(&self.checkpoint_content, [(contents.digest(), &contents)])?;

        batch.write()
    }

    pub fn delete_full_checkpoint_contents(
        &self,
        seq: CheckpointSequenceNumber,
    ) -> Result<(), TypedStoreError> {
        self.full_checkpoint_content.remove(&seq)
    }

    pub fn get_epoch_last_checkpoint(
        &self,
        epoch_id: EpochId,
    ) -> SuiResult<Option<VerifiedCheckpoint>> {
        let seq = self.epoch_last_checkpoint_map.get(&epoch_id)?;
        let checkpoint = match seq {
            Some(seq) => self.get_checkpoint_by_sequence_number(seq)?,
            None => None,
        };
        Ok(checkpoint)
    }

    pub fn insert_epoch_last_checkpoint(
        &self,
        epoch_id: EpochId,
        checkpoint: &VerifiedCheckpoint,
    ) -> SuiResult {
        self.epoch_last_checkpoint_map
            .insert(&epoch_id, checkpoint.sequence_number())?;
        Ok(())
    }

    /// Given the epoch ID, and the last checkpoint of the epoch, derive a few statistics of the epoch.
    pub fn get_epoch_stats(
        &self,
        epoch: EpochId,
        last_checkpoint: &CheckpointSummary,
    ) -> Option<EpochStats> {
        let (first_checkpoint, prev_epoch_network_transactions) = if epoch == 0 {
            (0, 0)
        } else if let Ok(Some(checkpoint)) = self.get_epoch_last_checkpoint(epoch - 1) {
            (
                checkpoint.sequence_number + 1,
                checkpoint.network_total_transactions,
            )
        } else {
            return None;
        };
        Some(EpochStats {
            checkpoint_count: last_checkpoint.sequence_number - first_checkpoint + 1,
            transaction_count: last_checkpoint.network_total_transactions
                - prev_epoch_network_transactions,
            total_gas_reward: last_checkpoint
                .epoch_rolling_gas_cost_summary
                .computation_cost,
        })
    }

    pub fn checkpoint_db(&self, path: &Path) -> SuiResult {
        // This checkpoints the entire db and not one column family
        self.checkpoint_content
            .checkpoint_db(path)
            .map_err(SuiError::StorageError)
    }

    pub fn delete_highest_executed_checkpoint_test_only(&self) -> Result<(), TypedStoreError> {
        let mut wb = self.watermarks.batch();
        wb.delete_batch(
            &self.watermarks,
            std::iter::once(CheckpointWatermark::HighestExecuted),
        )?;
        wb.write()?;
        Ok(())
    }

    pub fn reset_db_for_execution_since_genesis(&self) -> SuiResult {
        self.delete_highest_executed_checkpoint_test_only()?;
        self.watermarks
            .rocksdb
            .flush()
            .map_err(SuiError::StorageError)?;
        Ok(())
    }
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
pub enum CheckpointWatermark {
    HighestVerified,
    HighestSynced,
    HighestExecuted,
    HighestPruned,
}

pub struct CheckpointBuilder {
    state: Arc<AuthorityState>,
    tables: Arc<CheckpointStore>,
    epoch_store: Arc<AuthorityPerEpochStore>,
    notify: Arc<Notify>,
    notify_aggregator: Arc<Notify>,
    effects_store: Box<dyn EffectsNotifyRead>,
    accumulator: Arc<StateAccumulator>,
    output: Box<dyn CheckpointOutput>,
    exit: watch::Receiver<()>,
    metrics: Arc<CheckpointMetrics>,
    max_transactions_per_checkpoint: usize,
    max_checkpoint_size_bytes: usize,
}

pub struct CheckpointAggregator {
    tables: Arc<CheckpointStore>,
    epoch_store: Arc<AuthorityPerEpochStore>,
    notify: Arc<Notify>,
    exit: watch::Receiver<()>,
    current: Option<CheckpointSignatureAggregator>,
    output: Box<dyn CertifiedCheckpointOutput>,
    state: Arc<AuthorityState>,
    metrics: Arc<CheckpointMetrics>,
}

// This holds information to aggregate signatures for one checkpoint
pub struct CheckpointSignatureAggregator {
    next_index: u64,
    summary: CheckpointSummary,
    digest: CheckpointDigest,
    /// Aggregates voting stake for each signed checkpoint proposal by authority
    signatures_by_digest: MultiStakeAggregator<CheckpointDigest, CheckpointSummary, true>,
    tables: Arc<CheckpointStore>,
    state: Arc<AuthorityState>,
    metrics: Arc<CheckpointMetrics>,
}

impl CheckpointBuilder {
    fn new(
        state: Arc<AuthorityState>,
        tables: Arc<CheckpointStore>,
        epoch_store: Arc<AuthorityPerEpochStore>,
        notify: Arc<Notify>,
        effects_store: Box<dyn EffectsNotifyRead>,
        accumulator: Arc<StateAccumulator>,
        output: Box<dyn CheckpointOutput>,
        exit: watch::Receiver<()>,
        notify_aggregator: Arc<Notify>,
        metrics: Arc<CheckpointMetrics>,
        max_transactions_per_checkpoint: usize,
        max_checkpoint_size_bytes: usize,
    ) -> Self {
        Self {
            state,
            tables,
            epoch_store,
            notify,
            effects_store,
            accumulator,
            output,
            exit,
            notify_aggregator,
            metrics,
            max_transactions_per_checkpoint,
            max_checkpoint_size_bytes,
        }
    }

    async fn run(mut self) {
        info!("Starting CheckpointBuilder");
        'main: loop {
            // Check whether an exit signal has been received, if so we break the loop.
            // This gives us a chance to exit, in case checkpoint making keeps failing.
            match self.exit.has_changed() {
                Ok(true) | Err(_) => {
                    break;
                }
                Ok(false) => (),
            };
            let mut last = self
                .epoch_store
                .last_built_checkpoint_commit_height()
                .expect("epoch should not have ended");
            for (height, pending) in self
                .epoch_store
                .get_pending_checkpoints(last)
                .expect("unexpected epoch store error")
            {
                last = Some(height);
                debug!(
                    checkpoint_commit_height = height,
                    "Making checkpoint at commit height"
                );
                if let Err(e) = self.make_checkpoint(height, pending).await {
                    error!("Error while making checkpoint, will retry in 1s: {:?}", e);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    self.metrics.checkpoint_errors.inc();
                    continue 'main;
                }
            }
            debug!("Waiting for more checkpoints from consensus after processing {last:?}");
            match select(self.exit.changed().boxed(), self.notify.notified().boxed()).await {
                Either::Left(_) => {
                    // break loop on exit signal
                    break;
                }
                Either::Right(_) => {}
            }
        }
        info!("Shutting down CheckpointBuilder");
    }

    #[instrument(level = "debug", skip_all)]
    async fn make_checkpoint(
        &self,
        height: CheckpointCommitHeight,
        pending: PendingCheckpoint,
    ) -> anyhow::Result<()> {
        self.metrics
            .checkpoint_roots_count
            .inc_by(pending.roots.len() as u64);
        let roots = self
            .effects_store
            .notify_read_executed_effects(pending.roots)
            .in_monitored_scope("CheckpointNotifyRead")
            .await?;
        let _scope = monitored_scope("CheckpointBuilder");
        let unsorted = self.complete_checkpoint_effects(roots)?;
        let sorted = {
            let _scope = monitored_scope("CheckpointBuilder::causal_sort");
            CausalOrder::causal_sort(unsorted)
        };
        let new_checkpoint = self.create_checkpoints(sorted, pending.details).await?;
        self.write_checkpoints(height, new_checkpoint).await?;
        Ok(())
    }

    #[instrument(level = "debug", skip_all)]
    async fn write_checkpoints(
        &self,
        height: CheckpointCommitHeight,
        new_checkpoint: Vec<(CheckpointSummary, CheckpointContents)>,
    ) -> SuiResult {
        let _scope = monitored_scope("CheckpointBuilder::write_checkpoints");
        let mut batch = self.tables.checkpoint_content.batch();
        for (summary, contents) in &new_checkpoint {
            debug!(
                checkpoint_commit_height = height,
                checkpoint_seq = summary.sequence_number,
                "Created checkpoint",
            );
            self.output
                .checkpoint_created(summary, contents, &self.epoch_store)
                .await?;

            self.metrics
                .transactions_included_in_checkpoint
                .inc_by(contents.size() as u64);
            let sequence_number = summary.sequence_number;
            self.metrics
                .last_constructed_checkpoint
                .set(sequence_number as i64);

            batch.insert_batch(
                &self.tables.checkpoint_content,
                [(contents.digest(), contents)],
            )?;

            batch.insert_batch(
                &self.tables.locally_computed_checkpoints,
                [(sequence_number, summary)],
            )?;
        }
        batch.write()?;

        for (local_checkpoint, _) in &new_checkpoint {
            if let Some(certified_checkpoint) = self
                .tables
                .certified_checkpoints
                .get(local_checkpoint.sequence_number())?
            {
                self.tables
                    .check_for_checkpoint_fork(local_checkpoint, &certified_checkpoint.into());
            }
        }

        self.notify_aggregator.notify_one();
        self.epoch_store
            .process_pending_checkpoint(height, new_checkpoint)?;
        Ok(())
    }

    #[allow(clippy::type_complexity)]
    fn split_checkpoint_chunks(
        &self,
        effects_and_transaction_sizes: Vec<(TransactionEffects, usize)>,
        signatures: Vec<Vec<GenericSignature>>,
    ) -> anyhow::Result<Vec<Vec<(TransactionEffects, Vec<GenericSignature>)>>> {
        let _guard = monitored_scope("CheckpointBuilder::split_checkpoint_chunks");
        let mut chunks = Vec::new();
        let mut chunk = Vec::new();
        let mut chunk_size: usize = 0;
        for ((effects, transaction_size), signatures) in effects_and_transaction_sizes
            .into_iter()
            .zip(signatures.into_iter())
        {
            // Roll over to a new chunk after either max count or max size is reached.
            // The size calculation here is intended to estimate the size of the
            // FullCheckpointContents struct. If this code is modified, that struct
            // should also be updated accordingly.
            let size = transaction_size
                + bcs::serialized_size(&effects)?
                + bcs::serialized_size(&signatures)?;
            if chunk.len() == self.max_transactions_per_checkpoint
                || (chunk_size + size) > self.max_checkpoint_size_bytes
            {
                if chunk.is_empty() {
                    // Always allow at least one tx in a checkpoint.
                    warn!("Size of single transaction ({size}) exceeds max checkpoint size ({}); allowing excessively large checkpoint to go through.", self.max_checkpoint_size_bytes);
                } else {
                    chunks.push(chunk);
                    chunk = Vec::new();
                    chunk_size = 0;
                }
            }

            chunk.push((effects, signatures));
            chunk_size += size;
        }

        if !chunk.is_empty() || chunks.is_empty() {
            // We intentionally create an empty checkpoint if there is no content provided
            // to make a 'heartbeat' checkpoint.
            // Important: if some conditions are added here later, we need to make sure we always
            // have at least one chunk if last_pending_of_epoch is set
            chunks.push(chunk);
            // Note: empty checkpoints are ok - they shouldn't happen at all on a network with even
            // modest load. Even if they do happen, it is still useful as it allows fullnodes to
            // distinguish between "no transactions have happened" and "i am not receiving new
            // checkpoints".
        }
        Ok(chunks)
    }

    #[instrument(level = "debug", skip_all)]
    async fn create_checkpoints(
        &self,
        all_effects: Vec<TransactionEffects>,
        details: PendingCheckpointInfo,
    ) -> anyhow::Result<Vec<(CheckpointSummary, CheckpointContents)>> {
        let _scope = monitored_scope("CheckpointBuilder::create_checkpoints");
        let total = all_effects.len();
        let mut last_checkpoint = self.epoch_store.last_built_checkpoint_summary()?;
        if last_checkpoint.is_none() {
            let epoch = self.epoch_store.epoch();
            if epoch > 0 {
                let previous_epoch = epoch - 1;
                let last_verified = self.tables.get_epoch_last_checkpoint(previous_epoch)?;
                last_checkpoint = last_verified.map(VerifiedCheckpoint::into_summary_and_sequence);
                if let Some((ref seq, _)) = last_checkpoint {
                    debug!("No checkpoints in builder DB, taking checkpoint from previous epoch with sequence {seq}");
                } else {
                    // This is some serious bug with when CheckpointBuilder started so surfacing it via panic
                    panic!("Can not find last checkpoint for previous epoch {previous_epoch}");
                }
            }
        }
        let last_checkpoint_seq = last_checkpoint.as_ref().map(|(seq, _)| *seq);
        info!(
            next_checkpoint_seq = last_checkpoint_seq.unwrap_or_default() + 1,
            checkpoint_timestamp = details.timestamp_ms,
            "Creating checkpoint(s) for {} transactions",
            all_effects.len(),
        );

        let all_digests: Vec<_> = all_effects
            .iter()
            .map(|effect| *effect.transaction_digest())
            .collect();
        let transactions_and_sizes = self
            .state
            .database
            .get_transactions_and_serialized_sizes(&all_digests)?;
        let mut all_effects_and_transaction_sizes = Vec::with_capacity(all_effects.len());
        let mut transaction_keys = Vec::new();
        {
            let _guard = monitored_scope("CheckpointBuilder::wait_for_transactions_sequenced");
            debug!(
                ?last_checkpoint_seq,
                "Waiting for {:?} certificates to appear in consensus",
                all_effects_and_transaction_sizes.len()
            );

            for (effects, transaction_and_size) in all_effects
                .into_iter()
                .zip(transactions_and_sizes.into_iter())
            {
                let (transaction, size) = transaction_and_size
                    .unwrap_or_else(|| panic!("Could not find executed transaction {:?}", effects));
                // ConsensusCommitPrologue and AuthenticatorStateUpdate are guaranteed to be
                // processed before we reach here
                if !matches!(
                    transaction.inner().transaction_data().kind(),
                    TransactionKind::ConsensusCommitPrologue(_)
                        | TransactionKind::ConsensusCommitPrologueV2(_)
                        | TransactionKind::AuthenticatorStateUpdate(_)
                        | TransactionKind::RandomnessStateUpdate(_)
                ) {
                    transaction_keys.push(SequencedConsensusTransactionKey::External(
                        ConsensusTransactionKey::Certificate(*effects.transaction_digest()),
                    ));
                }
                all_effects_and_transaction_sizes.push((effects, size));
            }

            self.epoch_store
                .consensus_messages_processed_notify(transaction_keys)
                .await?;
        }

        let signatures = self
            .epoch_store
            .user_signatures_for_checkpoint(&all_digests)?;
        debug!(
            ?last_checkpoint_seq,
            "Received {} checkpoint user signatures from consensus",
            signatures.len()
        );

        let chunks = self.split_checkpoint_chunks(all_effects_and_transaction_sizes, signatures)?;
        let chunks_count = chunks.len();

        let mut checkpoints = Vec::with_capacity(chunks_count);
        debug!(
            ?last_checkpoint_seq,
            "Creating {} checkpoints with {} transactions", chunks_count, total,
        );

        let epoch = self.epoch_store.epoch();
        for (index, transactions) in chunks.into_iter().enumerate() {
            let first_checkpoint_of_epoch = index == 0
                && last_checkpoint
                    .as_ref()
                    .map(|(_, c)| c.epoch != epoch)
                    .unwrap_or(true);
            if first_checkpoint_of_epoch {
                self.epoch_store
                    .record_epoch_first_checkpoint_creation_time_metric();
            }
            let last_checkpoint_of_epoch = details.last_of_epoch && index == chunks_count - 1;

            let sequence_number = last_checkpoint
                .as_ref()
                .map(|(_, c)| c.sequence_number + 1)
                .unwrap_or_default();
            let timestamp_ms = details.timestamp_ms;
            if let Some((_, last_checkpoint)) = &last_checkpoint {
                if last_checkpoint.timestamp_ms > timestamp_ms {
                    error!("Unexpected decrease of checkpoint timestamp, sequence: {}, previous: {}, current: {}",
                    sequence_number,  last_checkpoint.timestamp_ms, timestamp_ms);
                }
            }

            let (mut effects, mut signatures): (Vec<_>, Vec<_>) = transactions.into_iter().unzip();
            let epoch_rolling_gas_cost_summary =
                self.get_epoch_total_gas_cost(last_checkpoint.as_ref().map(|(_, c)| c), &effects);

            let end_of_epoch_data = if last_checkpoint_of_epoch {
                let system_state_obj = self
                    .augment_epoch_last_checkpoint(
                        &epoch_rolling_gas_cost_summary,
                        timestamp_ms,
                        &mut effects,
                        &mut signatures,
                        sequence_number,
                    )
                    .await?;

                let committee = system_state_obj.get_current_epoch_committee().committee;

                // This must happen after the call to augment_epoch_last_checkpoint,
                // otherwise we will not capture the change_epoch tx
                self.accumulator.accumulate_checkpoint(
                    effects.clone(),
                    sequence_number,
                    self.epoch_store.clone(),
                )?;

                let root_state_digest = self
                    .accumulator
                    .digest_epoch(&epoch, sequence_number, self.epoch_store.clone())
                    .in_monitored_scope("CheckpointBuilder::digest_epoch")
                    .await?;
                self.metrics.highest_accumulated_epoch.set(epoch as i64);
                info!("Epoch {epoch} root state hash digest: {root_state_digest:?}");

                let epoch_commitments = if self
                    .epoch_store
                    .protocol_config()
                    .check_commit_root_state_digest_supported()
                {
                    vec![root_state_digest.into()]
                } else {
                    vec![]
                };

                Some(EndOfEpochData {
                    next_epoch_committee: committee.voting_rights,
                    next_epoch_protocol_version: ProtocolVersion::new(
                        system_state_obj.protocol_version(),
                    ),
                    epoch_commitments,
                })
            } else {
                None
            };

            let contents = CheckpointContents::new_with_digests_and_signatures(
                effects.iter().map(TransactionEffects::execution_digests),
                signatures,
            );

            let num_txns = contents.size() as u64;

            let network_total_transactions = last_checkpoint
                .as_ref()
                .map(|(_, c)| c.network_total_transactions + num_txns)
                .unwrap_or(num_txns);

            let previous_digest = last_checkpoint.as_ref().map(|(_, c)| c.digest());
            let summary = CheckpointSummary::new(
                epoch,
                sequence_number,
                network_total_transactions,
                &contents,
                previous_digest,
                epoch_rolling_gas_cost_summary,
                end_of_epoch_data,
                timestamp_ms,
            );
            summary.report_checkpoint_age_ms(&self.metrics.last_created_checkpoint_age_ms);
            if last_checkpoint_of_epoch {
                info!(
                    checkpoint_seq = sequence_number,
                    "creating last checkpoint of epoch {}", epoch
                );
                if let Some(stats) = self.tables.get_epoch_stats(epoch, &summary) {
                    self.epoch_store
                        .report_epoch_metrics_at_last_checkpoint(stats);
                }
            }
            last_checkpoint = Some((sequence_number, summary.clone()));
            checkpoints.push((summary, contents));
        }

        Ok(checkpoints)
    }

    fn get_epoch_total_gas_cost(
        &self,
        last_checkpoint: Option<&CheckpointSummary>,
        cur_checkpoint_effects: &[TransactionEffects],
    ) -> GasCostSummary {
        let (previous_epoch, previous_gas_costs) = last_checkpoint
            .map(|c| (c.epoch, c.epoch_rolling_gas_cost_summary.clone()))
            .unwrap_or_default();
        let current_gas_costs = GasCostSummary::new_from_txn_effects(cur_checkpoint_effects.iter());
        if previous_epoch == self.epoch_store.epoch() {
            // sum only when we are within the same epoch
            GasCostSummary::new(
                previous_gas_costs.computation_cost + current_gas_costs.computation_cost,
                previous_gas_costs.storage_cost + current_gas_costs.storage_cost,
                previous_gas_costs.storage_rebate + current_gas_costs.storage_rebate,
                previous_gas_costs.non_refundable_storage_fee
                    + current_gas_costs.non_refundable_storage_fee,
            )
        } else {
            current_gas_costs
        }
    }

    #[instrument(level = "error", skip_all)]
    async fn augment_epoch_last_checkpoint(
        &self,
        epoch_total_gas_cost: &GasCostSummary,
        epoch_start_timestamp_ms: CheckpointTimestamp,
        checkpoint_effects: &mut Vec<TransactionEffects>,
        signatures: &mut Vec<Vec<GenericSignature>>,
        checkpoint: CheckpointSequenceNumber,
        // TODO: Check whether we must use anyhow::Result or can we use SuiResult.
    ) -> anyhow::Result<SuiSystemState> {
        let (system_state, effects) = self
            .state
            .create_and_execute_advance_epoch_tx(
                &self.epoch_store,
                epoch_total_gas_cost,
                checkpoint,
                epoch_start_timestamp_ms,
            )
            .await?;
        checkpoint_effects.push(effects);
        signatures.push(vec![]);
        Ok(system_state)
    }

    /// For the given roots return complete list of effects to include in checkpoint
    /// This list includes the roots and all their dependencies, which are not part of checkpoint already
    #[instrument(level = "debug", skip_all)]
    fn complete_checkpoint_effects(
        &self,
        mut roots: Vec<TransactionEffects>,
    ) -> SuiResult<Vec<TransactionEffects>> {
        let _scope = monitored_scope("CheckpointBuilder::complete_checkpoint_effects");
        let mut results = vec![];
        let mut seen = HashSet::new();
        loop {
            let mut pending = HashSet::new();

            let transactions_included = self
                .epoch_store
                .builder_included_transactions_in_checkpoint(
                    roots.iter().map(|e| e.transaction_digest()),
                )?;

            for (effect, tx_included) in roots.into_iter().zip(transactions_included.into_iter()) {
                let digest = effect.transaction_digest();
                // Unnecessary to read effects of a dependency if the effect is already processed.
                seen.insert(*digest);

                // Skip roots already included in checkpoints or roots from previous epochs
                if tx_included || effect.executed_epoch() < self.epoch_store.epoch() {
                    continue;
                }

                let existing_effects = self
                    .epoch_store
                    .effects_signatures_exists(effect.dependencies().iter())?;

                for (dependency, effects_signature_exists) in
                    effect.dependencies().iter().zip(existing_effects.iter())
                {
                    // Skip here if dependency not executed in the current epoch.
                    // Note that the existence of an effects signature in the
                    // epoch store for the given digest indicates that the transaction
                    // was locally executed in the current epoch
                    if !effects_signature_exists {
                        continue;
                    }
                    if seen.insert(*dependency) {
                        pending.insert(*dependency);
                    }
                }
                results.push(effect);
            }
            if pending.is_empty() {
                break;
            }
            let pending = pending.into_iter().collect::<Vec<_>>();
            let effects = self.effects_store.multi_get_executed_effects(&pending)?;
            let effects = effects
                .into_iter()
                .zip(pending)
                .map(|(opt, digest)| match opt {
                    Some(x) => x,
                    None => panic!(
                        "Can not find effect for transaction {:?}, however transaction that depend on it was already executed",
                        digest
                    ),
                })
                .collect::<Vec<_>>();
            roots = effects;
        }
        Ok(results)
    }
}

impl CheckpointAggregator {
    fn new(
        tables: Arc<CheckpointStore>,
        epoch_store: Arc<AuthorityPerEpochStore>,
        notify: Arc<Notify>,
        exit: watch::Receiver<()>,
        output: Box<dyn CertifiedCheckpointOutput>,
        state: Arc<AuthorityState>,
        metrics: Arc<CheckpointMetrics>,
    ) -> Self {
        let current = None;
        Self {
            tables,
            epoch_store,
            notify,
            exit,
            current,
            output,
            state,
            metrics,
        }
    }

    async fn run(mut self) {
        info!("Starting CheckpointAggregator");
        loop {
            if let Err(e) = self.run_and_notify().await {
                error!(
                    "Error while aggregating checkpoint, will retry in 1s: {:?}",
                    e
                );
                self.metrics.checkpoint_errors.inc();
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }

            match select(
                self.exit.changed().boxed(),
                timeout(Duration::from_secs(1), self.notify.notified()).boxed(),
            )
            .await
            {
                Either::Left(_) => {
                    // return on exit signal
                    info!("Shutting down CheckpointAggregator");
                    return;
                }
                Either::Right(_) => {}
            }
        }
    }

    async fn run_and_notify(&mut self) -> SuiResult {
        let summaries = self.run_inner()?;
        for summary in summaries {
            self.output.certified_checkpoint_created(&summary).await?;
        }
        Ok(())
    }

    fn run_inner(&mut self) -> SuiResult<Vec<CertifiedCheckpointSummary>> {
        let _scope = monitored_scope("CheckpointAggregator");
        let mut result = vec![];
        'outer: loop {
            let next_to_certify = self.next_checkpoint_to_certify();
            let current = if let Some(current) = &mut self.current {
                // It's possible that the checkpoint was already certified by
                // the rest of the network and we've already received the
                // certified checkpoint via StateSync. In this case, we reset
                // the current signature aggregator to the next checkpoint to
                // be certified
                if current.summary.sequence_number < next_to_certify {
                    self.current = None;
                    continue;
                }
                current
            } else {
                let Some(summary) = self
                    .epoch_store
                    .get_built_checkpoint_summary(next_to_certify)?
                else {
                    return Ok(result);
                };
                self.current = Some(CheckpointSignatureAggregator {
                    next_index: 0,
                    digest: summary.digest(),
                    summary,
                    signatures_by_digest: MultiStakeAggregator::new(
                        self.epoch_store.committee().clone(),
                    ),
                    tables: self.tables.clone(),
                    state: self.state.clone(),
                    metrics: self.metrics.clone(),
                });
                self.current.as_mut().unwrap()
            };

            let epoch_tables = self
                .epoch_store
                .tables()
                .expect("should not run past end of epoch");
            let iter = epoch_tables.get_pending_checkpoint_signatures_iter(
                current.summary.sequence_number,
                current.next_index,
            )?;
            for ((seq, index), data) in iter {
                if seq != current.summary.sequence_number {
                    debug!(
                        checkpoint_seq =? current.summary.sequence_number,
                        "Not enough checkpoint signatures",
                    );
                    // No more signatures (yet) for this checkpoint
                    return Ok(result);
                }
                debug!(
                    checkpoint_seq = current.summary.sequence_number,
                    "Processing signature for checkpoint (digest: {:?}) from {:?}",
                    current.summary.digest(),
                    data.summary.auth_sig().authority.concise()
                );
                self.metrics
                    .checkpoint_participation
                    .with_label_values(&[&format!(
                        "{:?}",
                        data.summary.auth_sig().authority.concise()
                    )])
                    .inc();
                if let Ok(auth_signature) = current.try_aggregate(data) {
                    let summary = VerifiedCheckpoint::new_unchecked(
                        CertifiedCheckpointSummary::new_from_data_and_sig(
                            current.summary.clone(),
                            auth_signature,
                        ),
                    );

                    self.tables.insert_certified_checkpoint(&summary)?;
                    self.metrics
                        .last_certified_checkpoint
                        .set(current.summary.sequence_number as i64);
                    current
                        .summary
                        .report_checkpoint_age_ms(&self.metrics.last_certified_checkpoint_age_ms);
                    result.push(summary.into_inner());
                    self.current = None;
                    continue 'outer;
                } else {
                    current.next_index = index + 1;
                }
            }
            break;
        }
        Ok(result)
    }

    fn next_checkpoint_to_certify(&self) -> CheckpointSequenceNumber {
        self.tables
            .certified_checkpoints
            .unbounded_iter()
            .skip_to_last()
            .next()
            .map(|(seq, _)| seq + 1)
            .unwrap_or_default()
    }
}

impl CheckpointSignatureAggregator {
    #[allow(clippy::result_unit_err)]
    pub fn try_aggregate(
        &mut self,
        data: CheckpointSignatureMessage,
    ) -> Result<AuthorityStrongQuorumSignInfo, ()> {
        let their_digest = *data.summary.digest();
        let (_, signature) = data.summary.into_data_and_sig();
        let author = signature.authority;
        let envelope =
            SignedCheckpointSummary::new_from_data_and_sig(self.summary.clone(), signature);
        match self.signatures_by_digest.insert(their_digest, envelope) {
            InsertResult::Failed { error } => {
                warn!(
                    checkpoint_seq = self.summary.sequence_number,
                    "Failed to aggregate new signature from validator {:?}: {:?}",
                    author.concise(),
                    error
                );
                self.check_for_split_brain();
                Err(())
            }
            InsertResult::QuorumReached(cert) => {
                // It is not guaranteed that signature.authority == narwhal_cert.author, but we do verify
                // the signature so we know that the author signed the message at some point.
                if their_digest != self.digest {
                    self.metrics.remote_checkpoint_forks.inc();
                    warn!(
                        checkpoint_seq = self.summary.sequence_number,
                        "Validator {:?} has mismatching checkpoint digest {}, we have digest {}",
                        author.concise(),
                        their_digest,
                        self.digest
                    );
                    return Err(());
                }
                Ok(cert)
            }
            InsertResult::NotEnoughVotes {
                bad_votes: _,
                bad_authorities: _,
            } => {
                self.check_for_split_brain();
                Err(())
            }
        }
    }

    /// Check if there is a split brain condition in checkpoint signature aggregation, defined
    /// as any state wherein it is no longer possible to achieve quorum on a checkpoint proposal,
    /// irrespective of the outcome of any outstanding votes.
    fn check_for_split_brain(&self) {
        debug!(
            checkpoint_seq = self.summary.sequence_number,
            "Checking for split brain condition"
        );
        if self.signatures_by_digest.quorum_unreachable() {
            // TODO: at this point we should immediately halt processing
            // of new transaction certificates to avoid building on top of
            // forked output
            // self.halt_all_execution();

            let digests_by_stake_messages = self
                .signatures_by_digest
                .get_all_unique_values()
                .into_iter()
                .sorted_by_key(|(_, (_, stake))| -(*stake as i64))
                .map(|(digest, (_authorities, total_stake))| {
                    format!("{:?} (total stake: {})", digest, total_stake)
                })
                .collect::<Vec<String>>();
            error!(
                checkpoint_seq = self.summary.sequence_number,
                "Split brain detected in checkpoint signature aggregation! Remaining stake: {:?}, Digests by stake: {:?}",
                self.signatures_by_digest.uncommitted_stake(),
                digests_by_stake_messages,
            );
            self.metrics.split_brain_checkpoint_forks.inc();

            let all_unique_values = self.signatures_by_digest.get_all_unique_values();
            let local_summary = self.summary.clone();
            let state = self.state.clone();
            let tables = self.tables.clone();

            tokio::spawn(async move {
                diagnose_split_brain(all_unique_values, local_summary, state, tables).await;
            });
        }
    }
}

/// Create data dump containing relevant data for diagnosing cause of the
/// split brain by querying one disagreeing validator for full checkpoint contents.
/// To minimize peer chatter, we only query one validator at random from each
/// disagreeing faction, as all honest validators that participated in this round may
/// inevitably run the same process.
async fn diagnose_split_brain(
    all_unique_values: BTreeMap<CheckpointDigest, (Vec<AuthorityName>, StakeUnit)>,
    local_summary: CheckpointSummary,
    state: Arc<AuthorityState>,
    tables: Arc<CheckpointStore>,
) {
    debug!(
        checkpoint_seq = local_summary.sequence_number,
        "Running split brain diagnostics..."
    );
    let time = Utc::now();
    // collect one random disagreeing validator per differing digest
    let digest_to_validator = all_unique_values
        .iter()
        .filter_map(|(digest, (validators, _))| {
            if *digest != local_summary.digest() {
                let random_validator = validators.choose(&mut OsRng).unwrap();
                Some((*digest, *random_validator))
            } else {
                None
            }
        })
        .collect::<HashMap<_, _>>();
    if digest_to_validator.is_empty() {
        panic!(
            "Given split brain condition, there should be at \
                least one validator that disagrees with local signature"
        );
    }

    let sui_system_state = state
        .database
        .get_sui_system_state_object_unsafe()
        .expect("Failed to get system state object");
    let committee = sui_system_state.get_current_epoch_committee();
    let network_config = default_mysten_network_config();
    let network_clients =
        make_network_authority_clients_with_network_config(&committee, &network_config)
            .expect("Failed to make authority clients from committee {committee}");

    // Query all disagreeing validators
    let response_futures = digest_to_validator
        .values()
        .cloned()
        .map(|validator| {
            let client = network_clients
                .get(&validator)
                .expect("Failed to get network client");
            let request = CheckpointRequestV2 {
                sequence_number: Some(local_summary.sequence_number),
                request_content: true,
                certified: false,
            };
            client.handle_checkpoint_v2(request)
        })
        .collect::<Vec<_>>();

    let digest_name_pair = digest_to_validator.iter();
    let response_data = futures::future::join_all(response_futures)
        .await
        .into_iter()
        .zip(digest_name_pair)
        .filter_map(|(response, (digest, name))| match response {
            Ok(response) => match response {
                CheckpointResponseV2 {
                    checkpoint: Some(CheckpointSummaryResponse::Pending(summary)),
                    contents: Some(contents),
                } => Some((*name, *digest, summary, contents)),
                CheckpointResponseV2 {
                    checkpoint: Some(CheckpointSummaryResponse::Certified(_)),
                    contents: _,
                } => {
                    panic!("Expected pending checkpoint, but got certified checkpoint");
                }
                CheckpointResponseV2 {
                    checkpoint: None,
                    contents: _,
                } => {
                    error!(
                        "Summary for checkpoint {:?} not found on validator {:?}",
                        local_summary.sequence_number, name
                    );
                    None
                }
                CheckpointResponseV2 {
                    checkpoint: _,
                    contents: None,
                } => {
                    error!(
                        "Contents for checkpoint {:?} not found on validator {:?}",
                        local_summary.sequence_number, name
                    );
                    None
                }
            },
            Err(e) => {
                error!(
                    "Failed to get checkpoint contents from validator for fork diagnostics: {:?}",
                    e
                );
                None
            }
        })
        .collect::<Vec<_>>();

    let local_checkpoint_contents = tables
        .get_checkpoint_contents(&local_summary.content_digest)
        .unwrap_or_else(|_| {
            panic!(
                "Could not find checkpoint contents for digest {:?}",
                local_summary.digest()
            )
        })
        .unwrap_or_else(|| {
            panic!(
                "Could not find local full checkpoint contents for checkpoint {:?}, digest {:?}",
                local_summary.sequence_number,
                local_summary.digest()
            )
        });
    let local_contents_text = format!("{local_checkpoint_contents:?}");

    let local_summary_text = format!("{local_summary:?}");
    let local_validator = state.name.concise();
    let diff_patches = response_data
        .iter()
        .map(|(name, other_digest, other_summary, contents)| {
            let other_contents_text = format!("{contents:?}");
            let other_summary_text = format!("{other_summary:?}");
            let (local_transactions, local_effects): (Vec<_>, Vec<_>) = local_checkpoint_contents
                .enumerate_transactions(&local_summary)
                .map(|(_, exec_digest)| (exec_digest.transaction, exec_digest.effects))
                .unzip();
            let (other_transactions, other_effects): (Vec<_>, Vec<_>) = contents
                .enumerate_transactions(other_summary)
                .map(|(_, exec_digest)| (exec_digest.transaction, exec_digest.effects))
                .unzip();
            let summary_patch = create_patch(&local_summary_text, &other_summary_text);
            let contents_patch = create_patch(&local_contents_text, &other_contents_text);
            let local_transactions_text = format!("{local_transactions:#?}");
            let other_transactions_text = format!("{other_transactions:#?}");
            let transactions_patch =
                create_patch(&local_transactions_text, &other_transactions_text);
            let local_effects_text = format!("{local_effects:#?}");
            let other_effects_text = format!("{other_effects:#?}");
            let effects_patch = create_patch(&local_effects_text, &other_effects_text);
            let seq_number = local_summary.sequence_number;
            let local_digest = local_summary.digest();
            let other_validator = name.concise();
            format!(
                "Checkpoint: {seq_number:?}\n\
                Local validator (original): {local_validator:?}, digest: {local_digest:?}\n\
                Other validator (modified): {other_validator:?}, digest: {other_digest:?}\n\n\
                Summary Diff: \n{summary_patch}\n\n\
                Contents Diff: \n{contents_patch}\n\n\
                Transactions Diff: \n{transactions_patch}\n\n\
                Effects Diff: \n{effects_patch}",
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n\n");

    let header = format!(
        "Checkpoint Fork Dump - Authority {local_validator:?}: \n\
        Datetime: {time}",
    );
    let fork_logs_text = format!("{header}\n\n{diff_patches}\n\n");
    let path = tempfile::tempdir()
        .expect("Failed to create tempdir")
        .into_path()
        .join(Path::new("checkpoint_fork_dump.txt"));
    let mut file = File::create(path).unwrap();
    write!(file, "{}", fork_logs_text).unwrap();
    debug!("{}", fork_logs_text);

    fail_point!("split_brain_reached");

    // There is no option to never restart the node, so choosing longer than should
    // be needed for any testcase
    // #[cfg(msim)]
    // sui_simulator::task::kill_current_node(Some(Duration::from_secs(100)));
}

pub trait CheckpointServiceNotify {
    fn notify_checkpoint_signature(
        &self,
        epoch_store: &AuthorityPerEpochStore,
        info: &CheckpointSignatureMessage,
    ) -> SuiResult;

    fn notify_checkpoint(&self, checkpoint: &PendingCheckpoint) -> SuiResult;
}

/// This is a service used to communicate with other pieces of sui(for ex. authority)
pub struct CheckpointService {
    tables: Arc<CheckpointStore>,
    notify_builder: Arc<Notify>,
    notify_aggregator: Arc<Notify>,
    last_signature_index: Mutex<u64>,
    metrics: Arc<CheckpointMetrics>,
}

impl CheckpointService {
    pub fn spawn(
        state: Arc<AuthorityState>,
        checkpoint_store: Arc<CheckpointStore>,
        epoch_store: Arc<AuthorityPerEpochStore>,
        effects_store: Box<dyn EffectsNotifyRead>,
        accumulator: Arc<StateAccumulator>,
        checkpoint_output: Box<dyn CheckpointOutput>,
        certified_checkpoint_output: Box<dyn CertifiedCheckpointOutput>,
        metrics: Arc<CheckpointMetrics>,
        max_transactions_per_checkpoint: usize,
        max_checkpoint_size_bytes: usize,
    ) -> (Arc<Self>, watch::Sender<()> /* The exit sender */) {
        info!(
            "Starting checkpoint service with {max_transactions_per_checkpoint} max_transactions_per_checkpoint and {max_checkpoint_size_bytes} max_checkpoint_size_bytes"
        );
        let notify_builder = Arc::new(Notify::new());
        let notify_aggregator = Arc::new(Notify::new());

        let (exit_snd, exit_rcv) = watch::channel(());

        let builder = CheckpointBuilder::new(
            state.clone(),
            checkpoint_store.clone(),
            epoch_store.clone(),
            notify_builder.clone(),
            effects_store,
            accumulator,
            checkpoint_output,
            exit_rcv.clone(),
            notify_aggregator.clone(),
            metrics.clone(),
            max_transactions_per_checkpoint,
            max_checkpoint_size_bytes,
        );

        spawn_monitored_task!(builder.run());

        let aggregator = CheckpointAggregator::new(
            checkpoint_store.clone(),
            epoch_store.clone(),
            notify_aggregator.clone(),
            exit_rcv,
            certified_checkpoint_output,
            state.clone(),
            metrics.clone(),
        );

        spawn_monitored_task!(aggregator.run());

        let last_signature_index = epoch_store
            .get_last_checkpoint_signature_index()
            .expect("should not cross end of epoch");
        let last_signature_index = Mutex::new(last_signature_index);

        let service = Arc::new(Self {
            tables: checkpoint_store,
            notify_builder,
            notify_aggregator,
            last_signature_index,
            metrics,
        });
        (service, exit_snd)
    }

    #[cfg(test)]
    fn write_and_notify_checkpoint_for_testing(
        &self,
        epoch_store: &AuthorityPerEpochStore,
        checkpoint: PendingCheckpoint,
    ) -> SuiResult {
        let mut batch = epoch_store.db_batch_for_test();
        epoch_store.write_pending_checkpoint(&mut batch, &checkpoint)?;
        batch.write()?;
        self.notify_checkpoint(&checkpoint)?;
        Ok(())
    }
}

impl CheckpointServiceNotify for CheckpointService {
    fn notify_checkpoint_signature(
        &self,
        epoch_store: &AuthorityPerEpochStore,
        info: &CheckpointSignatureMessage,
    ) -> SuiResult {
        let sequence = info.summary.sequence_number;
        let signer = info.summary.auth_sig().authority.concise();
        if let Some(last_certified) = self
            .tables
            .certified_checkpoints
            .keys()
            .skip_to_last()
            .next()
            .transpose()?
        {
            if sequence <= last_certified {
                debug!(
                    checkpoint_seq = sequence,
                    "Ignore checkpoint signature from {} - already certified", signer,
                );
                return Ok(());
            }
        }
        debug!(
            checkpoint_seq = sequence,
            "Received checkpoint signature, digest {} from {}",
            info.summary.digest(),
            signer,
        );
        self.metrics
            .last_received_checkpoint_signatures
            .with_label_values(&[&signer.to_string()])
            .set(sequence as i64);
        // While it can be tempting to make last_signature_index into AtomicU64, this won't work
        // We need to make sure we write to `pending_signatures` and trigger `notify_aggregator` without race conditions
        let mut index = self.last_signature_index.lock();
        *index += 1;
        epoch_store.insert_checkpoint_signature(sequence, *index, info)?;
        self.notify_aggregator.notify_one();
        Ok(())
    }

    fn notify_checkpoint(&self, checkpoint: &PendingCheckpoint) -> SuiResult {
        debug!(
            checkpoint_commit_height = checkpoint.height(),
            "Notifying builder about checkpoint",
        );
        self.notify_builder.notify_one();
        Ok(())
    }
}

// test helper
pub struct CheckpointServiceNoop {}
impl CheckpointServiceNotify for CheckpointServiceNoop {
    fn notify_checkpoint_signature(
        &self,
        _: &AuthorityPerEpochStore,
        _: &CheckpointSignatureMessage,
    ) -> SuiResult {
        Ok(())
    }

    fn notify_checkpoint(&self, _: &PendingCheckpoint) -> SuiResult {
        Ok(())
    }
}

impl PendingCheckpoint {
    pub fn height(&self) -> CheckpointCommitHeight {
        self.details.commit_height
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authority::test_authority_builder::TestAuthorityBuilder;
    use async_trait::async_trait;
    use shared_crypto::intent::{Intent, IntentScope};
    use std::collections::{BTreeMap, HashMap};
    use std::ops::Deref;
    use sui_macros::sim_test;
    use sui_types::base_types::{ObjectID, SequenceNumber, TransactionEffectsDigest};
    use sui_types::crypto::{AuthoritySignInfo, Signature};
    use sui_types::effects::TransactionEffects;
    use sui_types::messages_checkpoint::SignedCheckpointSummary;
    use sui_types::move_package::MovePackage;
    use sui_types::object;
    use sui_types::transaction::{GenesisObject, VerifiedTransaction};
    use tokio::sync::mpsc;

    #[sim_test]
    pub async fn checkpoint_builder_test() {
        telemetry_subscribers::init_for_testing();
        let state = TestAuthorityBuilder::new().build().await;

        let dummy_tx = VerifiedTransaction::new_genesis_transaction(vec![]);
        let dummy_tx_with_data =
            VerifiedTransaction::new_genesis_transaction(vec![GenesisObject::RawObject {
                data: object::Data::Package(
                    MovePackage::new(
                        ObjectID::random(),
                        SequenceNumber::new(),
                        BTreeMap::from([(format!("{:0>40000}", "1"), Vec::new())]),
                        100_000,
                        // no modules so empty type_origin_table as no types are defined in this package
                        Vec::new(),
                        // no modules so empty linkage_table as no dependencies of this package exist
                        BTreeMap::new(),
                    )
                    .unwrap(),
                ),
                owner: object::Owner::Immutable,
            }]);
        for i in 0..15 {
            state
                .database
                .perpetual_tables
                .transactions
                .insert(&d(i), dummy_tx.serializable_ref())
                .unwrap();
        }
        for i in 15..20 {
            state
                .database
                .perpetual_tables
                .transactions
                .insert(&d(i), dummy_tx_with_data.serializable_ref())
                .unwrap();
        }

        let mut store = HashMap::<TransactionDigest, TransactionEffects>::new();
        commit_cert_for_test(
            &mut store,
            state.clone(),
            d(1),
            vec![d(2), d(3)],
            GasCostSummary::new(11, 12, 11, 1),
        );
        commit_cert_for_test(
            &mut store,
            state.clone(),
            d(2),
            vec![d(3), d(4)],
            GasCostSummary::new(21, 22, 21, 1),
        );
        commit_cert_for_test(
            &mut store,
            state.clone(),
            d(3),
            vec![],
            GasCostSummary::new(31, 32, 31, 1),
        );
        commit_cert_for_test(
            &mut store,
            state.clone(),
            d(4),
            vec![],
            GasCostSummary::new(41, 42, 41, 1),
        );
        for i in [10, 11, 12, 13] {
            commit_cert_for_test(
                &mut store,
                state.clone(),
                d(i),
                vec![],
                GasCostSummary::new(41, 42, 41, 1),
            );
        }
        for i in [15, 16, 17] {
            commit_cert_for_test(
                &mut store,
                state.clone(),
                d(i),
                vec![],
                GasCostSummary::new(51, 52, 51, 1),
            );
        }
        let all_digests: Vec<_> = store.keys().copied().collect();
        for digest in all_digests {
            let signature = Signature::Ed25519SuiSignature(Default::default()).into();
            state
                .epoch_store_for_testing()
                .test_insert_user_signature(digest, vec![signature]);
        }

        let (output, mut result) = mpsc::channel::<(CheckpointContents, CheckpointSummary)>(10);
        let (certified_output, mut certified_result) =
            mpsc::channel::<CertifiedCheckpointSummary>(10);
        let store = Box::new(store);

        let ckpt_dir = tempfile::tempdir().unwrap();
        let checkpoint_store = CheckpointStore::new(ckpt_dir.path());

        let accumulator = StateAccumulator::new(state.database.clone());

        let epoch_store = state.epoch_store_for_testing();
        let (checkpoint_service, _exit) = CheckpointService::spawn(
            state.clone(),
            checkpoint_store,
            epoch_store.clone(),
            store,
            Arc::new(accumulator),
            Box::new(output),
            Box::new(certified_output),
            CheckpointMetrics::new_for_tests(),
            3,
            100_000,
        );

        checkpoint_service
            .write_and_notify_checkpoint_for_testing(&epoch_store, p(0, vec![4]))
            .unwrap();
        // Verify that sending same digests at same height is noop
        checkpoint_service
            .write_and_notify_checkpoint_for_testing(&epoch_store, p(0, vec![4]))
            .unwrap();
        checkpoint_service
            .write_and_notify_checkpoint_for_testing(&epoch_store, p(1, vec![1, 3]))
            .unwrap();
        checkpoint_service
            .write_and_notify_checkpoint_for_testing(&epoch_store, p(2, vec![10, 11, 12, 13]))
            .unwrap();
        checkpoint_service
            .write_and_notify_checkpoint_for_testing(&epoch_store, p(3, vec![15, 16, 17]))
            .unwrap();

        let (c1c, c1s) = result.recv().await.unwrap();
        let (c2c, c2s) = result.recv().await.unwrap();

        let c1t = c1c.iter().map(|d| d.transaction).collect::<Vec<_>>();
        let c2t = c2c.iter().map(|d| d.transaction).collect::<Vec<_>>();
        assert_eq!(c1t, vec![d(4)]);
        assert_eq!(c1s.previous_digest, None);
        assert_eq!(c1s.sequence_number, 0);
        assert_eq!(
            c1s.epoch_rolling_gas_cost_summary,
            GasCostSummary::new(41, 42, 41, 1)
        );

        assert_eq!(c2t, vec![d(3), d(2), d(1)]);
        assert_eq!(c2s.previous_digest, Some(c1s.digest()));
        assert_eq!(c2s.sequence_number, 1);
        assert_eq!(
            c2s.epoch_rolling_gas_cost_summary,
            GasCostSummary::new(104, 108, 104, 4)
        );

        // Pending at index 2 had 4 transactions, and we configured 3 transactions max.
        // Verify that we split into 2 checkpoints.
        let (c3c, c3s) = result.recv().await.unwrap();
        let c3t = c3c.iter().map(|d| d.transaction).collect::<Vec<_>>();
        let (c4c, c4s) = result.recv().await.unwrap();
        let c4t = c4c.iter().map(|d| d.transaction).collect::<Vec<_>>();
        assert_eq!(c3s.sequence_number, 2);
        assert_eq!(c3s.previous_digest, Some(c2s.digest()));
        assert_eq!(c4s.sequence_number, 3);
        assert_eq!(c4s.previous_digest, Some(c3s.digest()));
        assert_eq!(c3t, vec![d(10), d(11), d(12)]);
        assert_eq!(c4t, vec![d(13)]);

        // Pending at index 3 had 3 transactions of 40K size, and we configured 100K max.
        // Verify that we split into 2 checkpoints.
        let (c5c, c5s) = result.recv().await.unwrap();
        let c5t = c5c.iter().map(|d| d.transaction).collect::<Vec<_>>();
        let (c6c, c6s) = result.recv().await.unwrap();
        let c6t = c6c.iter().map(|d| d.transaction).collect::<Vec<_>>();
        assert_eq!(c5s.sequence_number, 4);
        assert_eq!(c5s.previous_digest, Some(c4s.digest()));
        assert_eq!(c6s.sequence_number, 5);
        assert_eq!(c6s.previous_digest, Some(c5s.digest()));
        assert_eq!(c5t, vec![d(15), d(16)]);
        assert_eq!(c6t, vec![d(17)]);

        let c1ss = SignedCheckpointSummary::new(c1s.epoch, c1s, state.secret.deref(), state.name);
        let c2ss = SignedCheckpointSummary::new(c2s.epoch, c2s, state.secret.deref(), state.name);

        checkpoint_service
            .notify_checkpoint_signature(
                &epoch_store,
                &CheckpointSignatureMessage { summary: c2ss },
            )
            .unwrap();
        checkpoint_service
            .notify_checkpoint_signature(
                &epoch_store,
                &CheckpointSignatureMessage { summary: c1ss },
            )
            .unwrap();

        let c1sc = certified_result.recv().await.unwrap();
        let c2sc = certified_result.recv().await.unwrap();
        assert_eq!(c1sc.sequence_number, 0);
        assert_eq!(c2sc.sequence_number, 1);
    }

    #[async_trait]
    impl EffectsNotifyRead for HashMap<TransactionDigest, TransactionEffects> {
        async fn notify_read_executed_effects(
            &self,
            digests: Vec<TransactionDigest>,
        ) -> SuiResult<Vec<TransactionEffects>> {
            Ok(digests
                .into_iter()
                .map(|d| self.get(&d).expect("effects not found").clone())
                .collect())
        }

        async fn notify_read_executed_effects_digests(
            &self,
            digests: Vec<TransactionDigest>,
        ) -> SuiResult<Vec<TransactionEffectsDigest>> {
            Ok(digests
                .into_iter()
                .map(|d| {
                    self.get(&d)
                        .map(|fx| fx.digest())
                        .expect("effects not found")
                })
                .collect())
        }

        fn multi_get_executed_effects(
            &self,
            digests: &[TransactionDigest],
        ) -> SuiResult<Vec<Option<TransactionEffects>>> {
            Ok(digests.iter().map(|d| self.get(d).cloned()).collect())
        }
    }

    #[async_trait::async_trait]
    impl CheckpointOutput for mpsc::Sender<(CheckpointContents, CheckpointSummary)> {
        async fn checkpoint_created(
            &self,
            summary: &CheckpointSummary,
            contents: &CheckpointContents,
            _epoch_store: &Arc<AuthorityPerEpochStore>,
        ) -> SuiResult {
            self.try_send((contents.clone(), summary.clone())).unwrap();
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl CertifiedCheckpointOutput for mpsc::Sender<CertifiedCheckpointSummary> {
        async fn certified_checkpoint_created(
            &self,
            summary: &CertifiedCheckpointSummary,
        ) -> SuiResult {
            self.try_send(summary.clone()).unwrap();
            Ok(())
        }
    }

    fn p(i: u64, t: Vec<u8>) -> PendingCheckpoint {
        PendingCheckpoint {
            roots: t.into_iter().map(d).collect(),
            details: PendingCheckpointInfo {
                timestamp_ms: 0,
                last_of_epoch: false,
                commit_height: i,
            },
        }
    }

    fn d(i: u8) -> TransactionDigest {
        let mut bytes: [u8; 32] = Default::default();
        bytes[0] = i;
        TransactionDigest::new(bytes)
    }

    fn e(
        transaction_digest: TransactionDigest,
        dependencies: Vec<TransactionDigest>,
        gas_used: GasCostSummary,
    ) -> TransactionEffects {
        let mut effects = TransactionEffects::default();
        *effects.transaction_digest_mut_for_testing() = transaction_digest;
        *effects.dependencies_mut_for_testing() = dependencies;
        *effects.gas_cost_summary_mut_for_testing() = gas_used;
        effects
    }

    fn commit_cert_for_test(
        store: &mut HashMap<TransactionDigest, TransactionEffects>,
        state: Arc<AuthorityState>,
        digest: TransactionDigest,
        dependencies: Vec<TransactionDigest>,
        gas_used: GasCostSummary,
    ) {
        let epoch_store = state.epoch_store_for_testing();
        let effects = e(digest, dependencies, gas_used);
        store.insert(digest, effects.clone());
        epoch_store
            .insert_tx_cert_and_effects_signature(
                &digest,
                None,
                Some(&AuthoritySignInfo::new(
                    epoch_store.epoch(),
                    &effects,
                    Intent::sui_app(IntentScope::TransactionEffects),
                    state.name,
                    &*state.secret,
                )),
            )
            .expect("Inserting cert fx and sigs should not fail");
    }
}
