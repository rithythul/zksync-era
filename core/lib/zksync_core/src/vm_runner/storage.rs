use std::{
    collections::{BTreeMap, HashMap},
    fmt::Debug,
    marker::PhantomData,
    sync::Arc,
    time::Duration,
};

use anyhow::Context as _;
use async_trait::async_trait;
use multivm::{interface::L1BatchEnv, vm_1_4_2::SystemEnv};
use once_cell::sync::OnceCell;
use tokio::sync::{watch, RwLock};
use vm_utils::storage::L1BatchParamsProvider;
use zksync_dal::{Connection, ConnectionPool, Core, CoreDal};
use zksync_state::{
    AsyncCatchupTask, BatchDiff, PgOrRocksdbStorage, ReadStorageFactory, RocksdbStorage,
    RocksdbStorageBuilder, RocksdbStorageOptions, RocksdbWithMemory, StateKeeperColumnFamily,
};
use zksync_storage::RocksDB;
use zksync_types::{block::L2BlockExecutionData, L1BatchNumber, L2ChainId};

/// Data needed to execute an L1 batch.
#[derive(Debug, Clone)]
pub struct BatchExecuteData {
    /// Parameters for L1 batch this data belongs to.
    pub l1_batch_env: L1BatchEnv,
    /// Execution process parameters.
    pub system_env: SystemEnv,
    /// List of L2 blocks and corresponding transactions that were executed within batch.
    pub l2_blocks: Vec<L2BlockExecutionData>,
}

#[derive(Debug, Clone)]
struct BatchData {
    execute_data: BatchExecuteData,
    diff: BatchDiff,
}

/// Functionality to fetch data about processed/unprocessed batches for a particular VM runner
/// instance.
#[async_trait]
pub trait VmRunnerStorageLoader: Debug + Send + Sync + 'static {
    /// Unique name of the VM runner instance.
    fn name() -> &'static str;

    /// Returns the last L1 batch number that has been processed by this VM runner instance.
    ///
    /// # Errors
    ///
    /// Propagates DB errors.
    async fn latest_processed_batch(
        &self,
        conn: &mut Connection<'_, Core>,
    ) -> anyhow::Result<L1BatchNumber>;

    /// Returns the last L1 batch number that is ready to be loaded by this VM runner instance.
    ///
    /// # Errors
    ///
    /// Propagates DB errors.
    async fn last_ready_to_be_loaded_batch(
        &self,
        conn: &mut Connection<'_, Core>,
    ) -> anyhow::Result<L1BatchNumber>;

    async fn mark_l1_batch_as_completed(
        &self,
        conn: &mut Connection<'_, Core>,
        l1_batch_number: L1BatchNumber,
    ) -> anyhow::Result<()>;
}

/// Abstraction for VM runner's storage layer that provides two main features:
///
/// 1. A [`ReadStorageFactory`] implementation backed by either Postgres or RocksDB (if it's
/// caught up). Always initialized as a `Postgres` variant and is then mutated into `Rocksdb`
/// once RocksDB cache is caught up.
/// 2. Loads data needed to re-execute the next unprocessed L1 batch.
///
/// Users of `VmRunnerStorage` are not supposed to retain storage access to batches that are less
/// than `L::latest_processed_batch`. Holding one is considered to be an undefined behavior.
#[derive(Debug)]
pub struct VmRunnerStorage<L: VmRunnerStorageLoader> {
    pool: ConnectionPool<Core>,
    l1_batch_params_provider: L1BatchParamsProvider,
    chain_id: L2ChainId,
    state: Arc<RwLock<State>>,
    _marker: PhantomData<L>,
}

#[derive(Debug)]
struct State {
    rocksdb: Option<RocksdbStorage>,
    l1_batch_number: L1BatchNumber,
    storage: BTreeMap<L1BatchNumber, BatchData>,
}

impl State {
    /// Whether this state can serve as a `ReadStorage` source for the given L1 batch.
    fn can_be_used_for_l1_batch(&self, l1_batch_number: L1BatchNumber) -> bool {
        l1_batch_number == self.l1_batch_number || self.storage.contains_key(&l1_batch_number)
    }
}

impl<L: VmRunnerStorageLoader> VmRunnerStorage<L> {
    /// Creates a new VM runner storage using provided Postgres pool and RocksDB path.
    pub async fn new(
        pool: ConnectionPool<Core>,
        rocksdb_path: String,
        loader: L,
        chain_id: L2ChainId,
    ) -> anyhow::Result<(Self, StorageSyncTask<L>)> {
        let mut conn = pool.connection_tagged(L::name()).await?;
        let l1_batch_params_provider = L1BatchParamsProvider::new(&mut conn)
            .await
            .context("Failed initializing L1 batch params provider")?;
        drop(conn);
        let state = Arc::new(RwLock::new(State {
            rocksdb: None,
            l1_batch_number: L1BatchNumber(0),
            storage: BTreeMap::new(),
        }));
        let task =
            StorageSyncTask::new(pool.clone(), chain_id, rocksdb_path, loader, state.clone())
                .await?;
        Ok((
            Self {
                pool,
                l1_batch_params_provider,
                chain_id,
                state,
                _marker: PhantomData,
            },
            task,
        ))
    }

    async fn access_storage_inner(
        &self,
        _stop_receiver: &watch::Receiver<bool>,
        l1_batch_number: L1BatchNumber,
    ) -> anyhow::Result<Option<PgOrRocksdbStorage<'_>>> {
        let state = self.state.read().await;
        let Some(rocksdb) = &state.rocksdb else {
            return Ok(Some(
                PgOrRocksdbStorage::access_storage_pg(&self.pool, l1_batch_number)
                    .await
                    .context("Failed accessing Postgres storage")?,
            ));
        };
        if !state.can_be_used_for_l1_batch(l1_batch_number) {
            tracing::debug!(
                %l1_batch_number,
                min_l1_batch = %state.l1_batch_number,
                max_l1_batch = %state.storage.last_key_value().map(|(k, _)| *k).unwrap_or(state.l1_batch_number),
                "Trying to access VM runner storage with L1 batch that is not available",
            );
            return Ok(None);
        }
        let batch_diffs = state
            .storage
            .iter()
            .filter_map(|(x, y)| {
                if x <= &l1_batch_number {
                    Some(y.diff.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        Ok(Some(PgOrRocksdbStorage::RocksdbWithMemory(
            RocksdbWithMemory {
                rocksdb: rocksdb.clone(),
                batch_diffs,
            },
        )))
    }

    /// Loads next unprocessed L1 batch along with all transactions that VM runner needs to
    /// re-execute. These are the transactions that are included in a sealed L2 block belonging
    /// to a sealed L1 batch (with state keeper being the source of truth). The order of the
    /// transactions is the same as it was when state keeper executed them.
    ///
    /// Can return `None` if there are no batches to be processed.
    ///
    /// # Errors
    ///
    /// Propagates DB errors.
    pub async fn load_batch(
        &self,
        l1_batch_number: L1BatchNumber,
    ) -> anyhow::Result<Option<BatchExecuteData>> {
        let state = self.state.read().await;
        if state.rocksdb.is_none() {
            let mut conn = self.pool.connection_tagged(L::name()).await?;
            return StorageSyncTask::<L>::load_batch_execute_data(
                &mut conn,
                l1_batch_number,
                &self.l1_batch_params_provider,
                self.chain_id,
            )
            .await;
        }
        match state.storage.get(&l1_batch_number) {
            None => {
                tracing::debug!(
                    %l1_batch_number,
                    min_l1_batch = %(state.l1_batch_number + 1),
                    max_l1_batch = %state.storage.last_key_value().map(|(k, _)| *k).unwrap_or(state.l1_batch_number),
                    "Trying to load an L1 batch that is not available"
                );
                Ok(None)
            }
            Some(batch_data) => Ok(Some(batch_data.execute_data.clone())),
        }
    }
}

#[async_trait]
impl<L: VmRunnerStorageLoader> ReadStorageFactory for VmRunnerStorage<L> {
    async fn access_storage(
        &self,
        stop_receiver: &watch::Receiver<bool>,
        l1_batch_number: L1BatchNumber,
    ) -> anyhow::Result<Option<PgOrRocksdbStorage<'_>>> {
        self.access_storage_inner(stop_receiver, l1_batch_number)
            .await
    }
}

/// A runnable task that catches up the provided RocksDB cache instance to the latest processed
/// batch and then continuously makes sure that this invariant is held for the foreseeable future.
/// In the meanwhile, `StorageSyncTask` also loads the next `max_batches_to_load` batches in memory
/// so that they are immediately accessible by [`VmRunnerStorage`].
#[derive(Debug)]
pub struct StorageSyncTask<L: VmRunnerStorageLoader> {
    pool: ConnectionPool<Core>,
    l1_batch_params_provider: L1BatchParamsProvider,
    chain_id: L2ChainId,
    rocksdb_cell: Arc<OnceCell<RocksDB<StateKeeperColumnFamily>>>,
    loader: L,
    state: Arc<RwLock<State>>,
    catchup_task: AsyncCatchupTask,
}

impl<L: VmRunnerStorageLoader> StorageSyncTask<L> {
    async fn new(
        pool: ConnectionPool<Core>,
        chain_id: L2ChainId,
        rocksdb_path: String,
        loader: L,
        state: Arc<RwLock<State>>,
    ) -> anyhow::Result<Self> {
        let mut conn = pool.connection_tagged(L::name()).await?;
        let l1_batch_params_provider = L1BatchParamsProvider::new(&mut conn)
            .await
            .context("Failed initializing L1 batch params provider")?;
        let rocksdb_cell = Arc::new(OnceCell::new());
        let catchup_task = AsyncCatchupTask::new(
            pool.clone(),
            rocksdb_path,
            RocksdbStorageOptions::default(),
            rocksdb_cell.clone(),
            Some(loader.latest_processed_batch(&mut conn).await?),
        );
        drop(conn);
        Ok(Self {
            pool,
            l1_batch_params_provider,
            chain_id,
            rocksdb_cell,
            loader,
            state,
            catchup_task,
        })
    }

    pub async fn run(self, stop_receiver: watch::Receiver<bool>) -> anyhow::Result<()> {
        const SLEEP_INTERVAL: Duration = Duration::from_millis(50);

        self.catchup_task.run(stop_receiver.clone()).await?;
        let rocksdb = self.rocksdb_cell.get().ok_or_else(|| {
            anyhow::anyhow!("Expected RocksDB to be initialized by `AsyncCatchupTask`")
        })?;
        loop {
            if *stop_receiver.borrow() {
                tracing::info!("`StorageSyncTask` was interrupted");
                return Ok(());
            }
            let mut conn = self.pool.connection_tagged(L::name()).await?;
            let latest_processed_batch = self.loader.latest_processed_batch(&mut conn).await?;
            let rocksdb_builder = RocksdbStorageBuilder::from_rocksdb(rocksdb.clone());
            if rocksdb_builder.l1_batch_number().await == Some(latest_processed_batch + 1) {
                // RocksDB is already caught up, we might not need to do anything.
                // Just need to check that the memory diff is up-to-date in case this is a fresh start.
                let state = self.state.read().await;
                if state
                    .storage
                    .contains_key(&self.loader.last_ready_to_be_loaded_batch(&mut conn).await?)
                {
                    // No need to do anything, killing time until last processed batch is updated.
                    drop(conn);
                    drop(state);
                    tokio::time::sleep(SLEEP_INTERVAL).await;
                    continue;
                }
            }
            // We rely on the assumption that no one is holding storage access to a batch with
            // number less than `latest_processed_batch`. If they do, RocksDB synchronization below
            // will cause them to have an inconsistent view on DB which we consider to be an
            // undefined behavior.
            let rocksdb = rocksdb_builder
                .synchronize(&mut conn, &stop_receiver, Some(latest_processed_batch))
                .await
                .context("Failed to catch up state keeper RocksDB storage to Postgres")?;
            let Some(rocksdb) = rocksdb else {
                tracing::info!("`StorageSyncTask` was interrupted during RocksDB synchronization");
                return Ok(());
            };
            let mut state = self.state.write().await;
            state.rocksdb = Some(rocksdb);
            state.l1_batch_number = latest_processed_batch;
            state
                .storage
                .retain(|l1_batch_number, _| l1_batch_number > &latest_processed_batch);
            let max_present = state
                .storage
                .last_entry()
                .map(|e| *e.key())
                .unwrap_or(latest_processed_batch);
            drop(state);
            let max_desired = self.loader.last_ready_to_be_loaded_batch(&mut conn).await?;
            for l1_batch_number in max_present.0 + 1..=max_desired.0 {
                let l1_batch_number = L1BatchNumber(l1_batch_number);
                let Some(execute_data) = Self::load_batch_execute_data(
                    &mut conn,
                    l1_batch_number,
                    &self.l1_batch_params_provider,
                    self.chain_id,
                )
                .await?
                else {
                    break;
                };
                let state_diff = conn
                    .storage_logs_dal()
                    .get_touched_slots_for_l1_batch(l1_batch_number)
                    .await?;
                let enum_index_diff = conn
                    .storage_logs_dedup_dal()
                    .initial_writes_for_batch(l1_batch_number)
                    .await?
                    .into_iter()
                    .collect::<HashMap<_, _>>();
                let factory_dep_diff = conn
                    .blocks_dal()
                    .get_l1_batch_factory_deps(l1_batch_number)
                    .await?;
                let diff = BatchDiff {
                    state_diff,
                    enum_index_diff,
                    factory_dep_diff,
                };

                let mut state = self.state.write().await;
                state
                    .storage
                    .insert(l1_batch_number, BatchData { execute_data, diff });
                drop(state);
            }
            drop(conn);
        }
    }

    async fn load_batch_execute_data(
        conn: &mut Connection<'_, Core>,
        l1_batch_number: L1BatchNumber,
        l1_batch_params_provider: &L1BatchParamsProvider,
        chain_id: L2ChainId,
    ) -> anyhow::Result<Option<BatchExecuteData>> {
        let first_l2_block_in_batch = l1_batch_params_provider
            .load_first_l2_block_in_batch(conn, l1_batch_number)
            .await
            .with_context(|| {
                format!(
                    "Failed loading first L2 block for L1 batch #{}",
                    l1_batch_number
                )
            })?;
        let Some(first_l2_block_in_batch) = first_l2_block_in_batch else {
            return Ok(None);
        };
        let (system_env, l1_batch_env) = l1_batch_params_provider
            .load_l1_batch_params(
                conn,
                &first_l2_block_in_batch,
                // `validation_computational_gas_limit` is only relevant when rejecting txs, but we
                // are re-executing so none of them should be rejected
                u32::MAX,
                chain_id,
            )
            .await
            .with_context(|| format!("Failed loading params for L1 batch #{}", l1_batch_number))?;
        let l2_blocks = conn
            .transactions_dal()
            .get_l2_blocks_to_execute_for_l1_batch(l1_batch_number)
            .await?;
        Ok(Some(BatchExecuteData {
            l1_batch_env,
            system_env,
            l2_blocks,
        }))
    }
}
