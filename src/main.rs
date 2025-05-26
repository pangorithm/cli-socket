use std::{collections::HashMap, process::Stdio, sync::Arc};

use clap::Parser;
use futures_util::{
    SinkExt, StreamExt,
    stream::{SplitSink, SplitStream},
};
use log::{error, info, trace};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    process::{ChildStdin, Command as TokioCommand},
    sync::{Mutex, mpsc::UnboundedReceiver},
};
use tokio_tungstenite::{
    WebSocketStream, accept_hdr_async,
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
        default_value = "127.0.0.1:9000",
        help = "WebSocket address"
    )]
    ws_addr: String,
    #[arg(
        short,
        long,
        default_value = "bash",
        help = "Path to the child process"
    )]
    child_process: String,
    #[arg(
        short,
        long,
        default_value = ".",
        help = "Working directory for child process"
    )]
    work_dir: String,
    #[arg(
        short,
        long,
        default_value = "authorization",
        help = "authorization header"
    )]
    authorization: String,
    #[arg(
        short,
        long,
        default_value = "info",
        help = "print log level (trace, debug, info, warn, error)"
    )]
    log_level: String,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let log_level = match args.log_level.to_lowercase().as_str() {
        "trace" => log::LevelFilter::Trace,
        "debug" => log::LevelFilter::Debug,
        "info" => log::LevelFilter::Info,
        "warn" => log::LevelFilter::Warn,
        "error" => log::LevelFilter::Error,
        _ => log::LevelFilter::Info, // 기본값
    };
    env_logger::Builder::new()
        .filter_module("cli_socket", log_level)
        .format(|buf, record| {
            use std::io::Write;
            writeln!(
                buf,
                "[{} {}] {}",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                record.level(),
                record.args()
            )
        })
        .init();
    let ws_addr = args.ws_addr.clone();

    if let Ok(current_dir) = std::env::current_dir() {
        info!("Working directory: {}", current_dir.display());
        info!("Working directory for child process: {}", &args.work_dir);
    } else {
        error!("Failed to get current working directory");
    }
    let mut child = TokioCommand::new(&args.child_process)
        .current_dir(&args.work_dir)
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
            trace!("stdout: {}", &msg);
            let clients = clients_clone.lock().await;
            for (id, (role, tx)) in clients.iter() {
                if role == "listen" {
                    // role이 "listen"인 클라이언트에게만 메시지 전송
                    if tx.send(msg.clone()).is_err() {
                        error!("Failed to send to listener {id}");
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
            if let Err(e) = handle_ws_connection(stream, authorization, clients, child_stdin).await
            {
                error!("Connection error: {}", e);
            }
        });
    }
}

// Role 추출 함수 분리
fn extract_role(req: &Request) -> String {
    req.uri()
        .query()
        .and_then(|q| {
            q.split('&')
                .find(|pair| pair.starts_with("role="))
                .and_then(|pair| pair.split('=').nth(1))
        })
        .unwrap_or("user")
        .to_string()
}

// 인증 체크 함수 분리
fn check_authorization(req: &Request, expected: &str) -> bool {
    req.headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("authorization")
        .eq(expected)
}

async fn handle_ws_connection(
    stream: TcpStream,
    authorization: String,
    clients: Clients,
    child_stdin: Arc<Mutex<ChildStdin>>,
) -> Result<(), Box<dyn std::error::Error>> {
    // role을 저장할 String
    let role = Arc::new(tokio::sync::Mutex::new(String::new()));
    let role_clone = role.clone();

    let callback = |req: &Request, response: Response| {
        if !check_authorization(req, &authorization) {
            return Err(Response::builder()
                .status(401)
                .body(Some("Unauthorized".to_string()))
                .unwrap());
        }
        let extracted_role = extract_role(req);
        // Use std::sync::Mutex instead for synchronous access
        if let Ok(mut role) = role_clone.try_lock() {
            *role = extracted_role;
        }
        Ok(response)
    };

    let ws_stream = accept_hdr_async(stream, callback).await?;
    let extracted_role = role.lock().await.clone();
    info!("Client connected with role: {}", extracted_role);
    handle_connection(ws_stream, clients, child_stdin, extracted_role).await
}

// handle_connection 함수 분리
async fn handle_stdin_messages(
    mut read: SplitStream<WebSocketStream<TcpStream>>,
    child_stdin: Arc<Mutex<ChildStdin>>,
) {
    while let Some(Ok(msg)) = read.next().await {
        if let Message::Text(text) = msg {
            let mut stdin = child_stdin.lock().await;
            trace!("stdin: {}", &text);
            if let Err(e) = stdin.write_all(format!("{text}\n").as_bytes()).await {
                error!("Failed to write to stdin: {e:?}");
            }
        }
    }
}

async fn handle_stdout_messages(
    mut write: SplitSink<WebSocketStream<TcpStream>, Message>,
    mut rx: UnboundedReceiver<Message>,
) {
    while let Some(msg) = rx.recv().await {
        if write.send(msg).await.is_err() {
            break;
        }
    }
}

async fn handle_connection(
    ws_stream: WebSocketStream<TcpStream>,
    clients: Clients,
    child_stdin: Arc<Mutex<ChildStdin>>,
    role: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let (write, read) = ws_stream.split();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let id = Uuid::new_v4().to_string();

    clients.lock().await.insert(id.clone(), (role, tx));

    let stdin_writer = tokio::spawn(handle_stdin_messages(read, child_stdin));
    let stdout_reader = tokio::spawn(handle_stdout_messages(write, rx));

    let _ = tokio::try_join!(stdin_writer, stdout_reader);
    clients.lock().await.remove(&id);
    Ok(())
}
