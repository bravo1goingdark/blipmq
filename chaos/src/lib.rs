use std::io;
use std::path::Path;
use std::time::Duration;

use rand::Rng;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::time;

/// Simulate a crash by dropping the given value, releasing all owned resources.
pub fn simulate_crash<T>(value: T) {
    drop(value);
}

/// Inject a random delay with the given maximum duration and probability.
pub async fn random_delay(max_delay: Duration, probability: f64) {
    if probability <= 0.0 {
        return;
    }
    let mut rng = rand::thread_rng();
    let p: f64 = rng.gen();
    if p <= probability {
        let millis = rng.gen_range(0..=max_delay.as_millis().max(1) as u64);
        time::sleep(Duration::from_millis(millis)).await;
    }
}

/// Flip a random byte near the end of the given file to simulate WAL corruption.
pub fn corrupt_file_tail_bit<P: AsRef<Path>>(path: P) -> io::Result<()> {
    use std::fs::OpenOptions;
    use std::io::{Read, Seek, SeekFrom, Write};

    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(());
    }

    let mut rng = rand::thread_rng();
    let offset_back: u64 = rng.gen_range(1..=len.min(16));
    file.seek(SeekFrom::End(-(offset_back as i64)))?;

    let mut byte = [0u8; 1];
    file.read_exact(&mut byte)?;
    byte[0] ^= 0x01;
    file.seek(SeekFrom::End(-(offset_back as i64)))?;
    file.write_all(&byte)?;
    file.flush()?;
    Ok(())
}

/// Wrap an async write operation with an artificial delay to simulate a slow disk.
pub async fn simulate_slow_disk_write<F, Fut, T>(delay: Duration, write_op: F) -> io::Result<T>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = io::Result<T>>,
{
    time::sleep(delay).await;
    write_op().await
}

/// Create an `io::Error` representing a full disk situation.
pub fn full_disk_error() -> io::Error {
    io::Error::other("simulated full disk")
}

/// With the given probability, abruptly close the TCP connection to simulate a
/// random network disconnect.
pub async fn maybe_disconnect(stream: &mut TcpStream, probability: f64) {
    if probability <= 0.0 {
        return;
    }
    let mut rng = rand::thread_rng();
    let p: f64 = rng.gen();
    if p <= probability {
        let _ = stream.shutdown().await;
    }
}
