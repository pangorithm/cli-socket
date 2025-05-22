use std::{collections::HashMap, process::Stdio, sync::Arc};

use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use log::info;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    process::{ChildStdin, Command as TokioCommand},
    sync::Mutex,
};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        Message,
        handshake::server::{Request, Response},
    },
};
use uuid::Uuid;

type Clients = Arc<Mutex<HashMap<String, (String, tokio::sync::mpsc::UnboundedSender<Message>)>>>;

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
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
    #[arg(
        short,
        long,
        default_value = "authorization",
        help = "authorization header"
    )]
    authorization: String,
}

#[tokio::main]
async fn main() {
    let _ = env_logger::try_init();
    let args = Args::parse();
    let ws_addr = args.ws_addr.clone();

    let mut child = TokioCommand::new(&args.child_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("Failed to spawn child process");

    let child_stdin = child.stdin.take().expect("No stdin");
    let child_stdout = child.stdout.take().expect("No stdout");
    let clients: Clients = Arc::new(Mutex::new(HashMap::new()));
    let child_stdin = Arc::new(Mutex::new(child_stdin));

    // child stdout 읽어서 특정 role에게만 전송
    let clients_clone = clients.clone();
    tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(child_stdout).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            let msg = Message::Text(line.into());
            let clients = clients_clone.lock().await;
            for (id, (role, tx)) in clients.iter() {
                if role == "listen" {
                    // role이 "listen"인 클라이언트에게만 메시지 전송
                    if tx.send(msg.clone()).is_err() {
                        eprintln!("Failed to send to listener {id}");
                    }
                }
            }
        }
    });

    let try_socket = TcpListener::bind(&ws_addr).await;
    let listener = try_socket.expect("Failed to bind");
    info!("Listening on: {}", ws_addr);

    loop {
        let (stream, _) = listener.accept().await.expect("Failed to accept");
        let clients = clients.clone();
        let child_stdin = child_stdin.clone();
        let authorization = args.authorization.clone();

        tokio::spawn(async move {
            let role_from_query = Arc::new(Mutex::new(None)); // 초기값 없음
            let role_capture = role_from_query.clone();

            let callback = move |req: &Request, response: Response| {
                // 인증
                let token = req
                    .headers()
                    .get("Authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("authorization");

                if token != authorization.as_str() {
                    let response = Response::builder()
                        .status(401)
                        .body(Some("Unauthorized".into()))
                        .unwrap();
                    return Err(response);
                }

                // 쿼리에서 role 추출
                if let Some(query) = req.uri().query() {
                    for pair in query.split('&') {
                        let mut kv = pair.splitn(2, '=');
                        if let (Some(k), Some(v)) = (kv.next(), kv.next()) {
                            if k == "role" {
                                let mut lock = role_capture.blocking_lock(); // 동기 잠금 (콜백이 sync 함수이므로)
                                *lock = Some(v.to_string());
                            }
                        }
                    }
                }

                Ok(response)
            };

            match accept_hdr_async(stream, callback).await {
                Ok(ws_stream) => {
                    println!("Client connected!");
                    // role_from_query를 클로저 밖에서 사용하려면 move로 복사
                    let role = role_from_query
                        .lock()
                        .await
                        .clone()
                        .unwrap_or("user".to_string());
                    if let Err(e) = handle_connection(ws_stream, clients, child_stdin, role).await {
                        eprintln!("Connection error: {e:?}");
                    }
                }
                Err(e) => {
                    eprintln!("Connection rejected: {:?}", e);
                }
            }
        });
    }
}

async fn handle_connection(
    ws_stream: tokio_tungstenite::WebSocketStream<TcpStream>,
    clients: Clients,
    child_stdin: Arc<Mutex<ChildStdin>>,
    role: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let (mut write, mut read) = ws_stream.split();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
    let id = Uuid::new_v4().to_string();

    // role과 함께 저장
    clients.lock().await.insert(id.clone(), (role.clone(), tx));

    // 클라이언트 -> child stdin
    let stdin_writer = {
        let child_stdin = child_stdin.clone();
        tokio::spawn(async move {
            while let Some(Ok(msg)) = read.next().await {
                if let Message::Text(text) = msg {
                    let mut stdin = child_stdin.lock().await;
                    if let Err(e) = stdin.write_all(format!("{text}\n").as_bytes()).await {
                        eprintln!("Failed to write to stdin: {e:?}");
                    }
                }
            }
        })
    };

    // child stdout -> 해당 클라이언트
    let stdout_reader = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if write.send(msg).await.is_err() {
                break;
            }
        }
    });

    let _ = tokio::try_join!(stdin_writer, stdout_reader);

    clients.lock().await.remove(&id);
    Ok(())
}
