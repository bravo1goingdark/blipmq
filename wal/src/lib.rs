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
// v3: WAL is now a directory of segments (`wal-NNNNNNNNNNNNNNNNNNNN.log`)
// rather than a single growing file. Each segment carries this same
// header. Bumped because old single-file WALs from v2 won't be auto-
// migrated; users with existing v2 files should drain them before
// upgrading.
const HEADER_VERSION: u32 = 3;
const HEADER_LEN: u64 = 32;
const RECORD_HEADER_LEN: usize = 8 + 4 + 4;

const SEGMENT_PREFIX: &str = "wal-";
const SEGMENT_SUFFIX: &str = ".log";
/// Default per-segment size in bytes: 256 MiB. Configurable via
/// `WalConfig::segment_bytes`.
const DEFAULT_SEGMENT_BYTES: u64 = 256 * 1024 * 1024;

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
    /// Roll to a new segment when the current segment grows past this many
    /// bytes (header + records). Default 256 MiB.
    pub segment_bytes: u64,
}

impl Default for WalConfig {
    fn default() -> Self {
        Self {
            fsync_every_n: Some(64),
            fsync_interval: None,
            channel_capacity: 1024,
            segment_bytes: DEFAULT_SEGMENT_BYTES,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WalRecord {
    pub id: u64,
    pub payload: Bytes,
}

/// Where in the segmented WAL a record lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordLocation {
    pub segment_seq: u64,
    pub offset_in_segment: u64,
}

#[derive(Debug)]
pub struct WriteAheadLog {
    dir: PathBuf,
    /// In-memory id → (segment_seq, offset) index. Rebuilt at open time
    /// by scanning all segments under `dir`.
    index: Arc<Mutex<HashMap<u64, RecordLocation>>>,
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
    dir: PathBuf,
    /// Currently-active segment file. New writes go here; rolls over once
    /// `current_offset` would exceed `segment_bytes`.
    file: File,
    current_segment_seq: u64,
    current_offset: u64,
    segment_bytes: u64,
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
    /// Open or create a write-ahead log at the given directory with default configuration.
    pub async fn open<P: AsRef<Path>>(path: P) -> Result<Self, WalError> {
        Self::open_with_config(path, WalConfig::default()).await
    }

    /// Open or create a write-ahead log at the given directory with the given configuration.
    /// `path` is interpreted as a **directory**: WAL segments live inside as
    /// `wal-NNNNNNNNNNNNNNNNNNNN.log` files.
    pub async fn open_with_config<P: AsRef<Path>>(
        path: P,
        config: WalConfig,
    ) -> Result<Self, WalError> {
        if config.channel_capacity == 0 {
            return Err(WalError::InvalidConfig(
                "channel_capacity must be greater than 0".to_string(),
            ));
        }
        if config.segment_bytes < HEADER_LEN + RECORD_HEADER_LEN as u64 {
            return Err(WalError::InvalidConfig(
                "segment_bytes too small to hold one record".to_string(),
            ));
        }

        let dir = path.as_ref().to_path_buf();

        // Migration guard: if the path exists as a regular file, it's a
        // pre-segmentation WAL. Refuse rather than silently shadowing.
        if let Ok(meta) = std::fs::metadata(&dir) {
            if meta.is_file() {
                return Err(WalError::InvalidConfig(format!(
                    "{} is a file; segmented WAL expects a directory. \
                     Drain old single-file WALs before upgrading.",
                    dir.display()
                )));
            }
        }
        fs::create_dir_all(&dir).await?;

        // Scan segments. If empty, create segment 1.
        let mut segment_seqs = list_segment_seqs(&dir).await?;
        segment_seqs.sort_unstable();

        let mut index: HashMap<u64, RecordLocation> = HashMap::new();
        let mut next_id: u64 = 1;
        let (current_seq, current_offset, current_file) = if segment_seqs.is_empty() {
            // Fresh WAL: create segment 1 with header.
            let seq = 1;
            let segment_path = segment_path(&dir, seq);
            let mut file = OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(&segment_path)
                .await?;
            write_header(&mut file).await?;
            (seq, HEADER_LEN, file)
        } else {
            // Validate every segment header and rebuild the index by
            // walking every record in every segment in order.
            let mut tail_seq = *segment_seqs.last().unwrap();
            let mut tail_offset = HEADER_LEN;
            for &seq in &segment_seqs {
                let segment_path = segment_path(&dir, seq);
                let mut file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&segment_path)
                    .await?;
                validate_header(&mut file).await?;
                let (seg_records, _next_id_in_seg, end_offset) =
                    rebuild_segment_index(&mut file).await?;
                for (id, offset) in seg_records {
                    index.insert(
                        id,
                        RecordLocation {
                            segment_seq: seq,
                            offset_in_segment: offset,
                        },
                    );
                    if id >= next_id {
                        next_id = id.wrapping_add(1);
                    }
                }
                tail_seq = seq;
                tail_offset = end_offset;
            }
            // Reopen the tail segment with append semantics for the writer.
            let tail_path = segment_path(&dir, tail_seq);
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&tail_path)
                .await?;
            file.seek(std::io::SeekFrom::Start(tail_offset)).await?;
            (tail_seq, tail_offset, file)
        };

        let (sender, receiver) = mpsc::channel(config.channel_capacity);
        let index = Arc::new(Mutex::new(index));
        let append_count = Arc::new(AtomicU64::new(0));
        let bytes_written = Arc::new(AtomicU64::new(0));

        let writer = WalWriter {
            dir: dir.clone(),
            file: current_file,
            current_segment_seq: current_seq,
            current_offset,
            segment_bytes: config.segment_bytes,
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
            dir,
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
    #[inline(always)]
    #[tracing::instrument(skip(self, data))]
    pub async fn append(&self, data: Bytes) -> Result<u64, WalError> {
        let (id, request) = self.build_request(data, None)?;
        self.send_request(request)?;
        Ok(id)
    }

    /// Append a record and wait for the next fsync that covers it. When this
    /// returns `Ok`, the record is durable on disk.
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

        // CRC covers (id, len, payload).
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

    /// Iterate over all records starting at the first record whose id is
    /// >= `from_id`, walking segments in order.
    pub async fn iterate_from(&self, from_id: u64) -> Result<Vec<WalRecord>, WalError> {
        // Determine starting segment by finding the smallest id >= from_id
        // and looking up its segment_seq. From there, walk that segment to
        // its end, then continue across subsequent segments.
        let (start_seq, start_offset, min_id) = {
            let inner = self.index.lock();
            if inner.is_empty() {
                return Ok(Vec::new());
            }
            let mut matching: Vec<(u64, RecordLocation)> = inner
                .iter()
                .filter(|(id, _)| **id >= from_id)
                .map(|(id, loc)| (*id, *loc))
                .collect();
            if matching.is_empty() {
                return Ok(Vec::new());
            }
            matching.sort_unstable_by_key(|(id, _)| *id);
            let (min_id, loc) = matching[0];
            (loc.segment_seq, loc.offset_in_segment, min_id)
        };

        let mut all_seqs = list_segment_seqs(&self.dir).await?;
        all_seqs.sort_unstable();
        let mut records = Vec::new();

        for &seq in all_seqs.iter().filter(|s| **s >= start_seq) {
            let path = segment_path(&self.dir, seq);
            let mut file = File::open(&path).await?;
            let start_offset_for_segment = if seq == start_seq {
                start_offset
            } else {
                HEADER_LEN
            };
            file.seek(std::io::SeekFrom::Start(start_offset_for_segment))
                .await?;
            let mut offset = start_offset_for_segment;
            loop {
                match read_next_record_with_offset(&mut file, offset).await? {
                    Some((id, payload, _record_offset, total_len)) => {
                        if id >= min_id {
                            records.push(WalRecord { id, payload });
                        }
                        offset = offset
                            .checked_add(total_len)
                            .ok_or_else(|| WalError::Corruption("offset overflow".to_string()))?;
                    }
                    None => break,
                }
            }
        }

        Ok(records)
    }

    /// Access the underlying log directory.
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Lookup the (segment_seq, offset) for a given logical id. Useful for
    /// tests and diagnostics.
    pub async fn lookup_offset(&self, id: u64) -> Option<RecordLocation> {
        let inner = self.index.lock();
        inner.get(&id).copied()
    }
}

impl WalWriter {
    async fn run(mut self, index: Arc<Mutex<HashMap<u64, RecordLocation>>>) -> Result<(), WalError> {
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
        index: &Arc<Mutex<HashMap<u64, RecordLocation>>>,
    ) -> Result<(), WalError> {
        // Compute total batch size to decide whether to roll.
        let batch_size: u64 = records
            .iter()
            .map(|r| RECORD_HEADER_LEN as u64 + r.payload.len() as u64)
            .sum();

        // If adding this batch would push the current segment past
        // `segment_bytes` AND the current segment already holds at least
        // one record beyond the header, roll to a new segment first. Never
        // roll on an empty segment (would create an empty file).
        if self.current_offset > HEADER_LEN
            && self.current_offset.saturating_add(batch_size) > self.segment_bytes
        {
            self.roll_segment().await?;
        }

        self.batch_buf.clear();
        let mut offsets: Vec<(u64, RecordLocation)> = Vec::with_capacity(records.len());
        let mut current_offset = self.current_offset;
        let current_seq = self.current_segment_seq;

        let mut new_acks: Vec<oneshot::Sender<Result<(), WalError>>> =
            Vec::with_capacity(records.len());

        for mut record in records {
            offsets.push((
                record.id,
                RecordLocation {
                    segment_seq: current_seq,
                    offset_in_segment: current_offset,
                },
            ));
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
        self.current_offset = current_offset;

        {
            let mut guard = index.lock();
            for (id, loc) in offsets {
                guard.insert(id, loc);
            }
        }

        self.append_count
            .fetch_add(total_records as u64, Ordering::Relaxed);
        self.bytes_written.fetch_add(total_bytes, Ordering::Relaxed);

        self.unflushed_records = self.unflushed_records.saturating_add(total_records);
        self.pending_durable_acks.extend(new_acks);

        if !self.pending_durable_acks.is_empty() {
            self.flush_file().await?;
        } else {
            self.maybe_sync().await?;
        }

        Ok(())
    }

    async fn roll_segment(&mut self) -> Result<(), WalError> {
        // Flush + fsync the current segment so we never lose records that
        // were in flight when the new segment was created.
        self.flush_file().await?;

        let next_seq = self.current_segment_seq.checked_add(1).ok_or_else(|| {
            WalError::Corruption("segment sequence overflow".to_string())
        })?;
        let path = segment_path(&self.dir, next_seq);
        let mut file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .await?;
        write_header(&mut file).await?;

        self.file = file;
        self.current_segment_seq = next_seq;
        self.current_offset = HEADER_LEN;
        Ok(())
    }

    async fn flush_file(&mut self) -> Result<(), WalError> {
        let result = async {
            self.file.flush().await?;
            self.file.sync_data().await?;
            Ok::<(), WalError>(())
        }
        .await;

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

/// Build the path for segment `seq`. The name is zero-padded to 20 digits
/// (u64 max) so lex-sort matches numeric order.
fn segment_path(dir: &Path, seq: u64) -> PathBuf {
    dir.join(format!("{SEGMENT_PREFIX}{seq:020}{SEGMENT_SUFFIX}"))
}

/// Parse a segment filename like `wal-00000000000000000007.log` into 7.
/// Returns `None` if the filename doesn't match.
fn parse_segment_seq(name: &str) -> Option<u64> {
    let s = name.strip_prefix(SEGMENT_PREFIX)?.strip_suffix(SEGMENT_SUFFIX)?;
    s.parse::<u64>().ok()
}

async fn list_segment_seqs(dir: &Path) -> Result<Vec<u64>, WalError> {
    let mut seqs = Vec::new();
    let mut entries = match fs::read_dir(dir).await {
        Ok(e) => e,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(seqs),
        Err(err) => return Err(WalError::Io(err)),
    };
    while let Some(entry) = entries.next_entry().await? {
        if let Some(name) = entry.file_name().to_str() {
            if let Some(seq) = parse_segment_seq(name) {
                seqs.push(seq);
            }
        }
    }
    Ok(seqs)
}

async fn write_header(file: &mut File) -> Result<(), WalError> {
    let mut buf = [0u8; HEADER_LEN as usize];
    buf[..8].copy_from_slice(HEADER_MAGIC);
    buf[8..12].copy_from_slice(&HEADER_VERSION.to_le_bytes());
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

/// Walk one segment from end-of-header to EOF (or first corruption),
/// returning (records_in_segment, next_id_after_segment, end_offset).
async fn rebuild_segment_index(
    file: &mut File,
) -> Result<(Vec<(u64, u64)>, u64, u64), WalError> {
    let mut records = Vec::new();
    let mut next_id = 1u64;

    file.seek(std::io::SeekFrom::Start(HEADER_LEN)).await?;
    let mut offset = HEADER_LEN;

    loop {
        match read_next_record_with_offset(file, offset).await {
            Ok(Some((id, _payload, record_offset, total_len))) => {
                records.push((id, record_offset));
                next_id = id.wrapping_add(1);
                offset = offset
                    .checked_add(total_len)
                    .ok_or_else(|| WalError::Corruption("offset overflow".to_string()))?;
            }
            Ok(None) => break,
            Err(WalError::Corruption(reason)) => {
                error!("WAL corruption detected during index rebuild: {}", reason);
                return Err(WalError::Corruption(reason));
            }
            Err(e) => return Err(e),
        }
    }

    Ok((records, next_id, offset))
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
                Ok(None)
            } else {
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
            return Ok(None);
        }
        read_payload += n;
    }

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

    /// Test helper: returns a fresh-empty directory under `temp_dir()` for
    /// the named WAL. Removes any leftover from a previous run.
    fn wal_dir(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("wal_test_{name}"));
        let _ = std::fs::remove_dir_all(&path);
        path
    }

    #[tokio::test]
    async fn append_and_iterate_roundtrip() {
        let dir = wal_dir("roundtrip");
        let wal = WriteAheadLog::open(&dir).await.unwrap();

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
        let dir = wal_dir("corruption");
        {
            let wal = WriteAheadLog::open(&dir).await.unwrap();
            let _ = wal.append(Bytes::from_static(b"good")).await.unwrap();
            let _ = wal.append(Bytes::from_static(b"also good")).await.unwrap();
            wal.flush().await.unwrap();
        }

        // Corrupt a byte near the end of segment 1.
        use std::io::{Read, Seek, SeekFrom, Write};

        let segment_1 = segment_path(&dir, 1);
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&segment_1)
            .unwrap();

        file.seek(SeekFrom::End(-1)).unwrap();
        let mut byte = [0u8; 1];
        file.read_exact(&mut byte).unwrap();
        byte[0] ^= 0xFF;
        file.seek(SeekFrom::End(-1)).unwrap();
        file.write_all(&byte).unwrap();
        file.flush().unwrap();

        let err = WriteAheadLog::open(&dir).await.unwrap_err();
        match err {
            WalError::Corruption(_) => {}
            other => panic!("expected corruption error, got {other:?}"),
        }
    }

    /// v3 CRC covers (id, len, payload). Flip a bit in the `id` field of
    /// the first record; reopen must fail with Corruption.
    #[tokio::test]
    async fn header_corruption_caught_by_crc_v2() {
        use std::io::{Read, Seek, SeekFrom, Write};

        let dir = wal_dir("header_corruption");
        {
            let wal = WriteAheadLog::open(&dir).await.unwrap();
            let _ = wal.append(Bytes::from_static(b"first")).await.unwrap();
            let _ = wal.append(Bytes::from_static(b"second")).await.unwrap();
            wal.flush().await.unwrap();
        }

        let id_offset = HEADER_LEN; // first record's id starts immediately after WAL header
        let segment_1 = segment_path(&dir, 1);
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&segment_1)
            .unwrap();

        file.seek(SeekFrom::Start(id_offset)).unwrap();
        let mut byte = [0u8; 1];
        file.read_exact(&mut byte).unwrap();
        byte[0] ^= 0x01;
        file.seek(SeekFrom::Start(id_offset)).unwrap();
        file.write_all(&byte).unwrap();
        file.flush().unwrap();

        let err = WriteAheadLog::open(&dir).await.unwrap_err();
        assert!(
            matches!(err, WalError::Corruption(_)),
            "expected Corruption, got {err:?}",
        );
    }

    /// append_durable must not return until the record is on disk.
    #[tokio::test]
    async fn append_durable_blocks_until_on_disk() {
        let dir = wal_dir("durable_blocks");
        let wal = WriteAheadLog::open(&dir).await.unwrap();

        let segment_1 = segment_path(&dir, 1);
        let len_before = std::fs::metadata(&segment_1).unwrap().len();
        let _ = wal
            .append_durable(Bytes::from_static(b"durable"))
            .await
            .unwrap();
        let len_after = std::fs::metadata(&segment_1).unwrap().len();

        assert!(
            len_after > len_before,
            "segment did not grow after append_durable: before={len_before} after={len_after}",
        );
        assert!(len_after >= len_before + RECORD_HEADER_LEN as u64);
    }

    #[tokio::test]
    async fn index_is_rebuilt_on_open() {
        let dir = wal_dir("index_rebuild");
        let id2;
        {
            let wal = WriteAheadLog::open(&dir).await.unwrap();
            let _id1 = wal.append(Bytes::from_static(b"first")).await.unwrap();
            id2 = wal.append(Bytes::from_static(b"second")).await.unwrap();
            let _id3 = wal.append(Bytes::from_static(b"third")).await.unwrap();
            wal.flush().await.unwrap();
        }

        let wal = WriteAheadLog::open(&dir).await.unwrap();

        let loc = wal.lookup_offset(id2).await;
        assert!(loc.is_some(), "location for id2 should be present");
        assert_eq!(loc.unwrap().segment_seq, 1);

        let records = wal.iterate_from(id2).await.unwrap();
        assert!(!records.is_empty());
        assert_eq!(records[0].id, id2);
        assert_eq!(records[0].payload, Bytes::from_static(b"second"));
    }

    /// Exercise segment rollover: write enough data to force a roll, then
    /// reopen and verify all records are recovered across both segments.
    #[tokio::test]
    async fn segments_roll_at_boundary_and_reopen_recovers_all() {
        let dir = wal_dir("rollover");
        // Tiny segment size so a single record (16 B payload + 16 B record
        // header = 32 B) puts the segment past the limit on the next batch.
        let config = WalConfig {
            segment_bytes: HEADER_LEN + RECORD_HEADER_LEN as u64 + 16,
            ..WalConfig::default()
        };

        let mut written_ids = Vec::new();
        {
            let wal = WriteAheadLog::open_with_config(&dir, config.clone())
                .await
                .unwrap();
            for i in 0..6u8 {
                let payload = Bytes::from(vec![i; 16]);
                let id = wal.append(payload).await.unwrap();
                written_ids.push(id);
                // Force the writer to flush this record as its own batch
                // so subsequent batches see a non-empty segment and trigger
                // the rollover check. Without the flush, all 6 appends
                // queue together into a single batch and the rollover never
                // fires because the segment is empty when the batch starts.
                wal.flush().await.unwrap();
            }
        }

        // Verify multiple segment files exist.
        let mut seqs = list_segment_seqs(&dir).await.unwrap();
        seqs.sort_unstable();
        assert!(seqs.len() >= 2, "expected at least 2 segments, got {seqs:?}");

        // Reopen and read everything back.
        let wal = WriteAheadLog::open_with_config(&dir, config).await.unwrap();
        let records = wal.iterate_from(written_ids[0]).await.unwrap();
        assert_eq!(records.len(), 6);
        for (i, rec) in records.iter().enumerate() {
            assert_eq!(rec.id, written_ids[i]);
        }
    }

    #[tokio::test]
    async fn migration_guard_rejects_old_single_file_wal() {
        // Create a regular file at the WAL path; open() must reject it
        // rather than silently shadowing it.
        let mut path = std::env::temp_dir();
        path.push("wal_test_old_file");
        let _ = std::fs::remove_dir_all(&path);
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, b"v2 single-file wal contents").unwrap();

        let err = WriteAheadLog::open(&path).await.unwrap_err();
        assert!(
            matches!(err, WalError::InvalidConfig(_)),
            "expected InvalidConfig, got {err:?}",
        );
        let _ = std::fs::remove_file(&path);
    }
}
