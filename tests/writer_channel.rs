use bytes::{BufMut, Bytes, BytesMut};
use tokio::io::AsyncReadExt;

#[tokio::test]
async fn writer_channel_orders_and_flushes() {
    // Create a duplex in-memory stream and spawn writer on the write half
    let (client, mut server) = tokio::io::duplex(1024);
    let tx = blipmq::core::subscriber::spawn_connection_writer(client, 1024);

    // Prepare two small fake frames (length + byte)
    let mut f1 = BytesMut::new();
    f1.put_u32(1);
    f1.extend_from_slice(&[0xAA]);
    let f1: Bytes = f1.freeze();

    let mut f2 = BytesMut::new();
    f2.put_u32(1);
    f2.extend_from_slice(&[0xBB]);
    let f2: Bytes = f2.freeze();

    // Send both frames quickly
    tx.send(f1.clone()).await.unwrap();
    tx.send(f2.clone()).await.unwrap();

    // Read back exactly the concatenation
    let mut buf = [0u8; 10];
    server.read_exact(&mut buf).await.unwrap();

    // Expect the bytes to match the order f1 || f2
    let expected: Vec<u8> = [&f1[..], &f2[..]].concat();
    assert_eq!(&buf[..], &expected[..]);
}
