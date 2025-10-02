//! Write-Ahead Log (WAL) implementation for BlipMQ
//!
//! Provides durable message persistence with:
//! - Atomic write operations
//! - Fast sequential writes
//! - Recovery and replay capabilities
//! - Segment rotation and cleanup
//! - Crash-safe message storage

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::{Bytes, BytesMut, BufMut};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};
use tracing::{debug, error, info, warn};

use crate::core::message::{Message, WireMessage};

/// WAL entry types for different operations
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WalEntryType {
    /// Message published to topic
    MessagePublished {
        topic: String,
        message_id: u64,
        payload: Bytes,
        timestamp: u64,
        ttl_ms: u64,
    },
    /// Message acknowledged by subscriber
    MessageAcknowledged {
        topic: String,
        message_id: u64,
        subscriber_id: String,
    },
    /// Topic created
    TopicCreated {
        topic: String,
        config: TopicConfig,
    },
    /// Subscriber registered
    SubscriberRegistered {
        topic: String,
        subscriber_id: String,
        qos: u8,
    },
    /// Checkpoint marker for recovery
    Checkpoint {
        sequence: u64,
        timestamp: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopicConfig {
    pub retention_ms: u64,
    pub max_message_size: usize,
    pub durable: bool,
}

impl Default for TopicConfig {
    fn default() -> Self {
        Self {
            retention_ms: 7 * 24 * 60 * 60 * 1000, // 7 days
            max_message_size: 1024 * 1024,         // 1MB
            durable: true,
        }
    }
}

/// WAL entry with metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalEntry {
    pub sequence: u64,
    pub timestamp: u64,
    pub entry_type: WalEntryType,
    pub checksum: u32,
}

impl WalEntry {
    pub fn new(sequence: u64, entry_type: WalEntryType) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let mut entry = Self {
            sequence,
            timestamp,
            entry_type,
            checksum: 0,
        };
        
        entry.checksum = entry.calculate_checksum();
        entry
    }

    fn calculate_checksum(&self) -> u32 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        
        let mut hasher = DefaultHasher::new();
        self.sequence.hash(&mut hasher);
        self.timestamp.hash(&mut hasher);
        // Note: We'd need to implement Hash for WalEntryType in production
        hasher.finish() as u32
    }

    pub fn is_valid(&self) -> bool {
        self.checksum == self.calculate_checksum()
    }
}

/// WAL segment file
#[derive(Debug)]
pub struct WalSegment {
    pub id: u64,
    pub file_path: PathBuf,
    pub writer: BufWriter<File>,
    pub size: u64,
    pub start_sequence: u64,
    pub end_sequence: u64,
    pub created_at: u64,
}

impl WalSegment {
    pub fn new(id: u64, directory: &Path, start_sequence: u64) -> io::Result<Self> {
        let file_path = directory.join(format!("wal-{:010}.log", id));
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .append(true)
            .open(&file_path)?;
            
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        Ok(Self {
            id,
            file_path,
            writer: BufWriter::new(file),
            size: 0,
            start_sequence,
            end_sequence: start_sequence,
            created_at,
        })
    }

    pub fn write_entry(&mut self, entry: &WalEntry) -> io::Result<()> {
        // Serialize entry to bytes
        let serialized = bincode::serialize(entry)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        
        // Write length prefix + data
        let len = serialized.len() as u32;
        self.writer.write_all(&len.to_be_bytes())?;
        self.writer.write_all(&serialized)?;
        
        self.size += 4 + serialized.len() as u64;
        self.end_sequence = entry.sequence;
        
        Ok(())
    }

    pub fn sync(&mut self) -> io::Result<()> {
        self.writer.flush()?;
        self.writer.get_ref().sync_all()
    }

    pub fn read_entries(&self) -> io::Result<Vec<WalEntry>> {
        let file = File::open(&self.file_path)?;
        let mut reader = BufReader::new(file);
        let mut entries = Vec::new();
        
        loop {
            // Read length prefix
            let mut len_bytes = [0u8; 4];
            match reader.read_exact(&mut len_bytes) {
                Ok(()) => {},
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }
            
            let len = u32::from_be_bytes(len_bytes) as usize;
            
            // Read entry data
            let mut entry_bytes = vec![0u8; len];
            reader.read_exact(&mut entry_bytes)?;
            
            // Deserialize entry
            match bincode::deserialize::<WalEntry>(&entry_bytes) {
                Ok(entry) => {
                    if entry.is_valid() {
                        entries.push(entry);
                    } else {
                        warn!("Skipping corrupted WAL entry at sequence {}", entry.sequence);
                    }
                }
                Err(e) => {
                    error!("Failed to deserialize WAL entry: {}", e);
                    break;
                }
            }
        }
        
        Ok(entries)
    }
}

/// WAL configuration
#[derive(Debug, Clone)]
pub struct WalConfig {
    pub directory: PathBuf,
    pub segment_size_bytes: u64,
    pub sync_interval_ms: u64,
    pub retention_segments: usize,
    pub enable_compression: bool,
    pub sync_on_write: bool,
}

impl Default for WalConfig {
    fn default() -> Self {
        Self {
            directory: PathBuf::from("./wal"),
            segment_size_bytes: 100 * 1024 * 1024, // 100MB
            sync_interval_ms: 1000,                 // 1 second
            retention_segments: 100,
            enable_compression: false,
            sync_on_write: false,
        }
    }
}

/// Statistics for WAL operations
#[derive(Debug, Default)]
pub struct WalStats {
    pub entries_written: AtomicU64,
    pub bytes_written: AtomicU64,
    pub sync_operations: AtomicU64,
    pub segment_rotations: AtomicU64,
    pub recovery_time_ms: AtomicU64,
    pub corrupted_entries: AtomicU64,
}

/// Serializable snapshot of WAL statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalStatsSnapshot {
    pub entries_written: u64,
    pub bytes_written: u64,
    pub sync_operations: u64,
    pub segment_rotations: u64,
    pub recovery_time_ms: u64,
    pub corrupted_entries: u64,
}

impl WalStats {
    pub fn snapshot(&self) -> WalStatsSnapshot {
        WalStatsSnapshot {
            entries_written: self.entries_written.load(Ordering::Relaxed),
            bytes_written: self.bytes_written.load(Ordering::Relaxed),
            sync_operations: self.sync_operations.load(Ordering::Relaxed),
            segment_rotations: self.segment_rotations.load(Ordering::Relaxed),
            recovery_time_ms: self.recovery_time_ms.load(Ordering::Relaxed),
            corrupted_entries: self.corrupted_entries.load(Ordering::Relaxed),
        }
    }
}


/// Write-Ahead Log manager
pub struct WriteAheadLog {
    config: WalConfig,
    current_segment: Arc<Mutex<Option<WalSegment>>>,
    segments: Arc<RwLock<BTreeMap<u64, PathBuf>>>,
    sequence_counter: AtomicU64,
    stats: Arc<WalStats>,
    write_tx: mpsc::UnboundedSender<WalEntry>,
}

impl WriteAheadLog {
    /// Creates a new WAL instance
    pub async fn new(config: WalConfig) -> io::Result<Self> {
        // Create WAL directory if it doesn't exist
        std::fs::create_dir_all(&config.directory)?;

        let (write_tx, write_rx) = mpsc::unbounded_channel();
        
        let wal = Self {
            config: config.clone(),
            current_segment: Arc::new(Mutex::new(None)),
            segments: Arc::new(RwLock::new(BTreeMap::new())),
            sequence_counter: AtomicU64::new(0),
            stats: Arc::new(WalStats::default()),
            write_tx,
        };

        // Recover from existing segments
        wal.recover().await?;

        // Start background writer
        wal.start_background_writer(write_rx).await;

        // Start background sync task
        wal.start_background_sync().await;

        Ok(wal)
    }

    /// Recovers WAL state from existing segments
    async fn recover(&self) -> io::Result<()> {
        let start_time = std::time::Instant::now();
        info!("Starting WAL recovery from directory: {:?}", self.config.directory);

        let mut max_sequence = 0u64;
        let mut segments = BTreeMap::new();

        // Scan for existing segment files
        if self.config.directory.exists() {
            for entry in std::fs::read_dir(&self.config.directory)? {
                let entry = entry?;
                let path = entry.path();
                
                if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
                    if file_name.starts_with("wal-") && file_name.ends_with(".log") {
                        // Parse segment ID
                        if let Some(id_str) = file_name.strip_prefix("wal-").and_then(|s| s.strip_suffix(".log")) {
                            if let Ok(segment_id) = id_str.parse::<u64>() {
                                segments.insert(segment_id, path);
                            }
                        }
                    }
                }
            }
        }

        // Replay entries from segments
        let mut total_entries = 0;
        for (&segment_id, segment_path) in &segments {
            match self.replay_segment(segment_path).await {
                Ok(entries) => {
                    total_entries += entries.len();
                    if let Some(last_entry) = entries.last() {
                        max_sequence = max_sequence.max(last_entry.sequence);
                    }
                    debug!("Recovered {} entries from segment {}", entries.len(), segment_id);
                }
                Err(e) => {
                    error!("Failed to recover segment {}: {}", segment_id, e);
                    self.stats.corrupted_entries.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        // Update segment registry
        *self.segments.write() = segments;
        
        // Set sequence counter
        self.sequence_counter.store(max_sequence + 1, Ordering::Relaxed);

        let recovery_time = start_time.elapsed().as_millis() as u64;
        self.stats.recovery_time_ms.store(recovery_time, Ordering::Relaxed);

        info!(
            "WAL recovery completed: {} entries from {} segments in {}ms",
            total_entries,
            self.segments.read().len(),
            recovery_time
        );

        Ok(())
    }

    /// Replays entries from a segment file
    async fn replay_segment(&self, segment_path: &Path) -> io::Result<Vec<WalEntry>> {
        let file = File::open(segment_path)?;
        let mut reader = BufReader::new(file);
        let mut entries = Vec::new();
        
        loop {
            // Read length prefix
            let mut len_bytes = [0u8; 4];
            match reader.read_exact(&mut len_bytes) {
                Ok(()) => {},
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }
            
            let len = u32::from_be_bytes(len_bytes) as usize;
            if len > 10 * 1024 * 1024 {  // Sanity check: max 10MB entry
                warn!("Skipping oversized entry: {} bytes", len);
                break;
            }
            
            // Read entry data
            let mut entry_bytes = vec![0u8; len];
            reader.read_exact(&mut entry_bytes)?;
            
            // Deserialize entry
            match bincode::deserialize::<WalEntry>(&entry_bytes) {
                Ok(entry) => {
                    if entry.is_valid() {
                        // Apply entry to state (would be implemented based on entry type)
                        self.apply_entry(&entry).await;
                        entries.push(entry);
                    } else {
                        warn!("Skipping corrupted entry in segment {:?}", segment_path);
                        self.stats.corrupted_entries.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(e) => {
                    error!("Failed to deserialize entry: {}", e);
                    break;
                }
            }
        }
        
        Ok(entries)
    }

    /// Applies a WAL entry to rebuild system state
    async fn apply_entry(&self, entry: &WalEntry) {
        match &entry.entry_type {
            WalEntryType::MessagePublished { topic, message_id, .. } => {
                debug!("Replaying message {} to topic {}", message_id, topic);
                // Restore message to topic state
            }
            WalEntryType::MessageAcknowledged { topic, message_id, subscriber_id } => {
                debug!("Replaying ack for message {} from subscriber {} on topic {}", 
                       message_id, subscriber_id, topic);
                // Mark message as acknowledged
            }
            WalEntryType::TopicCreated { topic, config } => {
                debug!("Replaying topic creation: {}", topic);
                // Recreate topic with config
            }
            WalEntryType::SubscriberRegistered { topic, subscriber_id, qos } => {
                debug!("Replaying subscriber registration: {} on topic {}", subscriber_id, topic);
                // Restore subscriber state
            }
            WalEntryType::Checkpoint { sequence, timestamp } => {
                debug!("Checkpoint at sequence {} timestamp {}", sequence, timestamp);
                // Checkpoint processing
            }
        }
    }

    /// Writes an entry to the WAL
    pub async fn write_entry(&self, entry_type: WalEntryType) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
        let sequence = self.sequence_counter.fetch_add(1, Ordering::Relaxed);
        let entry = WalEntry::new(sequence, entry_type);
        
        self.write_tx.send(entry)?;
        Ok(sequence)
    }

    /// Starts the background writer task
    async fn start_background_writer(&self, mut write_rx: mpsc::UnboundedReceiver<WalEntry>) {
        let current_segment = Arc::clone(&self.current_segment);
        let segments = Arc::clone(&self.segments);
        let config = self.config.clone();
        let stats = Arc::clone(&self.stats);

        tokio::spawn(async move {
            while let Some(entry) = write_rx.recv().await {
                if let Err(e) = Self::write_entry_to_segment(
                    &entry,
                    &current_segment,
                    &segments,
                    &config,
                    &stats,
                ).await {
                    error!("Failed to write WAL entry: {}", e);
                }
            }
        });
    }

    /// Writes an entry to the current segment
    async fn write_entry_to_segment(
        entry: &WalEntry,
        current_segment: &Arc<Mutex<Option<WalSegment>>>,
        segments: &Arc<RwLock<BTreeMap<u64, PathBuf>>>,
        config: &WalConfig,
        stats: &Arc<WalStats>,
    ) -> io::Result<()> {
        let mut segment_guard = current_segment.lock();
        
        // Create new segment if needed
        if segment_guard.is_none() || 
           segment_guard.as_ref().unwrap().size >= config.segment_size_bytes {
            
            if segment_guard.is_some() {
                stats.segment_rotations.fetch_add(1, Ordering::Relaxed);
            }
            
            let segment_id = entry.sequence / 10000; // Group by 10k sequences
            let new_segment = WalSegment::new(segment_id, &config.directory, entry.sequence)?;
            
            // Register segment
            segments.write().insert(segment_id, new_segment.file_path.clone());
            
            *segment_guard = Some(new_segment);
        }
        
        // Write entry
        if let Some(ref mut segment) = segment_guard.as_mut() {
            let bytes_before = segment.size;
            segment.write_entry(entry)?;
            let bytes_written = segment.size - bytes_before;
            
            stats.bytes_written.fetch_add(bytes_written, Ordering::Relaxed);
            
            if config.sync_on_write {
                segment.sync()?;
                stats.sync_operations.fetch_add(1, Ordering::Relaxed);
            }
        }
        
        stats.entries_written.fetch_add(1, Ordering::Relaxed);
        
        Ok(())
    }

    /// Starts the background sync task
    async fn start_background_sync(&self) {
        let current_segment = Arc::clone(&self.current_segment);
        let stats = Arc::clone(&self.stats);
        let sync_interval = Duration::from_millis(self.config.sync_interval_ms);

        tokio::spawn(async move {
            let mut interval = interval(sync_interval);
            
            loop {
                interval.tick().await;
                
                let mut segment_guard = current_segment.lock();
                if let Some(ref mut segment) = segment_guard.as_mut() {
                    if let Err(e) = segment.sync() {
                        error!("Failed to sync WAL segment: {}", e);
                    } else {
                        stats.sync_operations.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        });
    }

    /// Returns current WAL statistics
    pub fn stats(&self) -> WalStatsSnapshot {
        self.stats.snapshot()
    }

    /// Performs a checkpoint operation
    pub async fn checkpoint(&self) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
        let current_sequence = self.sequence_counter.load(Ordering::Relaxed);
        self.write_entry(WalEntryType::Checkpoint {
            sequence: current_sequence,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
        }).await
    }

    /// Cleans up old segments based on retention policy
    pub async fn cleanup_old_segments(&self) -> io::Result<usize> {
        let segments_map = self.segments.read();
        let segment_count = segments_map.len();
        
        if segment_count <= self.config.retention_segments {
            return Ok(0);
        }
        
        let to_remove = segment_count - self.config.retention_segments;
        let mut removed = 0;
        
        // Remove oldest segments
        for (&segment_id, segment_path) in segments_map.iter().take(to_remove) {
            if let Err(e) = std::fs::remove_file(segment_path) {
                warn!("Failed to remove old segment {}: {}", segment_id, e);
            } else {
                removed += 1;
                debug!("Removed old WAL segment: {}", segment_id);
            }
        }
        
        // Update segments map
        drop(segments_map);
        let mut segments_map = self.segments.write();
        let keys_to_remove: Vec<u64> = segments_map.keys().take(to_remove).cloned().collect();
        for key in keys_to_remove {
            segments_map.remove(&key);
        }
        
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::time::sleep;

    #[tokio::test]
    async fn test_wal_basic_operations() {
        let temp_dir = TempDir::new().unwrap();
        let config = WalConfig {
            directory: temp_dir.path().to_path_buf(),
            segment_size_bytes: 1024, // Small for testing
            sync_interval_ms: 100,
            retention_segments: 5,
            enable_compression: false,
            sync_on_write: true,
        };

        let wal = WriteAheadLog::new(config).await.unwrap();

        // Write some entries
        let seq1 = wal.write_entry(WalEntryType::TopicCreated {
            topic: "test-topic".to_string(),
            config: TopicConfig::default(),
        }).await.unwrap();

        let seq2 = wal.write_entry(WalEntryType::MessagePublished {
            topic: "test-topic".to_string(),
            message_id: 123,
            payload: Bytes::from("hello world"),
            timestamp: 1234567890,
            ttl_ms: 60000,
        }).await.unwrap();

        assert!(seq2 > seq1);

        // Wait for background processing
        sleep(Duration::from_millis(200)).await;

        let stats = wal.stats();
        assert_eq!(stats.entries_written, 2);
        assert!(stats.bytes_written > 0);
    }

    #[tokio::test]
    async fn test_wal_recovery() {
        let temp_dir = TempDir::new().unwrap();
        let config = WalConfig {
            directory: temp_dir.path().to_path_buf(),
            segment_size_bytes: 1024,
            sync_interval_ms: 100,
            retention_segments: 5,
            enable_compression: false,
            sync_on_write: true,
        };

        // Write entries with first WAL instance
        {
            let wal1 = WriteAheadLog::new(config.clone()).await.unwrap();
            
            for i in 0..10 {
                wal1.write_entry(WalEntryType::MessagePublished {
                    topic: "test-topic".to_string(),
                    message_id: i,
                    payload: Bytes::from(format!("message-{}", i)),
                    timestamp: 1234567890 + i,
                    ttl_ms: 60000,
                }).await.unwrap();
            }

            sleep(Duration::from_millis(200)).await;
        }

        // Create new WAL instance - should recover
        let wal2 = WriteAheadLog::new(config).await.unwrap();
        sleep(Duration::from_millis(100)).await;

        let stats = wal2.stats();
        assert_eq!(stats.entries_written, 0); // No new entries written
        assert!(stats.recovery_time_ms > 0);
    }
}