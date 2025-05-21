// https://github.com/snapview/tokio-tungstenite/blob/master/examples/echo-server.rs

//! A simple echo server.
//!
//! You can test this out by running:
//!
//!     cargo run --example echo-server 127.0.0.1:12345
//!
//! And then in another window run:
//!
//!     cargo run --example client ws://127.0.0.1:12345/
//!
//! Type a message into the client window, press enter to send it and
//! see it echoed back.

use std::io::Error;

use clap::Parser;
use futures_util::{StreamExt, TryStreamExt, future};
use log::info;
use tokio::net::{TcpListener, TcpStream};

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    // cargo run -- --ws-addr 127.0.0.1:8080 --child-path /bin/bash
    // ./target/release/my_app --ws-addr 127.0.0.1:8080 --child-path /bin/bash
    #[arg(
        short,
        long,
        default_value = "127.0.0.1:8080",
        help = "WebSocket address"
    )]
    ws_addr: String,
    #[arg(
        short,
        long,
        default_value = "bash",
        help = "Path to the child process"
    )]
    child_path: String,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let _ = env_logger::try_init();
    let args = Args::parse();
    let ws_addr = args.ws_addr;

    // Create the event loop and TCP listener we'll accept connections on.
    let try_socket = TcpListener::bind(&ws_addr).await;
    let listener = try_socket.expect("Failed to bind");
    info!("Listening on: {}", ws_addr);

    while let Ok((stream, _)) = listener.accept().await {
        tokio::spawn(accept_connection(stream));
    }

    Ok(())
}

async fn accept_connection(stream: TcpStream) {
    let addr = stream
        .peer_addr()
        .expect("connected streams should have a peer address");
    info!("Peer address: {}", addr);

    let ws_stream = tokio_tungstenite::accept_async(stream)
        .await
        .expect("Error during the websocket handshake occurred");

    info!("New WebSocket connection: {}", addr);

    let (write, read) = ws_stream.split();
    // We should not forward messages other than text or binary.
    read.try_filter(|msg| future::ready(msg.is_text() || msg.is_binary()))
        .forward(write)
        .await
        .expect("Failed to forward messages")
}
