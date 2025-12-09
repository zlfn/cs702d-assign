use rkyv::{Archive, Deserialize, Serialize};

/// 16-byte UUID type, compatible with uuid::Uuid via `from_bytes` / `into_bytes`
pub type Uuid = [u8; 16];

#[derive(Archive, Deserialize, Serialize, Debug, Clone)]
pub struct EpochUsage {
    pub epoch: u64,
    pub usages: Vec<(Uuid, u64)>,
}

/// Ring buffer for storing usage epochs with fixed capacity.
/// When capacity is exceeded, the oldest epoch is removed.
#[derive(Archive, Deserialize, Serialize, Debug, Clone)]
pub struct EpochUsageTable {
    pub recent_epoch: u64,
    pub node_count: usize,
    pub tables: Vec<EpochUsage>,
}

impl EpochUsage {
    /// Create EpochUsage from a vector of (node_id, usage) tuples.
    /// Usages must be sorted by node_id for binary search to work.
    pub fn from_vec(epoch: u64, usages: Vec<(Uuid, u64)>) -> Self {
        Self {
            epoch,
            usages: usages
        }
    }

    /// Create EpochUsage from a BTreeMap of node_id to usage.
    pub fn from_btree_map(epoch: u64, usages: &std::collections::BTreeMap<Uuid, u64>) -> Self {
        Self {
            epoch,
            usages: usages.iter().map(|(k, v)| (*k, *v)).collect(),
        }
    }

    /// Get usage for a specific node_id using binary search.
    /// Returns None if node_id is not found.
    /// Usages must be sorted by node_id for binary search to work.
    pub fn get_usage(&self, node_id: &Uuid) -> Option<u64> {
        let idx = self.usages.binary_search_by_key(&node_id, |&(ref id, _)| id).ok()?;
        Some(self.usages[idx].1)
    }
}

impl EpochUsageTable {
    pub fn new(recent_epoch: u64, capacity: usize) -> Self {
        Self {
            recent_epoch,
            node_count: capacity,
            tables: Vec::with_capacity(capacity),
        }
    }

    /// Push a new epoch to the ring buffer.
    /// If capacity is exceeded, the oldest epoch (last element) is removed.
    pub fn push_epoch(&mut self, epoch: EpochUsage) {
        self.recent_epoch = epoch.epoch;
        if self.tables.len() >= self.node_count - 1 {
            self.tables.pop();
        }
        self.tables.insert(0, epoch);
    }

    /// Get total usage for a specific node_id across all epochs.
    pub fn get_usage(&self, node_id: &Uuid) -> u64 {
        let mut total_usage: u64 = 0;
        for epoch in &self.tables {
            if let Some(usage) = epoch.get_usage(node_id) {
                total_usage += usage;
            }
        }
        total_usage
    }

    /// Get total usage excluding the oldest epoch if it will be dropped when passed to next node.
    /// This represents the usage that the next node will actually see after we add our usage.
    pub fn get_usage_excluding_oldest(&self, node_id: &Uuid) -> u64 {
        if self.tables.is_empty() {
            return 0;
        }

        let mut total_usage: u64 = 0;
        // Only skip oldest if table is full (will be dropped on next push)
        let will_drop = self.tables.len() >= self.node_count - 1;
        let count = if will_drop {
            self.tables.len().saturating_sub(1)
        } else {
            self.tables.len()
        };

        for epoch in self.tables.iter().take(count) {
            if let Some(usage) = epoch.get_usage(node_id) {
                total_usage += usage;
            }
        }
        total_usage
    }
}

impl Default for EpochUsageTable {
    fn default() -> Self {
        Self::new(0, 10)
    }
}
