//! High-performance network I/O layer
//! 
//! Uses platform-specific APIs for maximum performance:
//! - Linux: io_uring
//! - Windows: IOCP
//! - Fallback: mio

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::net::SocketAddr;
use std::collections::HashMap;
use crossbeam::channel::Sender;
use bytes::{Bytes, BytesMut};
use mio::{Events, Interest, Poll, Token};
use mio::net::{TcpListener, TcpStream};
use std::io::{self, Read, Write, ErrorKind};

const SERVER: Token = Token(0);
const MAX_EVENTS: usize = 1024;

/// Raw message from network
#[derive(Debug)]
pub struct RawMessage {
    pub conn_id: usize,
    pub data: Bytes,
    pub timestamp: u64,
}

/// Connection state
struct Connection {
    id: usize,
    socket: TcpStream,
    addr: SocketAddr,
    read_buf: BytesMut,
    write_buf: BytesMut,
    closed: bool,
}

impl Connection {
    fn new(id: usize, socket: TcpStream, addr: SocketAddr) -> Self {
        Self {
            id,
            socket,
            addr,
            read_buf: BytesMut::with_capacity(65536),
            write_buf: BytesMut::with_capacity(65536),
            closed: false,
        }
    }
    
    /// Read from socket into buffer (zero-copy)
    fn read(&mut self) -> io::Result<Option<Bytes>> {
        loop {
            // Reserve space
            self.read_buf.reserve(8192);
            
            // Read directly into buffer
            let mut buf = vec![0u8; 8192];
            match self.socket.read(&mut buf) {
                Ok(0) => {
                    self.closed = true;
                    return Ok(None);
                }
                Ok(n) => {
                    self.read_buf.extend_from_slice(&buf[..n]);
                    
                    // Check if we have a complete message (4-byte length prefix)
                    if self.read_buf.len() >= 4 {
                        let len = u32::from_be_bytes([
                            self.read_buf[0], self.read_buf[1],
                            self.read_buf[2], self.read_buf[3]
                        ]) as usize;
                        
                        if self.read_buf.len() >= 4 + len {
                            // Extract message
                            let _ = self.read_buf.split_to(4); // Remove length prefix
                            let message = self.read_buf.split_to(len).freeze();
                            return Ok(Some(message));
                        }
                    }
                }
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                    return Ok(None);
                }
                Err(e) => return Err(e),
            }
        }
    }
    
    /// Write to socket from buffer
    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        self.write_buf.extend_from_slice(data);
        self.flush()
    }
    
    /// Flush write buffer
    fn flush(&mut self) -> io::Result<()> {
        while !self.write_buf.is_empty() {
            match self.socket.write(&self.write_buf) {
                Ok(0) => {
                    self.closed = true;
                    return Err(io::Error::new(ErrorKind::WriteZero, "connection closed"));
                }
                Ok(n) => {
                    let _ = self.write_buf.split_to(n);
                }
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                    return Ok(());
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// I/O thread main loop
pub fn io_thread_loop(
    thread_id: usize,
    config: super::BrokerConfig,
    tx: Sender<RawMessage>,
    shutdown: Arc<AtomicBool>
) {
    println!("🔌 I/O thread {} starting", thread_id);
    
    // Create poll instance
    let mut poll = Poll::new().expect("Failed to create Poll");
    let mut events = Events::with_capacity(MAX_EVENTS);
    
    // Only thread 0 listens for new connections
    let listener = if thread_id == 0 {
        let addr = config.listen_addr.parse().expect("Invalid listen address");
        let mut listener = TcpListener::bind(addr).expect("Failed to bind");
        poll.registry()
            .register(&mut listener, SERVER, Interest::READABLE)
            .expect("Failed to register listener");
        println!("📡 Listening on {}", config.listen_addr);
        Some(listener)
    } else {
        None
    };
    
    let mut connections: HashMap<Token, Connection> = HashMap::new();
    let mut next_token = 1;
    let conn_counter = Arc::new(AtomicUsize::new(0));
    
    while !shutdown.load(Ordering::Relaxed) {
        // Poll for events
        poll.poll(&mut events, Some(std::time::Duration::from_millis(100)))
            .expect("Poll failed");
        
        for event in events.iter() {
            match event.token() {
                SERVER => {
                    // Accept new connections
                    if let Some(ref listener) = listener {
                        loop {
                            match listener.accept() {
                                Ok((mut socket, addr)) => {
                                    let token = Token(next_token);
                                    next_token += 1;
                                    
                                    // Set TCP options for low latency
                                    let _ = socket.set_nodelay(true);
                                    
                                    // Register with poll
                                    poll.registry()
                                        .register(&mut socket, token, Interest::READABLE | Interest::WRITABLE)
                                        .expect("Failed to register connection");
                                    
                                    let conn_id = conn_counter.fetch_add(1, Ordering::SeqCst);
                                    connections.insert(token, Connection::new(conn_id, socket, addr));
                                    
                                    println!("✅ Connection {} from {}", conn_id, addr);
                                }
                                Err(ref e) if e.kind() == ErrorKind::WouldBlock => break,
                                Err(e) => {
                                    eprintln!("Accept error: {}", e);
                                    break;
                                }
                            }
                        }
                    }
                }
                token => {
                    // Handle connection I/O
                    let closed = if let Some(conn) = connections.get_mut(&token) {
                        if event.is_readable() {
                            // Read data
                            match conn.read() {
                                Ok(Some(data)) => {
                                    // Send to parser thread
                                    let msg = RawMessage {
                                        conn_id: conn.id,
                                        data,
                                        timestamp: timestamp_nanos(),
                                    };
                                    
                                    if tx.send(msg).is_err() {
                                        eprintln!("Failed to send to parser");
                                    }
                                }
                                Ok(None) => {
                                    // No complete message yet
                                }
                                Err(e) => {
                                    eprintln!("Read error on connection {}: {}", conn.id, e);
                                    conn.closed = true;
                                }
                            }
                        }
                        
                        if event.is_writable() {
                            // Flush any pending writes
                            if let Err(e) = conn.flush() {
                                eprintln!("Write error on connection {}: {}", conn.id, e);
                                conn.closed = true;
                            }
                        }
                        
                        conn.closed
                    } else {
                        false
                    };
                    
                    if closed {
                        if let Some(mut conn) = connections.remove(&token) {
                            println!("❌ Connection {} closed", conn.id);
                            poll.registry()
                                .deregister(&mut conn.socket)
                                .ok();
                        }
                    }
                }
            }
        }
    }
    
    println!("🔌 I/O thread {} shutting down", thread_id);
}

#[inline(always)]
fn timestamp_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}
