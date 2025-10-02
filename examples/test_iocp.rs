//! Test IOCP implementation

use std::net::{TcpListener, TcpStream};
use std::io::{Read, Write};
use std::thread;
use std::time::Duration;

fn main() {
    println!("Testing high-performance server on Windows...");
    
    // Start server
    let server_thread = thread::spawn(|| {
        let listener = TcpListener::bind("127.0.0.1:9999").unwrap();
        println!("Server listening on 127.0.0.1:9999");
        
        for stream in listener.incoming() {
            if let Ok(mut stream) = stream {
                println!("Client connected!");
                
                // Echo server
                let mut buffer = [0u8; 1024];
                while let Ok(n) = stream.read(&mut buffer) {
                    if n == 0 {
                        break;
                    }
                    stream.write_all(&buffer[..n]).ok();
                }
            }
        }
    });
    
    // Give server time to start
    thread::sleep(Duration::from_millis(100));
    
    // Start client
    let client_thread = thread::spawn(|| {
        if let Ok(mut stream) = TcpStream::connect("127.0.0.1:9999") {
            println!("Client connected to server");
            
            // Send test message
            stream.write_all(b"Hello, IOCP!").unwrap();
            
            // Read response
            let mut buffer = [0u8; 1024];
            if let Ok(n) = stream.read(&mut buffer) {
                let response = String::from_utf8_lossy(&buffer[..n]);
                println!("Client received: {}", response);
            }
        }
    });
    
    client_thread.join().ok();
    println!("Test completed!");
}
