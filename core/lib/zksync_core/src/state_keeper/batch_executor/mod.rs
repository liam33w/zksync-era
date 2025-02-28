use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use once_cell::sync::OnceCell;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use multivm::{MultivmTracer, VmInstance, VmInstanceData};
use vm::{
    CallTracer, ExecutionResult, FinishedL1Batch, Halt, HistoryEnabled, L1BatchEnv, L2BlockEnv,
    SystemEnv, VmExecutionResultAndLogs,
};
use zksync_dal::ConnectionPool;
use zksync_state::{ReadStorage, RocksdbStorage, StorageView};
use zksync_types::{vm_trace::Call, witness_block_state::WitnessBlockState, Transaction, U256};

use zksync_utils::bytecode::CompressedBytecodeInfo;

#[cfg(test)]
mod tests;

use crate::{
    gas_tracker::{gas_count_from_metrics, gas_count_from_tx_and_metrics},
    state_keeper::types::ExecutionMetricsForCriteria,
};

/// Representation of a transaction executed in the virtual machine.
#[derive(Debug, Clone)]
pub(crate) enum TxExecutionResult {
    /// Successful execution of the tx and the block tip dry run.
    Success {
        tx_result: Box<VmExecutionResultAndLogs>,
        tx_metrics: ExecutionMetricsForCriteria,
        bootloader_dry_run_metrics: ExecutionMetricsForCriteria,
        bootloader_dry_run_result: Box<VmExecutionResultAndLogs>,
        compressed_bytecodes: Vec<CompressedBytecodeInfo>,
        call_tracer_result: Vec<Call>,
    },
    /// The VM rejected the tx for some reason.
    RejectedByVm { reason: Halt },
    /// Bootloader gas limit is not enough to execute the tx.
    BootloaderOutOfGasForTx,
    /// Bootloader gas limit is enough to run the tx but not enough to execute block tip.
    BootloaderOutOfGasForBlockTip,
}

impl TxExecutionResult {
    /// Returns a revert reason if either transaction was rejected or bootloader ran out of gas.
    pub(super) fn err(&self) -> Option<&Halt> {
        match self {
            Self::Success { .. } => None,
            Self::RejectedByVm {
                reason: rejection_reason,
            } => Some(rejection_reason),
            Self::BootloaderOutOfGasForTx | Self::BootloaderOutOfGasForBlockTip { .. } => {
                Some(&Halt::BootloaderOutOfGas)
            }
        }
    }
}

/// An abstraction that allows us to create different kinds of batch executors.
/// The only requirement is to return a [`BatchExecutorHandle`], which does its work
/// by communicating with the externally initialized thread.
#[async_trait]
pub trait L1BatchExecutorBuilder: 'static + Send + Sync + fmt::Debug {
    async fn init_batch(
        &self,
        l1_batch_params: L1BatchEnv,
        system_env: SystemEnv,
    ) -> BatchExecutorHandle;
}

/// The default implementation of [`L1BatchExecutorBuilder`].
/// Creates a "real" batch executor which maintains the VM (as opposed to the test builder which doesn't use the VM).
#[derive(Debug, Clone)]
pub struct MainBatchExecutorBuilder {
    state_keeper_db_path: String,
    pool: ConnectionPool,
    save_call_traces: bool,
    max_allowed_tx_gas_limit: U256,
    upload_witness_inputs_to_gcs: bool,
}

impl MainBatchExecutorBuilder {
    pub fn new(
        state_keeper_db_path: String,
        pool: ConnectionPool,
        max_allowed_tx_gas_limit: U256,
        save_call_traces: bool,
        upload_witness_inputs_to_gcs: bool,
    ) -> Self {
        Self {
            state_keeper_db_path,
            pool,
            save_call_traces,
            max_allowed_tx_gas_limit,
            upload_witness_inputs_to_gcs,
        }
    }
}

#[async_trait]
impl L1BatchExecutorBuilder for MainBatchExecutorBuilder {
    async fn init_batch(
        &self,
        l1_batch_params: L1BatchEnv,
        system_env: SystemEnv,
    ) -> BatchExecutorHandle {
        let mut secondary_storage = RocksdbStorage::new(self.state_keeper_db_path.as_ref());
        let mut conn = self
            .pool
            .access_storage_tagged("state_keeper")
            .await
            .unwrap();
        secondary_storage.update_from_postgres(&mut conn).await;
        drop(conn);

        BatchExecutorHandle::new(
            self.save_call_traces,
            self.max_allowed_tx_gas_limit,
            secondary_storage,
            l1_batch_params,
            system_env,
            self.upload_witness_inputs_to_gcs,
        )
    }
}

/// A public interface for interaction with the `BatchExecutor`.
/// `BatchExecutorHandle` is stored in the state keeper and is used to invoke or rollback transactions, and also seal
/// the batches.
#[derive(Debug)]
pub struct BatchExecutorHandle {
    handle: JoinHandle<()>,
    commands: mpsc::Sender<Command>,
}

impl BatchExecutorHandle {
    // TODO: to be removed once testing in stage2 is done
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        save_call_traces: bool,
        max_allowed_tx_gas_limit: U256,
        secondary_storage: RocksdbStorage,
        l1_batch_env: L1BatchEnv,
        system_env: SystemEnv,
        upload_witness_inputs_to_gcs: bool,
    ) -> Self {
        // Since we process `BatchExecutor` commands one-by-one (the next command is never enqueued
        // until a previous command is processed), capacity 1 is enough for the commands channel.
        let (commands_sender, commands_receiver) = mpsc::channel(1);
        let executor = BatchExecutor {
            save_call_traces,
            max_allowed_tx_gas_limit,
            commands: commands_receiver,
        };

        let handle = tokio::task::spawn_blocking(move || {
            executor.run(
                secondary_storage,
                l1_batch_env,
                system_env,
                upload_witness_inputs_to_gcs,
            )
        });
        Self {
            handle,
            commands: commands_sender,
        }
    }

    /// Creates a batch executor handle from the provided sender and thread join handle.
    /// Can be used to inject an alternative batch executor implementation.
    #[cfg(test)]
    pub(super) fn from_raw(handle: JoinHandle<()>, commands: mpsc::Sender<Command>) -> Self {
        Self { handle, commands }
    }

    pub(super) async fn execute_tx(&self, tx: Transaction) -> TxExecutionResult {
        let tx_gas_limit = tx.gas_limit().as_u32();

        let (response_sender, response_receiver) = oneshot::channel();
        self.commands
            .send(Command::ExecuteTx(Box::new(tx), response_sender))
            .await
            .unwrap();

        let start = Instant::now();
        let res = response_receiver.await.unwrap();
        let elapsed = start.elapsed();

        metrics::histogram!("state_keeper.batch_executor.command_response_time", elapsed, "command" => "execute_tx");

        if let TxExecutionResult::Success { tx_metrics, .. } = res {
            metrics::histogram!(
                "state_keeper.computational_gas_per_nanosecond",
                tx_metrics.execution_metrics.computational_gas_used as f64
                    / elapsed.as_nanos() as f64
            );
        } else {
            // The amount of computational gas paid for failed transactions is hard to get
            // but comparing to the gas limit makes sense, since we can burn all gas
            // if some kind of failure is a DDoS vector otherwise.
            metrics::histogram!(
                "state_keeper.failed_tx_gas_limit_per_nanosecond",
                tx_gas_limit as f64 / elapsed.as_nanos() as f64
            );
        }

        res
    }

    pub(super) async fn start_next_miniblock(&self, miniblock_info: L2BlockEnv) {
        // While we don't get anything from the channel, it's useful to have it as a confirmation that the operation
        // indeed has been processed.
        let (response_sender, response_receiver) = oneshot::channel();
        self.commands
            .send(Command::StartNextMiniblock(miniblock_info, response_sender))
            .await
            .unwrap();
        let start = Instant::now();
        response_receiver.await.unwrap();
        metrics::histogram!("state_keeper.batch_executor.command_response_time", start.elapsed(), "command" => "start_next_miniblock");
    }

    pub(super) async fn rollback_last_tx(&self) {
        // While we don't get anything from the channel, it's useful to have it as a confirmation that the operation
        // indeed has been processed.
        let (response_sender, response_receiver) = oneshot::channel();
        self.commands
            .send(Command::RollbackLastTx(response_sender))
            .await
            .unwrap();
        let start = Instant::now();
        response_receiver.await.unwrap();
        metrics::histogram!("state_keeper.batch_executor.command_response_time", start.elapsed(), "command" => "rollback_last_tx");
    }

    pub(super) async fn finish_batch(self) -> (FinishedL1Batch, Option<WitnessBlockState>) {
        let (response_sender, response_receiver) = oneshot::channel();
        self.commands
            .send(Command::FinishBatch(response_sender))
            .await
            .unwrap();
        let start = Instant::now();
        let resp = response_receiver.await.unwrap();
        self.handle.await.unwrap();
        metrics::histogram!("state_keeper.batch_executor.command_response_time", start.elapsed(), "command" => "finish_batch");
        resp
    }
}

#[derive(Debug)]
pub(super) enum Command {
    ExecuteTx(Box<Transaction>, oneshot::Sender<TxExecutionResult>),
    StartNextMiniblock(L2BlockEnv, oneshot::Sender<()>),
    RollbackLastTx(oneshot::Sender<()>),
    FinishBatch(oneshot::Sender<(FinishedL1Batch, Option<WitnessBlockState>)>),
}

/// Implementation of the "primary" (non-test) batch executor.
/// Upon launch, it initializes the VM object with provided block context and properties, and keeps applying
/// transactions until the batch is sealed.
///
/// One `BatchExecutor` can execute exactly one batch, so once the batch is sealed, a new `BatchExecutor` object must
/// be constructed.
#[derive(Debug)]
pub(super) struct BatchExecutor {
    save_call_traces: bool,
    max_allowed_tx_gas_limit: U256,
    commands: mpsc::Receiver<Command>,
}

impl BatchExecutor {
    pub(super) fn run(
        mut self,
        secondary_storage: RocksdbStorage,
        l1_batch_params: L1BatchEnv,
        system_env: SystemEnv,
        upload_witness_inputs_to_gcs: bool,
    ) {
        tracing::info!("Starting executing batch #{:?}", &l1_batch_params.number);

        let storage_view = StorageView::new(secondary_storage).to_rc_ptr();

        let mut instance_data =
            VmInstanceData::new(storage_view.clone(), &system_env, HistoryEnabled);
        let mut vm = VmInstance::new(l1_batch_params, system_env, &mut instance_data);

        while let Some(cmd) = self.commands.blocking_recv() {
            match cmd {
                Command::ExecuteTx(tx, resp) => {
                    let result = self.execute_tx(&tx, &mut vm);
                    resp.send(result).unwrap();
                }
                Command::RollbackLastTx(resp) => {
                    self.rollback_last_tx(&mut vm);
                    resp.send(()).unwrap();
                }
                Command::StartNextMiniblock(l2_block_env, resp) => {
                    self.start_next_miniblock(l2_block_env, &mut vm);
                    resp.send(()).unwrap();
                }
                Command::FinishBatch(resp) => {
                    let vm_block_result = self.finish_batch(&mut vm);
                    let witness_block_state = if upload_witness_inputs_to_gcs {
                        Some(storage_view.borrow_mut().witness_block_state())
                    } else {
                        None
                    };
                    resp.send((vm_block_result, witness_block_state)).unwrap();

                    // storage_view cannot be accessed while borrowed by the VM,
                    // so this is the only point at which storage metrics can be obtained
                    let metrics = storage_view.as_ref().borrow_mut().metrics();
                    metrics::histogram!(
                        "state_keeper.batch_storage_interaction_duration",
                        metrics.time_spent_on_get_value,
                        "interaction" => "get_value"
                    );
                    metrics::histogram!(
                        "state_keeper.batch_storage_interaction_duration",
                        metrics.time_spent_on_set_value,
                        "interaction" => "set_value"
                    );

                    return;
                }
            }
        }
        // State keeper can exit because of stop signal, so it's OK to exit mid-batch.
        tracing::info!("State keeper exited with an unfinished batch");
    }

    fn execute_tx<S: ReadStorage>(
        &self,
        tx: &Transaction,
        vm: &mut VmInstance<'_, S, HistoryEnabled>,
    ) -> TxExecutionResult {
        // Save pre-`execute_next_tx` VM snapshot.
        vm.make_snapshot();

        // Reject transactions with too big gas limit.
        // They are also rejected on the API level, but
        // we need to secure ourselves in case some tx will somehow get into mempool.
        if tx.gas_limit() > self.max_allowed_tx_gas_limit {
            tracing::warn!(
                "Found tx with too big gas limit in state keeper, hash: {:?}, gas_limit: {}",
                tx.hash(),
                tx.gas_limit()
            );
            return TxExecutionResult::RejectedByVm {
                reason: Halt::TooBigGasLimit,
            };
        }

        // Execute the transaction.
        let stage_started_at = Instant::now();
        let (tx_result, compressed_bytecodes, call_tracer_result) = self.execute_tx_in_vm(tx, vm);
        metrics::histogram!(
            "server.state_keeper.tx_execution_time",
            stage_started_at.elapsed(),
            "stage" => "execution"
        );
        metrics::increment_counter!(
            "server.processed_txs",
            "stage" => "state_keeper"
        );
        metrics::counter!(
            "server.processed_l1_txs",
            tx.is_l1() as u64,
            "stage" => "state_keeper"
        );

        if let ExecutionResult::Halt { reason } = tx_result.result {
            return match reason {
                Halt::BootloaderOutOfGas => TxExecutionResult::BootloaderOutOfGasForTx,
                _ => TxExecutionResult::RejectedByVm { reason },
            };
        }

        let tx_metrics = Self::get_execution_metrics(Some(tx), &tx_result);

        let (bootloader_dry_run_result, bootloader_dry_run_metrics) = self.dryrun_block_tip(vm);
        match &bootloader_dry_run_result.result {
            ExecutionResult::Success { .. } => TxExecutionResult::Success {
                tx_result: Box::new(tx_result),
                tx_metrics,
                bootloader_dry_run_metrics,
                bootloader_dry_run_result: Box::new(bootloader_dry_run_result),
                compressed_bytecodes,
                call_tracer_result,
            },
            ExecutionResult::Revert { .. } => {
                unreachable!(
                    "VM must not revert when finalizing block (except `BootloaderOutOfGas`)"
                );
            }
            ExecutionResult::Halt { reason } => match reason {
                Halt::BootloaderOutOfGas => TxExecutionResult::BootloaderOutOfGasForBlockTip,
                _ => {
                    panic!("VM must not revert when finalizing block (except `BootloaderOutOfGas`)")
                }
            },
        }
    }

    fn rollback_last_tx<S: ReadStorage>(&self, vm: &mut VmInstance<'_, S, HistoryEnabled>) {
        let stage_started_at = Instant::now();
        vm.rollback_to_the_latest_snapshot();
        metrics::histogram!(
            "server.state_keeper.tx_execution_time",
            stage_started_at.elapsed(),
            "stage" => "tx_rollback"
        );
    }

    fn start_next_miniblock<S: ReadStorage>(
        &self,
        l2_block_env: L2BlockEnv,
        vm: &mut VmInstance<'_, S, HistoryEnabled>,
    ) {
        vm.start_new_l2_block(l2_block_env);
    }

    fn finish_batch<S: ReadStorage>(
        &self,
        vm: &mut VmInstance<'_, S, HistoryEnabled>,
    ) -> FinishedL1Batch {
        // The vm execution was paused right after the last transaction was executed.
        // There is some post-processing work that the VM needs to do before the block is fully processed.
        let result = vm.finish_batch();
        if result.block_tip_execution_result.result.is_failed() {
            panic!("VM must not fail when finalizing block");
        }
        result
    }

    // Err when transaction is rejected.
    // Ok(TxExecutionStatus::Success) when the transaction succeeded
    // Ok(TxExecutionStatus::Failure) when the transaction failed.
    // Note that failed transactions are considered properly processed and are included in blocks
    fn execute_tx_in_vm<S: ReadStorage>(
        &self,
        tx: &Transaction,
        vm: &mut VmInstance<'_, S, HistoryEnabled>,
    ) -> (
        VmExecutionResultAndLogs,
        Vec<CompressedBytecodeInfo>,
        Vec<Call>,
    ) {
        // Note, that the space where we can put the calldata for compressing transactions
        // is limited and the transactions do not pay for taking it.
        // In order to not let the accounts spam the space of compressed bytecodes with bytecodes
        // that will not be published (e.g. due to out of gas), we use the following scheme:
        // We try to execute the transaction with compressed bytecodes.
        // If it fails and the compressed bytecodes have not been published,
        // it means that there is no sense in pollutting the space of compressed bytecodes,
        // and so we reexecute the transaction, but without compressions.

        // Saving the snapshot before executing
        vm.make_snapshot();

        let call_tracer_result = Arc::new(OnceCell::default());
        let custom_tracers = if self.save_call_traces {
            vec![CallTracer::new(call_tracer_result.clone(), HistoryEnabled).into_boxed()]
        } else {
            vec![]
        };
        if let Ok(result) =
            vm.inspect_transaction_with_bytecode_compression(custom_tracers, tx.clone(), true)
        {
            let compressed_bytecodes = vm.get_last_tx_compressed_bytecodes();
            vm.pop_snapshot_no_rollback();

            let trace = Arc::try_unwrap(call_tracer_result)
                .unwrap()
                .take()
                .unwrap_or_default();
            return (result, compressed_bytecodes, trace);
        }

        let call_tracer_result = Arc::new(OnceCell::default());
        let custom_tracers = if self.save_call_traces {
            vec![CallTracer::new(call_tracer_result.clone(), HistoryEnabled).into_boxed()]
        } else {
            vec![]
        };
        vm.rollback_to_the_latest_snapshot();
        let result = vm
            .inspect_transaction_with_bytecode_compression(custom_tracers, tx.clone(), false)
            .expect("Compression can't fail if we don't apply it");
        let compressed_bytecodes = vm.get_last_tx_compressed_bytecodes();

        // TODO implement tracer manager which will be responsible
        // for collecting result from all tracers and save it to the database
        let trace = Arc::try_unwrap(call_tracer_result)
            .unwrap()
            .take()
            .unwrap_or_default();
        (result, compressed_bytecodes, trace)
    }

    fn dryrun_block_tip<S: ReadStorage>(
        &self,
        vm: &mut VmInstance<'_, S, HistoryEnabled>,
    ) -> (VmExecutionResultAndLogs, ExecutionMetricsForCriteria) {
        let started_at = Instant::now();
        let mut stage_started_at = Instant::now();

        // Save pre-`execute_till_block_end` VM snapshot.
        vm.make_snapshot();

        metrics::histogram!(
            "server.state_keeper.tx_execution_time",
            stage_started_at.elapsed(),
            "stage" => "dryrun_make_snapshot",
        );
        stage_started_at = Instant::now();

        let block_tip_result = vm.execute_block_tip();

        metrics::histogram!(
            "server.state_keeper.tx_execution_time",
            stage_started_at.elapsed(),
            "stage" => "dryrun_execute_block_tip",
        );
        stage_started_at = Instant::now();

        let metrics = Self::get_execution_metrics(None, &block_tip_result);

        metrics::histogram!(
            "server.state_keeper.tx_execution_time",
            stage_started_at.elapsed(),
            "stage" => "dryrun_get_execution_metrics",
        );
        stage_started_at = Instant::now();

        // Rollback to the pre-`execute_till_block_end` state.
        vm.rollback_to_the_latest_snapshot();

        metrics::histogram!(
            "server.state_keeper.tx_execution_time",
            stage_started_at.elapsed(),
            "stage" => "dryrun_rollback_to_the_latest_snapshot"
        );

        metrics::histogram!(
            "server.state_keeper.tx_execution_time",
            started_at.elapsed(),
            "stage" => "dryrun_rollback"
        );

        (block_tip_result, metrics)
    }

    fn get_execution_metrics(
        tx: Option<&Transaction>,
        execution_result: &VmExecutionResultAndLogs,
    ) -> ExecutionMetricsForCriteria {
        let execution_metrics = execution_result.get_execution_metrics(tx);
        let l1_gas = match tx {
            Some(tx) => gas_count_from_tx_and_metrics(tx, &execution_metrics),
            None => gas_count_from_metrics(&execution_metrics),
        };

        ExecutionMetricsForCriteria {
            l1_gas,
            execution_metrics,
        }
    }
}
