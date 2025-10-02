//! Windows IOCP implementation for ultra-fast async I/O

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::io;
use std::ptr;
use std::mem;
use std::collections::HashMap;
use bytes::{Bytes, BytesMut, BufMut};
use std::os::windows::io::{AsRawSocket, RawSocket, FromRawSocket};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::IO::*;
use windows_sys::Win32::Networking::WinSock::*;
use windows_sys::Win32::System::Threading::*;

const BUFFER_SIZE: usize = 65536;
const MAX_CONCURRENT_OPS: usize = 4096;

/// Operation type for IOCP
#[derive(Debug, Clone, Copy, PartialEq)]
enum OpType {
    Accept,
    Read,
    Write,
    Close,
}

/// Per-I/O context
struct IoContext {
    overlapped: OVERLAPPED,
    op_type: OpType,
    conn_id: usize,
    buffer: Vec<u8>,
    wsabuf: WSABUF,
}

impl IoContext {
    fn new(op_type: OpType, conn_id: usize, buffer_size: usize) -> Box<Self> {
        let mut ctx = Box::new(Self {
            overlapped: unsafe { mem::zeroed() },
            op_type,
            conn_id,
            buffer: vec![0u8; buffer_size],
            wsabuf: WSABUF {
                len: 0,
                buf: ptr::null_mut(),
            },
        });
        
        // Setup WSABUF
        ctx.wsabuf.len = ctx.buffer.len() as u32;
        ctx.wsabuf.buf = ctx.buffer.as_mut_ptr();
        
        ctx
    }
}

/// Connection state
struct Connection {
    socket: SOCKET,
    addr: SocketAddr,
    read_buffer: BytesMut,
    write_buffer: BytesMut,
    pending_reads: usize,
    pending_writes: usize,
}

/// IOCP-based I/O backend
pub struct IocpBackend {
    iocp: HANDLE,
    listener: Option<TcpListener>,
    connections: HashMap<usize, Connection>,
    next_conn_id: AtomicU64,
    accept_ex: Option<LPFN_ACCEPTEX>,
    get_accept_ex_sockaddrs: Option<LPFN_GETACCEPTEXSOCKADDRS>,
    num_threads: usize,
}

impl IocpBackend {
    pub fn new(num_threads: usize) -> io::Result<Self> {
        // Initialize Winsock
        unsafe {
            let mut wsa_data: WSADATA = mem::zeroed();
            let result = WSAStartup(0x0202, &mut wsa_data);
            if result != 0 {
                return Err(io::Error::from_raw_os_error(result));
            }
        }
        
        // Create IOCP with optimal thread count
        let iocp = unsafe {
            CreateIoCompletionPort(
                INVALID_HANDLE_VALUE,
                0,
                0,
                num_threads as u32,
            )
        };
        
        if iocp == 0 {
            return Err(io::Error::last_os_error());
        }
        
        Ok(Self {
            iocp,
            listener: None,
            connections: HashMap::with_capacity(10000),
            next_conn_id: AtomicU64::new(1),
            accept_ex: None,
            get_accept_ex_sockaddrs: None,
            num_threads,
        })
    }
    
    /// Initialize listener and load AcceptEx
    pub fn init(&mut self, addr: SocketAddr) -> io::Result<()> {
        let listener = TcpListener::bind(addr)?;
        let socket = listener.as_raw_socket() as SOCKET;
        
        // Associate listener with IOCP
        unsafe {
            let result = CreateIoCompletionPort(
                socket as HANDLE,
                self.iocp,
                0, // Use overlapped structure for context
                0,
            );
            
            if result == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        
        // Load AcceptEx and GetAcceptExSockaddrs
        self.load_accept_ex(socket)?;
        
        // Post initial accepts
        for _ in 0..128 {
            self.post_accept(socket)?;
        }
        
        self.listener = Some(listener);
        Ok(())
    }
    
    /// Load AcceptEx function pointer
    fn load_accept_ex(&mut self, socket: SOCKET) -> io::Result<()> {
        unsafe {
            let mut bytes_returned: u32 = 0;
            
            // GUID for AcceptEx
            let accept_ex_guid = GUID {
                Data1: 0xb5367df1,
                Data2: 0xcbac,
                Data3: 0x11cf,
                Data4: [0x95, 0xca, 0x00, 0x80, 0x5f, 0x48, 0xa1, 0x92],
            };
            
            let mut accept_ex_ptr: LPFN_ACCEPTEX = None;
            let result = WSAIoctl(
                socket,
                SIO_GET_EXTENSION_FUNCTION_POINTER,
                &accept_ex_guid as *const _ as *const _,
                mem::size_of::<GUID>() as u32,
                &mut accept_ex_ptr as *mut _ as *mut _,
                mem::size_of::<LPFN_ACCEPTEX>() as u32,
                &mut bytes_returned,
                ptr::null_mut(),
                None,
            );
            
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
            
            self.accept_ex = accept_ex_ptr;
            
            // GUID for GetAcceptExSockaddrs
            let get_sockaddrs_guid = GUID {
                Data1: 0xb5367df2,
                Data2: 0xcbac,
                Data3: 0x11cf,
                Data4: [0x95, 0xca, 0x00, 0x80, 0x5f, 0x48, 0xa1, 0x92],
            };
            
            let mut get_sockaddrs_ptr: LPFN_GETACCEPTEXSOCKADDRS = None;
            let result = WSAIoctl(
                socket,
                SIO_GET_EXTENSION_FUNCTION_POINTER,
                &get_sockaddrs_guid as *const _ as *const _,
                mem::size_of::<GUID>() as u32,
                &mut get_sockaddrs_ptr as *mut _ as *mut _,
                mem::size_of::<LPFN_GETACCEPTEXSOCKADDRS>() as u32,
                &mut bytes_returned,
                ptr::null_mut(),
                None,
            );
            
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
            
            self.get_accept_ex_sockaddrs = get_sockaddrs_ptr;
        }
        
        Ok(())
    }
    
    /// Post an accept operation
    fn post_accept(&self, listener: SOCKET) -> io::Result<()> {
        unsafe {
            // Create accept socket
            let accept_socket = WSASocketW(
                AF_INET,
                SOCK_STREAM,
                IPPROTO_TCP,
                ptr::null_mut(),
                0,
                WSA_FLAG_OVERLAPPED,
            );
            
            if accept_socket == INVALID_SOCKET {
                return Err(io::Error::last_os_error());
            }
            
            // Create I/O context
            let mut ctx = IoContext::new(OpType::Accept, 0, 256);
            
            // Store accept socket in buffer (hack but works)
            let socket_bytes = accept_socket.to_ne_bytes();
            ctx.buffer[0..8].copy_from_slice(&socket_bytes);
            
            let mut bytes_received: u32 = 0;
            
            if let Some(accept_ex) = self.accept_ex {
                let result = accept_ex(
                    listener,
                    accept_socket,
                    ctx.buffer.as_mut_ptr().add(8) as *mut _,
                    0, // No initial data
                    116, // Size of local address + 16
                    116, // Size of remote address + 16
                    &mut bytes_received,
                    &mut ctx.overlapped,
                );
                
                if result == 0 {
                    let error = WSAGetLastError();
                    if error != WSA_IO_PENDING {
                        closesocket(accept_socket);
                        return Err(io::Error::from_raw_os_error(error));
                    }
                }
                
                // Leak context - will be retrieved in completion
                Box::leak(ctx);
            }
        }
        
        Ok(())
    }
    
    /// Post a read operation
    pub fn post_read(&mut self, conn_id: usize) -> io::Result<()> {
        if let Some(conn) = self.connections.get_mut(&conn_id) {
            unsafe {
                let mut ctx = IoContext::new(OpType::Read, conn_id, BUFFER_SIZE);
                let mut flags: u32 = 0;
                let mut bytes_received: u32 = 0;
                
                let result = WSARecv(
                    conn.socket,
                    &mut ctx.wsabuf,
                    1,
                    &mut bytes_received,
                    &mut flags,
                    &mut ctx.overlapped,
                    None,
                );
                
                if result == SOCKET_ERROR {
                    let error = WSAGetLastError();
                    if error != WSA_IO_PENDING {
                        return Err(io::Error::from_raw_os_error(error));
                    }
                }
                
                conn.pending_reads += 1;
                Box::leak(ctx);
            }
        }
        
        Ok(())
    }
    
    /// Post a write operation
    pub fn post_write(&mut self, conn_id: usize, data: Bytes) -> io::Result<()> {
        if let Some(conn) = self.connections.get_mut(&conn_id) {
            unsafe {
                let mut ctx = IoContext::new(OpType::Write, conn_id, data.len());
                ctx.buffer.copy_from_slice(&data);
                ctx.wsabuf.len = data.len() as u32;
                ctx.wsabuf.buf = ctx.buffer.as_mut_ptr();
                
                let mut bytes_sent: u32 = 0;
                
                let result = WSASend(
                    conn.socket,
                    &ctx.wsabuf,
                    1,
                    &mut bytes_sent,
                    0,
                    &mut ctx.overlapped,
                    None,
                );
                
                if result == SOCKET_ERROR {
                    let error = WSAGetLastError();
                    if error != WSA_IO_PENDING {
                        return Err(io::Error::from_raw_os_error(error));
                    }
                }
                
                conn.pending_writes += 1;
                Box::leak(ctx);
            }
        }
        
        Ok(())
    }
    
    /// Poll for completion events
    pub fn poll(&mut self, timeout_ms: u32) -> io::Result<Vec<IoEvent>> {
        let mut events = Vec::with_capacity(64);
        
        unsafe {
            let mut entries: [OVERLAPPED_ENTRY; 64] = mem::zeroed();
            let mut num_entries: u32 = 0;
            
            let result = GetQueuedCompletionStatusEx(
                self.iocp,
                entries.as_mut_ptr(),
                64,
                &mut num_entries,
                timeout_ms,
                0,
            );
            
            if result == 0 {
                let error = GetLastError();
                if error == WAIT_TIMEOUT {
                    return Ok(events);
                }
                return Err(io::Error::from_raw_os_error(error as i32));
            }
            
            for i in 0..num_entries as usize {
                let entry = &entries[i];
                
                if entry.lpOverlapped.is_null() {
                    continue;
                }
                
                // Retrieve context
                let ctx = Box::from_raw(entry.lpOverlapped as *mut IoContext);
                
                match ctx.op_type {
                    OpType::Accept => {
                        // Extract accept socket from buffer
                        let mut socket_bytes = [0u8; 8];
                        socket_bytes.copy_from_slice(&ctx.buffer[0..8]);
                        let accept_socket = SOCKET::from_ne_bytes(socket_bytes);
                        
                        // Associate with IOCP
                        let conn_id = self.next_conn_id.fetch_add(1, Ordering::SeqCst) as usize;
                        
                        CreateIoCompletionPort(
                            accept_socket as HANDLE,
                            self.iocp,
                            conn_id,
                            0,
                        );
                        
                        // Update socket options
                        let nodelay: i32 = 1;
                        setsockopt(
                            accept_socket,
                            IPPROTO_TCP,
                            TCP_NODELAY,
                            &nodelay as *const _ as *const _,
                            mem::size_of::<i32>() as i32,
                        );
                        
                        // Store connection
                        self.connections.insert(conn_id, Connection {
                            socket: accept_socket,
                            addr: "0.0.0.0:0".parse().unwrap(), // TODO: Extract real address
                            read_buffer: BytesMut::with_capacity(BUFFER_SIZE),
                            write_buffer: BytesMut::with_capacity(BUFFER_SIZE),
                            pending_reads: 0,
                            pending_writes: 0,
                        });
                        
                        events.push(IoEvent {
                            conn_id,
                            event_type: EventType::Accept,
                            data: None,
                        });
                        
                        // Post initial read
                        self.post_read(conn_id)?;
                        
                        // Post another accept
                        if let Some(listener) = &self.listener {
                            self.post_accept(listener.as_raw_socket() as SOCKET)?;
                        }
                    }
                    OpType::Read => {
                        let bytes_transferred = entry.dwNumberOfBytesTransferred;
                        
                        if bytes_transferred > 0 {
                            let data = Bytes::copy_from_slice(&ctx.buffer[..bytes_transferred as usize]);
                            
                            events.push(IoEvent {
                                conn_id: ctx.conn_id,
                                event_type: EventType::Read,
                                data: Some(data),
                            });
                            
                            // Post another read
                            if let Some(conn) = self.connections.get_mut(&ctx.conn_id) {
                                conn.pending_reads -= 1;
                            }
                            self.post_read(ctx.conn_id)?;
                        } else {
                            // Connection closed
                            events.push(IoEvent {
                                conn_id: ctx.conn_id,
                                event_type: EventType::Close,
                                data: None,
                            });
                            
                            self.close_connection(ctx.conn_id);
                        }
                    }
                    OpType::Write => {
                        if let Some(conn) = self.connections.get_mut(&ctx.conn_id) {
                            conn.pending_writes -= 1;
                        }
                        
                        events.push(IoEvent {
                            conn_id: ctx.conn_id,
                            event_type: EventType::Write,
                            data: None,
                        });
                    }
                    OpType::Close => {
                        self.close_connection(ctx.conn_id);
                    }
                }
            }
        }
        
        Ok(events)
    }
    
    /// Close a connection
    fn close_connection(&mut self, conn_id: usize) {
        if let Some(conn) = self.connections.remove(&conn_id) {
            unsafe {
                shutdown(conn.socket, SD_BOTH);
                closesocket(conn.socket);
            }
        }
    }
}

/// I/O event
pub struct IoEvent {
    pub conn_id: usize,
    pub event_type: EventType,
    pub data: Option<Bytes>,
}

#[derive(Debug, Clone, Copy)]
pub enum EventType {
    Accept,
    Read,
    Write,
    Close,
}

// Type definitions for Windows function pointers
type LPFN_ACCEPTEX = Option<unsafe extern "system" fn(
    sListenSocket: SOCKET,
    sAcceptSocket: SOCKET,
    lpOutputBuffer: *mut std::ffi::c_void,
    dwReceiveDataLength: u32,
    dwLocalAddressLength: u32,
    dwRemoteAddressLength: u32,
    lpdwBytesReceived: *mut u32,
    lpOverlapped: *mut OVERLAPPED,
) -> BOOL>;

type LPFN_GETACCEPTEXSOCKADDRS = Option<unsafe extern "system" fn(
    lpOutputBuffer: *mut std::ffi::c_void,
    dwReceiveDataLength: u32,
    dwLocalAddressLength: u32,
    dwRemoteAddressLength: u32,
    LocalSockaddr: *mut *mut SOCKADDR,
    LocalSockaddrLength: *mut i32,
    RemoteSockaddr: *mut *mut SOCKADDR,
    RemoteSockaddrLength: *mut i32,
)>;

// GUID definition
#[repr(C)]
struct GUID {
    Data1: u32,
    Data2: u16,
    Data3: u16,
    Data4: [u8; 8],
}

// Additional constants
const SIO_GET_EXTENSION_FUNCTION_POINTER: i32 = -939524090;
const TCP_NODELAY: i32 = 0x0001;
const SD_BOTH: i32 = 2;
const WSA_IO_PENDING: i32 = 997;
const WAIT_TIMEOUT: u32 = 258;
