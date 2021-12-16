use std::time::Duration;

use tokio::io::split;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time;
use tokio_tfo::TfoStream;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let stream = TfoStream::connect("127.0.0.1:80".parse().unwrap()).await.unwrap();
    let (mut reader, mut writer) = split(stream);

    tokio::spawn(async move {
        time::sleep(Duration::from_secs(1)).await;

        let buffer = b"GET / HTTP/1.1\r\n\r\n";
        writer.write_all(buffer).await.unwrap();
    });

    let mut buffer = [0u8; 10240];
    let n = reader.read(&mut buffer).await.unwrap();
    println!("{:?}", &buffer[..n]);
}
