use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpSocket,
};
use tokio_tfo::{TfoListener, TfoStream};

#[tokio::test]
async fn from_std() {
    let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    // For TfoListener::from_std, there is no need to set it in advance.
    // server.set_nonblocking(true).unwrap();

    let server = TfoListener::from_std(server).unwrap();
    test_echo(server).await;
}

#[tokio::test]
async fn from_tokio() {
    let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = TfoListener::from_tokio(server).unwrap();
    test_echo(server).await;
}

async fn test_echo(server: TfoListener) {
    let server_addr = server.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let (mut stream, peer_addr) = server.accept().await.unwrap();
            println!("accepted {}", peer_addr);

            tokio::spawn(async move {
                let mut buffer = [0u8; 4096];
                loop {
                    let n = stream.read(&mut buffer).await.unwrap();
                    if n == 0 {
                        break;
                    }

                    let _ = stream.write_all(&buffer[..n]).await;
                }
            });
        }
    });

    const TEST_PAYLOAD: &[u8] = b"hello world";

    let socket = TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();

    let mut client = TfoStream::connect_with_socket(socket, server_addr).await.unwrap();
    client.write_all(TEST_PAYLOAD).await.unwrap();

    let mut buffer = [0u8; 1024];
    let n = client.read(&mut buffer).await.unwrap();
    assert_eq!(&buffer[..n], TEST_PAYLOAD);
}
