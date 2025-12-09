use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{oneshot, Mutex, RwLock};
use tokio::task::JoinHandle;

pub mod stream;
pub mod usage;

pub use usage::Uuid;

use crate::stream::{OrbitNode, OrbitServer, OrbitStream};

/// Rate limit configuration for Orbit protocol
#[derive(Clone)]
pub struct OrbitConfig {
    /// Address of this Orbit node server
    pub server_addr: SocketAddr,
    /// Address of the previous node in the ring (to pull usage table from)
    pub prev_node_addr: SocketAddr,

    /// Total allocation per epoch across all nodes
    /// Rate limit is strict: (orbit_limit) * (node_count)
    /// must not be exceeded within (node_count) * (epoch_duration_ms).
    /// By removing (node_count) from both sides, an average rate limit control can be achieved.
    pub orbit_limit: u64,
    /// Number of nodes in the ring
    pub node_count: u64,
    /// Rollover ratio (0.0 ~ 1.0)
    /// Each node must leave this % of remaining capacity for the next node.
    pub rollover_ratio: f64,
    /// Epoch duration in milliseconds
    pub epoch_duration_ms: u64,
}

impl Default for OrbitConfig {
    fn default() -> Self {
        Self {
            server_addr: SocketAddr::from(([127, 0, 0, 1], 1119)),
            prev_node_addr: SocketAddr::from(([127, 0, 0, 1], 1118)),
            orbit_limit: 1000,
            node_count: 10,
            rollover_ratio: 0.15,
            epoch_duration_ms: 1000,
        }
    }
}

/// Internal state of the rate limiter
struct OrbitInner {
    config: OrbitConfig,

    /// Current epoch number
    current_epoch: AtomicU64,

    /// send_table's epoch number
    data_epoch: AtomicU64,

    /// Current epoch usage per UUID
    /// Read/Updated on each request
    ///
    /// If the lock on current_epoch_usage is held, requests will wait until the lock is released.
    current_epoch_usage: Mutex<BTreeMap<crate::Uuid, u64>>,

    /// Usage table received from the previous node
    /// At the epoch boundary, this is read from the previous node
    /// If the Write Lock on received_table is held, requests will restricted until the lock is
    /// released.
    received_table: RwLock<Option<usage::EpochUsageTable>>,

    /// Usage table to send to the next node 
    /// At the epoch boundary, this is populated from current_epoch_usage + received_table
    /// and then current_epoch_usage is reset.
    send_table: Mutex<Option<usage::EpochUsageTable>>,
}

/// Rate limit manager for Orbit protocol
///
/// Can be shared across threads via `Arc<Orbit>`.
/// Use `OrbitHandle` to stop the epoch management task.
pub struct Orbit {
    inner: Arc<OrbitInner>,
}

/// Handle for controlling the Orbit lifecycle.
///
/// Owns the shutdown channel and task handles.
/// Call `stop()` to gracefully shutdown the epoch management and server tasks.
pub struct OrbitHandle {
    epoch_task: Option<JoinHandle<()>>,
    server_task: Option<JoinHandle<()>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl Orbit {
    /// Creates a new Orbit rate limiter and starts the epoch management task and server.
    ///
    /// Returns a tuple of (Orbit, OrbitHandle).
    /// - `Orbit` can be wrapped in `Arc` for sharing across threads.
    /// - `OrbitHandle` should be kept to stop the tasks on shutdown.
    pub fn new(config: OrbitConfig) -> Result<(Self, OrbitHandle), stream::OrbitStreamError> {
        let inner = OrbitInner {
            config: config.clone(),
            current_epoch: AtomicU64::new(Self::calculate_epoch(config.epoch_duration_ms)),
            data_epoch: AtomicU64::new(0),
            send_table: Mutex::new(None),
            received_table: RwLock::new(None),
            current_epoch_usage: Mutex::new(BTreeMap::new()),
        };

        let inner = Arc::new(inner);

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let epoch_task = Self::spawn_epoch_task(
            inner.clone(),
            config.epoch_duration_ms,
            shutdown_rx
        );

        // Start server
        let server = OrbitServer::new(config.server_addr)?;
        let server_inner = inner.clone();
        let server_task = tokio::spawn(async move {
            if let Err(e) = server.run(server_inner).await {
                tracing::error!("OrbitServer error: {}", e);
            }
        });

        let orbit = Self { inner };
        let handle = OrbitHandle {
            epoch_task: Some(epoch_task),
            server_task: Some(server_task),
            shutdown_tx: Some(shutdown_tx),
        };

        Ok((orbit, handle))
    }

    /// Returns current epoch number based on system time.
    /// Epoch = (unix_timestamp_ms / epoch_duration_ms)
    fn calculate_epoch(epoch_duration_ms: u64) -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("System time before UNIX epoch");
        now.as_millis() as u64 / epoch_duration_ms
    }

    /// Returns milliseconds until the next epoch boundary.
    fn ms_until_next_epoch(epoch_duration_ms: u64) -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("System time before UNIX epoch");
        let current_ms = now.as_millis() as u64;
        epoch_duration_ms - (current_ms % epoch_duration_ms)
    }

    /// Spawns the epoch management task.
    /// Synchronizes to system time boundaries (e.g., minute boundaries).
    fn spawn_epoch_task(
        inner: Arc<OrbitInner>,
        epoch_duration_ms: u64,
        mut shutdown_rx: oneshot::Receiver<()>,
    ) -> JoinHandle<()> {
        inner.current_epoch
            .store(Self::calculate_epoch(epoch_duration_ms), std::sync::atomic::Ordering::SeqCst);
        tokio::spawn(async move {
            // Persistent connection to the previous node
            let mut prev_node_stream: Option<OrbitStream> = None;

            // Wait for initial epoch boundary before starting
            let initial_sleep_ms = Self::ms_until_next_epoch(epoch_duration_ms);
            tokio::time::sleep(Duration::from_millis(initial_sleep_ms)).await;

            loop {
                let current_epoch = inner.current_epoch.load(std::sync::atomic::Ordering::SeqCst);
                tracing::info!("Epoch boundary reached, processing epoch transition for epoch {current_epoch}");

                // send_table Mutex Lock (Phase 1)
                // current_epoch_usage: Mutex Lock (Phase 1 ~ 2)
                // received_table: RwLock Write Lock (Phase 1 ~ 3)
                let mut send_table_lock = inner.send_table.lock().await;
                let mut received_table_lock = inner.received_table.write().await;
                let mut current_usage = inner.current_epoch_usage.lock().await;

                // Phase 1. Fill to_send_table for next node
                // If received_table exists, append current usage to it
                if let Some(mut table) = received_table_lock.take() {
                    table.push_epoch(usage::EpochUsage::from_btree_map(
                        current_epoch,
                        &current_usage,
                    ));
                    send_table_lock.replace(table);
                // Otherwise, create new table from current usage
                } else {
                    let mut new_table = usage::EpochUsageTable::new(
                        current_epoch,
                        inner.config.node_count as usize,
                    );
                    new_table.push_epoch(usage::EpochUsage::from_btree_map(
                        current_epoch,
                        &current_usage,
                    ));
                    send_table_lock.replace(new_table);
                }

                // Update data_epoch to current_epoch
                inner.data_epoch.store(current_epoch, std::sync::atomic::Ordering::SeqCst);

                // Send table is ready, release lock
                drop(send_table_lock);

                // Phase 2. Reset current_epoch_usage for next epoch
                current_usage.clear();
                // Current usage reset done, release lock
                drop(current_usage);

                // Small delay to ensure previous node has updated
                tokio::time::sleep(Duration::from_millis(50)).await;

                // Phase 3. Pull usage table from previous node
                // On failure, keep received_table_lock held until next epoch boundary
                let prev_addr = inner.config.prev_node_addr;
                let pull_timeout = Duration::from_millis(epoch_duration_ms / 5);

                let mut pull_success = false;

                // Ensure we have a connection to the previous node
                if prev_node_stream.is_none() {
                    match OrbitStream::connect(prev_addr, pull_timeout).await {
                        Ok(stream) => {
                            tracing::info!("Connected to previous node at {prev_addr}");
                            prev_node_stream = Some(stream);
                        }
                        Err(e) => {
                            tracing::error!("Failed to connect to previous node at {prev_addr}: {e}");
                        }
                    }
                }

                // Pull usage table from previous node
                if let Some(ref stream) = prev_node_stream {
                    match stream.get_usage_table(current_epoch).await {
                        Ok(table) => {
                            tracing::info!(
                                "Received usage table from previous node for epoch {current_epoch} with {} entries",
                                table.tables.len()
                            );
                            // Update received_table for next epoch
                            *received_table_lock = Some(table);
                            pull_success = true;
                        }
                        Err(e) => {
                            tracing::error!("Error pulling usage table from previous node: {e}");
                            // Reset connection on error
                            if let Some(stream) = prev_node_stream.take() {
                                stream.close();
                            }
                        }
                    }
                }

                // On success, release the lock immediately
                if pull_success {
                    drop(received_table_lock);
                } else {
                    todo!("Table pull failed");
                }

                // Update epoch based on system time
                let new_epoch = Self::calculate_epoch(epoch_duration_ms);
                inner.current_epoch.store(new_epoch, std::sync::atomic::Ordering::SeqCst);

                tracing::info!("Epoch {new_epoch} started (duration: {epoch_duration_ms} ms)");

                // Sleep until next epoch boundary, but wake up immediately on shutdown
                // On pull failure, received_table_lock is held during this sleep
                let sleep_ms = Self::ms_until_next_epoch(epoch_duration_ms);
                let sleep_future = tokio::time::sleep(Duration::from_millis(sleep_ms));

                tokio::select! {
                    _ = sleep_future => {
                        // Epoch boundary reached
                    }
                    _ = &mut shutdown_rx => {
                        tracing::info!("Epoch task received shutdown signal");
                        // Close connection if exists
                        if let Some(stream) = prev_node_stream.take() {
                            stream.close();
                        }
                        break;
                    }
                }
            }
        })
    }

    /// Checks if a request is allowed for a UUID and increments the counter if so.
    ///
    /// Returns: true if the request is allowed, false if rate limited
    pub async fn check_and_increment(&self, uuid: &crate::Uuid, amount: u64) -> bool {
        let mut usage_lock = self.inner.current_epoch_usage.lock().await;

        // Calculate available quota
        let available_quota = {
            let table = self.inner.received_table.try_read();
            match table {
                Ok(t) => match &*t {
                    Some(table) => {
                        // Total usage from previous nodes in the sliding window
                        let total_usage = table.get_usage(uuid);

                        // Remaining capacity after previous nodes
                        let remaining = self.inner.config.orbit_limit.saturating_sub(total_usage);

                        // Must leave rollover_ratio % for next node
                        let reserved = (remaining as f64 * self.inner.config.rollover_ratio) as u64;
                        remaining.saturating_sub(reserved)
                    }
                    None => {
                        let remaining = self.inner.config.orbit_limit;
                        let reserved = (remaining as f64 * self.inner.config.rollover_ratio) as u64;
                        remaining.saturating_sub(reserved)
                    }
                },
                Err(_) => {
                    // Write lock is held - release usage_lock and wait for read access
                    drop(usage_lock);
                    let table = self.inner.received_table.read().await;
                    let quota = match &*table {
                        Some(t) => {
                            let total_usage = t.get_usage(uuid);
                            let remaining = self.inner.config.orbit_limit.saturating_sub(total_usage);
                            let reserved = (remaining as f64 * self.inner.config.rollover_ratio) as u64;
                            remaining.saturating_sub(reserved)
                        }
                        None => {
                            let remaining = self.inner.config.orbit_limit;
                            let reserved = (remaining as f64 * self.inner.config.rollover_ratio) as u64;
                            remaining.saturating_sub(reserved)
                        }
                    };
                    drop(table);
                    usage_lock = self.inner.current_epoch_usage.lock().await;
                    quota
                }
            }
        };

        let current_usage = usage_lock.get(uuid).unwrap_or(&0).clone();
        if current_usage + amount <= available_quota {
            // Allowed, increment usage
            let entry = usage_lock.entry(*uuid).or_insert(0);
            *entry += amount;
            return true;
        } else {
            // Rate limited
            return false;
        }
    }
}

impl OrbitHandle {
    /// Stops the epoch management task and server.
    pub fn stop(&mut self) {
        // Send shutdown signal to wake up the epoch task immediately
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }

        // Abort server task
        if let Some(handle) = self.server_task.take() {
            handle.abort();
        }

        // Wait for the epoch task to finish
        if let Some(handle) = self.epoch_task.take() {
            // Use block_in_place to allow blocking in async context
            let _ = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(handle)
            });
        }
    }
}

impl OrbitNode for OrbitInner {
    async fn get_usage_table(
            &self,
            epoch: u64,
        ) -> Result<usage::EpochUsageTable, stream::OrbitStreamError> {
        let current_epoch = self.data_epoch.load(std::sync::atomic::Ordering::SeqCst);
        if epoch != current_epoch {
            return Err(stream::OrbitStreamError::EpochMismatch {
                requested: epoch,
                server: current_epoch,
            });
        }
        if let Some(table) = self.send_table.lock().await.take() {
            Ok(table)
        } else {
            Err(stream::OrbitStreamError::TableNotExist(current_epoch))
        }
    }
}

impl Drop for OrbitHandle {
    fn drop(&mut self) {
        self.stop();
    }
}
