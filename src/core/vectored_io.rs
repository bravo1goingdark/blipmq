//! High-performance vectored I/O implementation for batch operations.
//! 
//! Provides utilities for batching multiple buffers into single system calls
//! to reduce overhead and improve throughput.

use std::io::{IoSlice, Result as IoResult};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use bytes::{Bytes, BytesMut};
use std::pin::Pin;
use std::task::{Context, Poll};

/// Batch writer that accumulates data and flushes using vectored I/O
#[derive(Debug)]
pub struct BatchWriter<W> {
    inner: W,
    buffers: Vec<Bytes>,
    total_size: usize,
    max_batch_size: usize,
    max_batch_count: usize,
}

impl<W: AsyncWrite + Unpin> BatchWriter<W> {
    pub fn new(writer: W) -> Self {
        Self::with_limits(writer, 64 * 1024, 16) // 64KB max batch, 16 buffers max
    }
    
    pub fn with_limits(writer: W, max_batch_size: usize, max_batch_count: usize) -> Self {
        Self {
            inner: writer,
            buffers: Vec::with_capacity(max_batch_count),
            total_size: 0,
            max_batch_size,
            max_batch_count,
        }
    }
    
    /// Add a buffer to the batch
    pub async fn add_buffer(&mut self, buffer: Bytes) -> IoResult<()> {
        self.total_size += buffer.len();
        self.buffers.push(buffer);
        
        // Flush if we've reached limits
        if self.buffers.len() >= self.max_batch_count || self.total_size >= self.max_batch_size {
            self.flush_batch().await?;
        }
        
        Ok(())
    }
    
    /// Force flush all pending buffers
    pub async fn flush_batch(&mut self) -> IoResult<()> {
        if self.buffers.is_empty() {
            return Ok(());
        }
        
        // Convert to IoSlice for vectored write
        let io_slices: Vec<IoSlice<'_>> = self.buffers
            .iter()
            .map(|buf| IoSlice::new(buf))
            .collect();
        
        // Write all buffers at once
        self.inner.write_vectored(&io_slices).await?;
        self.inner.flush().await?;
        
        // Clear the batch
        self.buffers.clear();
        self.total_size = 0;
        
        Ok(())
    }
    
    /// Get the number of pending buffers
    pub fn pending_count(&self) -> usize {
        self.buffers.len()
    }
    
    /// Get the total size of pending data
    pub fn pending_size(&self) -> usize {
        self.total_size
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for BatchWriter<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8]
    ) -> Poll<IoResult<usize>> {
        // For direct writes, bypass batching
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<IoResult<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<IoResult<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Optimized message encoder that minimizes allocations
#[derive(Debug)]
pub struct OptimizedEncoder {
    buffer: BytesMut,
    scratch_buffer: Vec<u8>,
}

impl OptimizedEncoder {
    pub fn new() -> Self {
        Self {
            buffer: BytesMut::with_capacity(4096),
            scratch_buffer: Vec::with_capacity(1024),
        }
    }
    
    pub fn encode_message_batch(&mut self, messages: &[&crate::core::message::Message]) -> Bytes {
        self.buffer.clear();
        
        for &message in messages {
            self.encode_single_message(message);
        }
        
        self.buffer.split().freeze()
    }
    
    fn encode_single_message(&mut self, message: &crate::core::message::Message) {
        // Use our scratch buffer for FlatBuffer serialization
        self.scratch_buffer.clear();
        
        // Serialize message (simplified - in real implementation use FlatBuffers)
        let message_data = format!("{}:{}:{}", message.id, message.timestamp, 
                                  String::from_utf8_lossy(&message.payload));
        
        let data_bytes = message_data.as_bytes();
        
        // Write length prefix
        self.buffer.extend_from_slice(&(data_bytes.len() as u32).to_be_bytes());
        
        // Write message data
        self.buffer.extend_from_slice(data_bytes);
    }
    
    pub fn clear(&mut self) {
        self.buffer.clear();
        self.scratch_buffer.clear();
    }
}

/// Zero-copy message splitter for efficient parsing
#[derive(Debug)]
pub struct MessageSplitter {
    buffer: BytesMut,
}

impl MessageSplitter {
    pub fn new() -> Self {
        Self {
            buffer: BytesMut::with_capacity(8192),
        }
    }
    
    pub fn add_data(&mut self, data: &[u8]) {
        self.buffer.extend_from_slice(data);
    }
    
    pub fn extract_messages(&mut self) -> Vec<Bytes> {
        let mut messages = Vec::new();
        
        while self.buffer.len() >= 4 {
            // Read length prefix
            let len_bytes = [
                self.buffer[0],
                self.buffer[1], 
                self.buffer[2],
                self.buffer[3]
            ];
            let message_len = u32::from_be_bytes(len_bytes) as usize;
            
            if self.buffer.len() >= 4 + message_len {
                // Skip length prefix
                self.buffer.advance(4);
                
                // Extract message
                let message_data = self.buffer.split_to(message_len).freeze();
                messages.push(message_data);
            } else {
                break; // Not enough data for complete message
            }
        }
        
        messages
    }
    
    pub fn remaining_bytes(&self) -> usize {
        self.buffer.len()
    }
}

/// High-performance connection wrapper with optimized I/O
pub struct OptimizedConnection<R, W> {
    reader: R,
    batch_writer: BatchWriter<W>,
    encoder: OptimizedEncoder,
    splitter: MessageSplitter,
    read_buffer: Vec<u8>,
}

impl<R: tokio::io::AsyncRead + Unpin, W: AsyncWrite + Unpin> OptimizedConnection<R, W> {
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader,
            batch_writer: BatchWriter::new(writer),
            encoder: OptimizedEncoder::new(),
            splitter: MessageSplitter::new(),
            read_buffer: vec![0u8; 8192],
        }
    }
    
    /// Read and parse multiple messages efficiently
    pub async fn read_messages(&mut self) -> IoResult<Vec<Bytes>> {
        use tokio::io::AsyncReadExt;
        
        // Read data into buffer
        let bytes_read = self.reader.read(&mut self.read_buffer).await?;
        if bytes_read == 0 {
            return Ok(Vec::new()); // EOF
        }
        
        // Add to splitter and extract complete messages
        self.splitter.add_data(&self.read_buffer[..bytes_read]);
        Ok(self.splitter.extract_messages())
    }
    
    /// Write multiple messages in a single batch
    pub async fn write_messages(&mut self, messages: &[&crate::core::message::Message]) -> IoResult<()> {
        if messages.is_empty() {
            return Ok(());
        }
        
        // Encode all messages into a single buffer
        let encoded_data = self.encoder.encode_message_batch(messages);
        
        // Add to batch writer
        self.batch_writer.add_buffer(encoded_data).await?;
        
        Ok(())
    }
    
    /// Force flush any pending writes
    pub async fn flush(&mut self) -> IoResult<()> {
        self.batch_writer.flush_batch().await
    }
    
    /// Get pending write statistics
    pub fn pending_stats(&self) -> (usize, usize) {
        (self.batch_writer.pending_count(), self.batch_writer.pending_size())
    }
}

/// Utilities for vectored I/O operations
pub mod utils {
    use super::*;
    
    /// Split large buffer into optimal chunks for vectored I/O
    pub fn split_for_vectored_io(data: &[u8], max_chunk_size: usize) -> Vec<&[u8]> {
        data.chunks(max_chunk_size).collect()
    }
    
    /// Calculate optimal buffer sizes based on system capabilities
    pub fn calculate_optimal_buffer_sizes() -> (usize, usize, usize) {
        // In a real implementation, this would query system capabilities
        // For now, return reasonable defaults
        (8192, 32768, 16) // read_buffer_size, write_buffer_size, max_vectored_count
    }
    
    /// Prepare multiple Bytes for efficient vectored write
    pub fn prepare_vectored_buffers(buffers: &[Bytes]) -> Vec<IoSlice<'_>> {
        buffers.iter().map(|buf| IoSlice::new(buf)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_test::io::Builder;
    
    #[tokio::test]
    async fn test_batch_writer() {
        let mock_writer = Builder::new()
            .write(b"hello")
            .write(b"world")
            .build();
            
        let mut batch_writer = BatchWriter::new(mock_writer);
        
        batch_writer.add_buffer(Bytes::from_static(b"hello")).await.unwrap();
        batch_writer.add_buffer(Bytes::from_static(b"world")).await.unwrap();
        
        batch_writer.flush_batch().await.unwrap();
    }
    
    #[test]
    fn test_message_splitter() {
        let mut splitter = MessageSplitter::new();
        
        // Add a complete message (4-byte length + data)
        let message_data = b"test message";
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&(message_data.len() as u32).to_be_bytes());
        buffer.extend_from_slice(message_data);
        
        splitter.add_data(&buffer);
        let messages = splitter.extract_messages();
        
        assert_eq!(messages.len(), 1);
        assert_eq!(&messages[0][..], message_data);
    }
}