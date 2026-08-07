//! The ceiling: a dumb bidirectional TCP relay.
//!
//! Parses nothing, frames nothing, decides nothing. No proxy that speaks HTTP
//! can beat this on the same path, so it bounds what TODO item 3 (byte-level
//! relay after the response is committed) could possibly be worth.

use tokio::net::{TcpListener, TcpStream};

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("PORT")
        .unwrap_or_else(|_| "4300".into())
        .parse()
        .unwrap();
    let upstream = std::env::var("UPSTREAM").unwrap_or_else(|_| "127.0.0.1:8100".into());
    let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    eprintln!("tcp relay on {port} -> {upstream}");

    loop {
        let (mut client, _) = listener.accept().await.unwrap();
        client.set_nodelay(true).unwrap();
        let up = upstream.clone();
        tokio::spawn(async move {
            let Ok(mut server) = TcpStream::connect(&up).await else {
                return;
            };
            server.set_nodelay(true).unwrap();
            let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
        });
    }
}
