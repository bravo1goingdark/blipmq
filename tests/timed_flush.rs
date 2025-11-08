use bytes::{BufMut, BytesMut};
use tokio::io::AsyncReadExt;

#[tokio::test]
async fn timed_flush_sla_small_frames() {
    let (client, mut server) = tokio::io::duplex(1024);
    let tx = blipmq::core::subscriber::spawn_connection_writer(client, 1024);

    // Send one tiny frame (will rely on flush timer)
    let mut f = BytesMut::new();
    f.put_u32(1);
    f.extend_from_slice(&[0xCC]);
    tx.send(f.freeze()).await.unwrap();

    // Expect to read it within a modest SLA (50ms)
    use tokio::time::{timeout, Duration};
    let mut buf = [0u8; 5];
    let res = timeout(Duration::from_millis(50), server.read_exact(&mut buf)).await;
    res.expect("writer did not flush within SLA")
        .expect("failed to read frame");

    let expected = [0x00, 0x00, 0x00, 0x01, 0xCC];
    assert_eq!(buf, expected, "unexpected frame contents");
}
