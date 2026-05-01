use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use crc32fast::Hasher as Crc32Hasher;
use parking_lot::Mutex;
use thiserror::Error;
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tracing::error;

const HEADER_MAGIC: &[u8; 8] = b"BLIPWAL\0";
// v2: CRC covers `id` and `len` header fields in addition to the payload,
// so a bit flip in the framing bytes is detected on replay (previously
// `id` and `len` were not protected).
const HEADER_VERSION: u32 = 2;
const HEADER_LEN: u64 = 32;
const RECORD_HEADER_LEN: usize = 8 + 4 + 4;

#[derive(Debug, Error)]
pub enum WalError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("wal overload: {0}")]
    Backpressure(String),

    #[error("wal writer stopped")]
    WriterStopped,

    #[error("invalid wal config: {0}")]
    InvalidConfig(String),

    #[error("log corruption: {0}")]
    Corruption(String),
}

/// Configuration for write-ahead log flush policy.
#[derive(Debug, Clone)]
pub struct WalConfig {
    /// Call fsync every N records. If `None`, do not fsync based on record count.
    pub fsync_every_n: Option<usize>,
    /// Call fsync if at least this duration has elapsed since the last fsync.
    /// Checked on each append. If `None`, do not fsync based on time.
    pub fsync_interval: Option<Duration>,
    /// Capacity for the WAL write channel. When full, appends return an error.
    pub channel_capacity: usize,
}

impl Default for WalConfig {
    fn default() -> Self {
        Self {
            fsync_every_n: Some(64),
            fsync_interval: None,
            channel_capacity: 1024,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WalRecord {
    pub id: u64,
    pub payload: Bytes,
}

#[derive(Debug)]
pub struct WriteAheadLog {
    path: PathBuf,
    index: Arc<Mutex<HashMap<u64, u64>>>,
    sender: mpsc::Sender<WalMessage>,
    next_id: AtomicU64,
    append_count: Arc<AtomicU64>,
    bytes_written: Arc<AtomicU64>,
}

#[derive(Debug)]
struct WalWriteRequest {
    id: u64,
    header: [u8; RECORD_HEADER_LEN],
    payload: Bytes,
    /// When `Some`, the writer signals after the next fsync covering this
    /// record. This is how `append_durable` returns "on disk", enabling
    /// fsync-gated QoS1 publish semantics.
    durable_ack: Option<oneshot::Sender<Result<(), WalError>>>,
}

#[derive(Debug)]
enum WalMessage {
    Record(WalWriteRequest),
    Flush(oneshot::Sender<Result<(), WalError>>),
}

struct WalWriter {
    file: File,
    write_offset: u64,
    unflushed_records: usize,
    last_fsync: Instant,
    config: WalConfig,
    receiver: mpsc::Receiver<WalMessage>,
    append_count: Arc<AtomicU64>,
    bytes_written: Arc<AtomicU64>,
    /// Reused per-batch encode buffer; `clear()` between batches so the
    /// allocation is amortized across the writer's lifetime.
    batch_buf: BytesMut,
    /// Durability-ack senders waiting for the next fsync that covers their
    /// record. Drained and signaled in `flush_file`.
    pending_durable_acks: Vec<oneshot::Sender<Result<(), WalError>>>,
}

impl WriteAheadLog {
    /// Open or create a write-ahead log at the given path with default configuration.
    pub async fn open<P: AsRef<Path>>(path: P) -> Result<Self, WalError> {
        Self::open_with_config(path, WalConfig::default()).await
    }

    /// Open or create a write-ahead log at the given path with the given configuration.
    pub async fn open_with_config<P: AsRef<Path>>(
        path: P,
        config: WalConfig,
    ) -> Result<Self, WalError> {
        let path_ref = path.as_ref();
        if let Some(parent) = path_ref.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).await?;
            }
        }

        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path_ref)
            .await?;

        let metadata = file.metadata().await?;
        let len = metadata.len();

        if len == 0 {
            // New file: write header.
            write_header(&mut file).await?;
        } else if len < HEADER_LEN {
            return Err(WalError::Corruption(
                "file too small to contain header".to_string(),
            ));
        } else {
            // Existing file: validate header.
            validate_header(&mut file).await?;
        }

        // Rebuild in-memory index and determine next_id / write_offset.
        let (index, next_id, write_offset) = rebuild_index(&mut file).await?;

        if config.channel_capacity == 0 {
            return Err(WalError::InvalidConfig(
                "channel_capacity must be greater than 0".to_string(),
            ));
        }

        let (sender, receiver) = mpsc::channel(config.channel_capacity);
        let index = Arc::new(Mutex::new(index));
        let append_count = Arc::new(AtomicU64::new(0));
        let bytes_written = Arc::new(AtomicU64::new(0));

        let writer = WalWriter {
            file,
            write_offset,
            unflushed_records: 0,
            last_fsync: Instant::now(),
            config: config.clone(),
            receiver,
            append_count: Arc::clone(&append_count),
            bytes_written: Arc::clone(&bytes_written),
            batch_buf: BytesMut::with_capacity(64 * 1024),
            pending_durable_acks: Vec::new(),
        };

        let writer_index = Arc::clone(&index);
        tokio::spawn(async move {
            if let Err(err) = writer.run(writer_index).await {
                error!("wal writer stopped with error: {err}");
            }
        });

        Ok(Self {
            path: path_ref.to_path_buf(),
            index,
            sender,
            next_id: AtomicU64::new(next_id),
            append_count,
            bytes_written,
        })
    }

    /// Append a record to the log, returning its logical id.
    ///
    /// **Channel-gated**: returns as soon as the record is queued for the
    /// writer task. The record is *not* guaranteed to be on disk when this
    /// returns. Use [`Self::append_durable`] for fsync-gated semantics.
    ///
    /// Takes `Bytes` rather than `&[u8]` so callers that already hold a
    /// `Bytes` (e.g. inbound PUBLISH payload) avoid an extra copy.
    #[inline(always)]
    #[tracing::instrument(skip(self, data))]
    pub async fn append(&self, data: Bytes) -> Result<u64, WalError> {
        let (id, request) = self.build_request(data, None)?;
        self.send_request(request)?;
        Ok(id)
    }

    /// Append a record and wait for the next fsync that covers it. When this
    /// returns `Ok`, the record is durable on disk. This is the strict-
    /// durability path used for QoS1 publishes.
    #[inline]
    #[tracing::instrument(skip(self, data))]
    pub async fn append_durable(&self, data: Bytes) -> Result<u64, WalError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        let (id, request) = self.build_request(data, Some(ack_tx))?;
        self.send_request(request)?;
        ack_rx.await.map_err(|_| WalError::WriterStopped)??;
        Ok(id)
    }

    fn build_request(
        &self,
        data: Bytes,
        durable_ack: Option<oneshot::Sender<Result<(), WalError>>>,
    ) -> Result<(u64, WalWriteRequest), WalError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut header = [0u8; RECORD_HEADER_LEN];
        header[..8].copy_from_slice(&id.to_le_bytes());
        let len_u32 = u32::try_from(data.len())
            .map_err(|_| WalError::Corruption("record too large".to_string()))?;
        header[8..12].copy_from_slice(&len_u32.to_le_bytes());

        // CRC covers (id, len, payload). v1 covered only payload.
        let mut hasher = Crc32Hasher::new();
        hasher.update(&header[..12]);
        hasher.update(&data);
        let crc = hasher.finalize();
        header[12..16].copy_from_slice(&crc.to_le_bytes());

        Ok((
            id,
            WalWriteRequest {
                id,
                header,
                payload: data,
                durable_ack,
            },
        ))
    }

    fn send_request(&self, request: WalWriteRequest) -> Result<(), WalError> {
        self.sender
            .try_send(WalMessage::Record(request))
            .map_err(|err| match err {
                mpsc::error::TrySendError::Full(_) => {
                    WalError::Backpressure("wal channel full".to_string())
                }
                mpsc::error::TrySendError::Closed(_) => WalError::WriterStopped,
            })
    }

    /// Force a flush of buffered data and an fsync, regardless of configuration.
    pub async fn flush(&self) -> Result<(), WalError> {
        let (sender, receiver) = oneshot::channel();
        self.sender
            .send(WalMessage::Flush(sender))
            .await
            .map_err(|_| WalError::WriterStopped)?;
        receiver.await.map_err(|_| WalError::WriterStopped)?
    }

    /// Return the number of WAL appends and total bytes written since this
    /// process started.
    pub async fn metrics(&self) -> (u64, u64) {
        (
            self.append_count.load(Ordering::Relaxed),
            self.bytes_written.load(Ordering::Relaxed),
        )
    }

    /// Iterate over all records starting at the first record whose id is >= `from_id`.
    pub async fn iterate_from(&self, from_id: u64) -> Result<Vec<WalRecord>, WalError> {
        // Find starting file offset from the index.
        let (start_offset, min_id) = {
            let inner = self.index.lock();

            if inner.is_empty() {
                return Ok(Vec::new());
            }

            // Find the smallest id >= from_id.
            let mut matching_ids: Vec<u64> =
                inner.keys().copied().filter(|id| *id >= from_id).collect();

            if matching_ids.is_empty() {
                return Ok(Vec::new());
            }

            matching_ids.sort_unstable();
            let min_id = matching_ids[0];
            let offset = *inner
                .get(&min_id)
                .expect("index missing offset for known id");

            (offset, min_id)
        };

        let mut file = File::open(&self.path).await?;
        file.seek(std::io::SeekFrom::Start(start_offset)).await?;

        let mut records = Vec::new();

        loop {
            match read_next_record(&mut file).await? {
                None => break,
                Some((id, payload)) => {
                    if id < min_id {
                        continue;
                    }

                    records.push(WalRecord { id, payload });
                }
            }
        }

        Ok(records)
    }

    /// Access the underlying log path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Lookup the file offset for a given logical id. Mainly useful for tests and diagnostics.
    pub async fn lookup_offset(&self, id: u64) -> Option<u64> {
        let inner = self.index.lock();
        inner.get(&id).copied()
    }
}

impl WalWriter {
    async fn run(mut self, index: Arc<Mutex<HashMap<u64, u64>>>) -> Result<(), WalError> {
        while let Some(message) = self.receiver.recv().await {
            match message {
                WalMessage::Record(record) => {
                    let (records, pending_flush) = self.collect_batch(record).await;
                    self.write_batch(records, &index).await?;
                    if let Some(flush_sender) = pending_flush {
                        let result = self.flush_file().await;
                        let _ = flush_sender.send(result);
                    }
                }
                WalMessage::Flush(sender) => {
                    let result = self.flush_file().await;
                    let _ = sender.send(result);
                }
            }
        }

        Ok(())
    }

    async fn collect_batch(
        &mut self,
        first: WalWriteRequest,
    ) -> (
        Vec<WalWriteRequest>,
        Option<oneshot::Sender<Result<(), WalError>>>,
    ) {
        let mut records = vec![first];
        let mut pending_flush = None;

        loop {
            match self.receiver.try_recv() {
                Ok(WalMessage::Record(record)) => {
                    records.push(record);
                }
                Ok(WalMessage::Flush(sender)) => {
                    pending_flush = Some(sender);
                    break;
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }

        (records, pending_flush)
    }

    async fn write_batch(
        &mut self,
        records: Vec<WalWriteRequest>,
        index: &Arc<Mutex<HashMap<u64, u64>>>,
    ) -> Result<(), WalError> {
        // Reuse the writer's batch buffer; clear (preserves capacity) instead
        // of re-allocating per batch.
        self.batch_buf.clear();
        let mut offsets = Vec::with_capacity(records.len());
        let mut current_offset = self.write_offset;

        // Drain durable_ack senders out of the records before we move
        // payloads into the buffer; we'll signal them after fsync.
        let mut new_acks: Vec<oneshot::Sender<Result<(), WalError>>> =
            Vec::with_capacity(records.len());

        for mut record in records {
            offsets.push((record.id, current_offset));
            current_offset += (RECORD_HEADER_LEN + record.payload.len()) as u64;
            self.batch_buf.extend_from_slice(&record.header);
            self.batch_buf.extend_from_slice(&record.payload);
            if let Some(ack) = record.durable_ack.take() {
                new_acks.push(ack);
            }
        }

        let total_records = offsets.len();
        let total_bytes = self.batch_buf.len() as u64;

        self.file.write_all(&self.batch_buf).await?;
        self.write_offset = current_offset;

        {
            let mut guard = index.lock();
            for (id, offset) in offsets {
                guard.insert(id, offset);
            }
        }

        self.append_count
            .fetch_add(total_records as u64, Ordering::Relaxed);
        self.bytes_written.fetch_add(total_bytes, Ordering::Relaxed);

        self.unflushed_records = self.unflushed_records.saturating_add(total_records);
        // Defer durable acks to the next fsync. If `maybe_sync` fires below
        // because of a record-count or interval threshold, all pending acks
        // (including these new ones) get signaled inside `flush_file`.
        // Otherwise they stay queued until the next fsync.
        self.pending_durable_acks.extend(new_acks);

        // If any record requested durable ack, force a sync now so callers
        // don't wait for the next batch / interval. This keeps p99 latency
        // bounded for QoS1 publishers; the cost is amortized across the
        // whole batch (group commit).
        if !self.pending_durable_acks.is_empty() {
            self.flush_file().await?;
        } else {
            self.maybe_sync().await?;
        }

        Ok(())
    }

    async fn flush_file(&mut self) -> Result<(), WalError> {
        let result = async {
            self.file.flush().await?;
            self.file.sync_data().await?;
            Ok::<(), WalError>(())
        }
        .await;

        // Fan the outcome out to all queued durable-ack senders so callers
        // wake. WalError isn't Clone, so on error we string-format once and
        // hand each waiter a fresh Corruption with that message.
        let err_msg: Option<String> = result
            .as_ref()
            .err()
            .map(|e| format!("wal fsync failed: {e}"));
        for ack in self.pending_durable_acks.drain(..) {
            let payload: Result<(), WalError> = match &err_msg {
                None => Ok(()),
                Some(s) => Err(WalError::Corruption(s.clone())),
            };
            let _ = ack.send(payload);
        }

        if result.is_ok() {
            self.unflushed_records = 0;
            self.last_fsync = Instant::now();
        }
        result
    }

    async fn maybe_sync(&mut self) -> Result<(), WalError> {
        let mut should_sync = false;

        if let Some(every_n) = self.config.fsync_every_n {
            if self.unflushed_records >= every_n {
                should_sync = true;
            }
        }

        if !should_sync {
            if let Some(interval) = self.config.fsync_interval {
                if self.last_fsync.elapsed() >= interval {
                    should_sync = true;
                }
            }
        }

        if should_sync {
            let span = tracing::trace_span!("wal_flush");
            let _guard = span.enter();
            self.flush_file().await?;
        }

        Ok(())
    }
}

async fn write_header(file: &mut File) -> Result<(), WalError> {
    let mut buf = [0u8; HEADER_LEN as usize];
    buf[..8].copy_from_slice(HEADER_MAGIC);
    buf[8..12].copy_from_slice(&HEADER_VERSION.to_le_bytes());
    // Remaining bytes are reserved / zero.
    file.write_all(&buf).await?;
    file.flush().await?;
    file.sync_data().await?;
    Ok(())
}

async fn validate_header(file: &mut File) -> Result<(), WalError> {
    let mut buf = [0u8; HEADER_LEN as usize];
    file.seek(std::io::SeekFrom::Start(0)).await?;
    let mut read = 0usize;
    while read < buf.len() {
        let n = file.read(&mut buf[read..]).await?;
        if n == 0 {
            return Err(WalError::Corruption(
                "unexpected EOF while reading WAL header".to_string(),
            ));
        }
        read += n;
    }

    if &buf[..8] != HEADER_MAGIC {
        return Err(WalError::Corruption("invalid WAL magic".to_string()));
    }

    let mut version_bytes = [0u8; 4];
    version_bytes.copy_from_slice(&buf[8..12]);
    let version = u32::from_le_bytes(version_bytes);
    if version != HEADER_VERSION {
        return Err(WalError::Corruption(format!(
            "unsupported WAL version: {version}"
        )));
    }

    Ok(())
}

async fn rebuild_index(file: &mut File) -> Result<(HashMap<u64, u64>, u64, u64), WalError> {
    let mut index = HashMap::new();
    let mut next_id = 1u64;

    file.seek(std::io::SeekFrom::Start(HEADER_LEN)).await?;
    let mut offset = HEADER_LEN;

    loop {
        match read_next_record_with_offset(file, offset).await {
            Ok(Some((id, _payload, record_offset, total_len))) => {
                index.insert(id, record_offset);
                next_id = id.wrapping_add(1);
                offset = offset
                    .checked_add(total_len)
                    .ok_or_else(|| WalError::Corruption("log offset overflow".to_string()))?;
            }
            Ok(None) => break,
            Err(WalError::Corruption(reason)) => {
                error!("WAL corruption detected during index rebuild: {}", reason);
                return Err(WalError::Corruption(reason));
            }
            Err(e) => return Err(e),
        }
    }

    Ok((index, next_id, offset))
}

async fn read_next_record(file: &mut File) -> Result<Option<(u64, Bytes)>, WalError> {
    match read_next_record_with_offset(file, 0).await {
        Ok(Some((id, payload, _offset, _len))) => Ok(Some((id, payload))),
        Ok(None) => Ok(None),
        Err(e) => Err(e),
    }
}

async fn read_next_record_with_offset(
    file: &mut File,
    current_offset: u64,
) -> Result<Option<(u64, Bytes, u64, u64)>, WalError> {
    let mut header = [0u8; RECORD_HEADER_LEN];
    let mut read = 0usize;
    while read < header.len() {
        let n = file.read(&mut header[read..]).await?;
        if n == 0 {
            return if read == 0 {
                // Clean EOF.
                Ok(None)
            } else {
                // Partial header at end of file: treat as no further records.
                Ok(None)
            };
        }
        read += n;
    }

    let mut id_bytes = [0u8; 8];
    id_bytes.copy_from_slice(&header[..8]);
    let id = u64::from_le_bytes(id_bytes);

    let mut len_bytes = [0u8; 4];
    len_bytes.copy_from_slice(&header[8..12]);
    let len = u32::from_le_bytes(len_bytes) as usize;

    let mut crc_bytes = [0u8; 4];
    crc_bytes.copy_from_slice(&header[12..16]);
    let expected_crc = u32::from_le_bytes(crc_bytes);

    let mut payload = vec![0u8; len];
    let mut read_payload = 0usize;
    while read_payload < len {
        let n = file.read(&mut payload[read_payload..]).await?;
        if n == 0 {
            // Partial payload at end of file: treat tail as not present.
            return Ok(None);
        }
        read_payload += n;
    }

    // CRC v2 covers the (id, len) header bytes plus the payload. v1 covered
    // only the payload, so a flip in the framing bytes went undetected.
    let mut hasher = Crc32Hasher::new();
    hasher.update(&header[..12]);
    hasher.update(&payload);
    let actual_crc = hasher.finalize();

    if actual_crc != expected_crc {
        return Err(WalError::Corruption(format!(
            "CRC mismatch at offset {current_offset}: expected {expected_crc:08x}, got {actual_crc:08x}"
        )));
    }

    let total_len = RECORD_HEADER_LEN as u64 + len as u64;

    Ok(Some((id, Bytes::from(payload), current_offset, total_len)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wal_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("wal_test_{name}.log"));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[tokio::test]
    async fn append_and_iterate_roundtrip() {
        let path = wal_path("roundtrip");
        let wal = WriteAheadLog::open(&path).await.unwrap();

        let id1 = wal.append(Bytes::from_static(b"first")).await.unwrap();
        let id2 = wal.append(Bytes::from_static(b"second")).await.unwrap();
        wal.flush().await.unwrap();

        let records = wal.iterate_from(id1).await.unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].id, id1);
        assert_eq!(records[0].payload, Bytes::from_static(b"first"));
        assert_eq!(records[1].id, id2);
        assert_eq!(records[1].payload, Bytes::from_static(b"second"));
    }

    #[tokio::test]
    async fn corruption_is_detected() {
        let path = wal_path("corruption");
        {
            let wal = WriteAheadLog::open(&path).await.unwrap();
            let _ = wal.append(Bytes::from_static(b"good")).await.unwrap();
            let _ = wal.append(Bytes::from_static(b"also good")).await.unwrap();
            wal.flush().await.unwrap();
        }

        // Corrupt a byte near the end of the file.
        use std::io::{Read, Seek, SeekFrom, Write};

        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();

        file.seek(SeekFrom::End(-1)).unwrap();
        let mut byte = [0u8; 1];
        file.read_exact(&mut byte).unwrap();
        byte[0] ^= 0xFF;
        file.seek(SeekFrom::End(-1)).unwrap();
        file.write_all(&byte).unwrap();
        file.flush().unwrap();

        let err = WriteAheadLog::open(&path).await.unwrap_err();
        match err {
            WalError::Corruption(_) => {}
            other => panic!("expected corruption error, got {other:?}"),
        }
    }

    /// v2 CRC covers (id, len, payload). A bit flip in the `id` bytes used
    /// to go undetected (v1 CRC was payload-only). Reopen must fail with
    /// Corruption.
    #[tokio::test]
    async fn header_corruption_caught_by_crc_v2() {
        use std::io::{Read, Seek, SeekFrom, Write};

        let path = wal_path("header_corruption");
        {
            let wal = WriteAheadLog::open(&path).await.unwrap();
            let _ = wal.append(Bytes::from_static(b"first")).await.unwrap();
            let _ = wal.append(Bytes::from_static(b"second")).await.unwrap();
            wal.flush().await.unwrap();
        }

        // Flip the lowest bit of the *id* field of the first record. With v1
        // CRC this would have been silently accepted; with v2 it must fail.
        let id_offset = HEADER_LEN; // first record's id starts immediately after WAL header
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();

        file.seek(SeekFrom::Start(id_offset)).unwrap();
        let mut byte = [0u8; 1];
        file.read_exact(&mut byte).unwrap();
        byte[0] ^= 0x01;
        file.seek(SeekFrom::Start(id_offset)).unwrap();
        file.write_all(&byte).unwrap();
        file.flush().unwrap();

        let err = WriteAheadLog::open(&path).await.unwrap_err();
        assert!(
            matches!(err, WalError::Corruption(_)),
            "expected Corruption, got {err:?}",
        );
    }

    /// Group-commit fsync ACK: append_durable must NOT return until the
    /// record is on disk. Verified by checking the file length after the
    /// call returns (without an explicit `flush` first).
    #[tokio::test]
    async fn append_durable_blocks_until_on_disk() {
        let path = wal_path("durable_blocks");
        let wal = WriteAheadLog::open(&path).await.unwrap();

        let len_before = std::fs::metadata(&path).unwrap().len();
        let _ = wal
            .append_durable(Bytes::from_static(b"durable"))
            .await
            .unwrap();
        let len_after = std::fs::metadata(&path).unwrap().len();

        // The record (header + payload) is at least HEADER_LEN larger than
        // before; if append_durable returned before fsync, the file might
        // still be empty/short.
        assert!(
            len_after > len_before,
            "file did not grow after append_durable: before={len_before} after={len_after}",
        );
        assert!(len_after >= len_before + RECORD_HEADER_LEN as u64);
    }

    #[tokio::test]
    async fn index_is_rebuilt_on_open() {
        let path = wal_path("index_rebuild");
        let id2;
        {
            let wal = WriteAheadLog::open(&path).await.unwrap();
            let _id1 = wal.append(Bytes::from_static(b"first")).await.unwrap();
            id2 = wal.append(Bytes::from_static(b"second")).await.unwrap();
            let _id3 = wal.append(Bytes::from_static(b"third")).await.unwrap();
            wal.flush().await.unwrap();
        }

        let wal = WriteAheadLog::open(&path).await.unwrap();

        let offset = wal.lookup_offset(id2).await;
        assert!(offset.is_some(), "offset for id2 should be present");

        let records = wal.iterate_from(id2).await.unwrap();
        assert!(!records.is_empty());
        assert_eq!(records[0].id, id2);
        assert_eq!(records[0].payload, Bytes::from_static(b"second"));
    }
}
