use dotenvy::dotenv;
use log::{error, info, warn};
use rand::rng;
use rand::seq::IndexedRandom;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::functions_service::{FunctionService, FunctionSupabaseService};

mod env;
mod functions_service;
mod logger;

#[derive(Clone, Debug)]
struct Worker {
    host: String,
    port: u16,
}

type WorkerMap = Arc<RwLock<Vec<Worker>>>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenv();
    logger::build_logger().init();

    // Initialize worker map with default worker
    let workers_hosts = crate::env::WORKER_HOSTS.split(",");
    let workers: WorkerMap = Arc::new(RwLock::new(Vec::new()));
    {
        let mut w = workers.write().await;
        for whost in workers_hosts {
            w.push(Worker {
                host: whost.to_string(),
                port: 6969,
            });
        }
    }

    info!("Connecting to Supabase service...");
    let functions_service = Arc::new(FunctionSupabaseService::from_env());
    functions_service.check_connection().await?;
    info!("Connected!");

    // Start the gateway server
    info!("Starting TCP server...");
    let listener = TcpListener::bind("0.0.0.0:8000").await?;
    info!("Gateway server listening on 0.0.0.0:8000");
    info!("Workers: {:?}", workers);

    loop {
        let (stream, addr) = listener.accept().await?;
        let workers_clone = Arc::clone(&workers);

        let functions_service = functions_service.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, addr, workers_clone, functions_service).await
            {
                error!("Error handling connection from {}: {}", addr, e);
            }
        });
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    addr: SocketAddr,
    workers: WorkerMap,
    functions_service: Arc<FunctionSupabaseService>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use std::time::Instant;
    info!("New connection from {}", addr);
    let start_time = Instant::now();

    // Read HTTP request (headers + body if present)
    let mut buffer = vec![0; 4096];
    let mut request_data = Vec::new();
    let mut total_read = 0;
    let mut headers_end = None;
    loop {
        let n = stream.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        request_data.extend_from_slice(&buffer[..n]);
        total_read += n;
        if let Some(pos) = twoway::find_bytes(&request_data, b"\r\n\r\n") {
            headers_end = Some(pos + 4);
            break;
        }
        if total_read > 1024 * 1024 {
            // 1MB max header size
            return Err("Request headers too large".into());
        }
    }
    if headers_end.is_none() {
        warn!("Did not receive full HTTP headers from {}", addr);
        return Err("Incomplete HTTP headers".into());
    }
    let headers_end = headers_end.unwrap();
    let request_str = String::from_utf8_lossy(&request_data[..headers_end]);
    let (function_id, path) = parse_http_request(&request_str)?;
    info!("Parsed function_id: {}, path: {}", function_id, path);

    // Parse Content-Length if present
    let content_length = request_str
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);

    let _body_len = request_data.len() - headers_end;
    let mut body = request_data[headers_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buffer[..n]);
    }
    let total_request_size = headers_end + body.len();
    info!(
        "Received HTTP request from {} ({} bytes)",
        addr, total_request_size
    );

    // functions_service.
    let function_uuid = match Uuid::from_str(&function_id) {
        Ok(uuid) => uuid,
        Err(e) => {
            error!("Invalid function_id={function_id}: {e}");
            let response =
                "HTTP/1.1 400 Bad Request\r\nContent-Length: 19\r\n\r\nInvalid function_id";
            stream.write_all(response.as_bytes()).await?;
            return Ok(());
        }
    };
    info!("Function UUID: {}", function_uuid);

    let last_dpl_time = match functions_service.get_last_depl_time(&function_uuid).await {
        Ok(deployment_time) => deployment_time,
        Err(e) => {
            error!(
                "Failed to get last deployment time for function_id {}: {}",
                function_id, e
            );
            let response = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 19\r\n\r\nService Unavailable";
            stream.write_all(response.as_bytes()).await?;
            return Ok(());
        }
    };
    info!(
        "Last deployment time for function_id {}: {:?}",
        function_id, last_dpl_time
    );

    if last_dpl_time.is_none() {
        warn!("No deployments found for function_id={}", function_id);
        let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 18\r\n\r\nFunction not found";
        stream.write_all(response.as_bytes()).await?;
        return Ok(());
    }
    let last_dpl_time = last_dpl_time.unwrap();

    // Get worker for this function (using default for now)
    let worker = {
        let workers_read = workers.read().await;
        let worker = workers_read.choose(&mut rng());
        if worker.is_none() {
            error!("No workers available");
            let response = "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 20\r\n\r\nNo workers available";
            stream.write_all(response.as_bytes()).await?;
            return Ok(());
        }
        worker.unwrap().clone()
    };

    let worker_addr = format!("{}:{}", worker.host, worker.port);
    let mut worker_stream = TcpStream::connect(&worker_addr)
        .await
        .map_err(|e| format!("Failed to connect to worker at {}: {}", worker_addr, e))?;
    info!("Connected to worker at {}", worker_addr);

    // Perform handshake
    let handshake_result =
        perform_handshake(&mut worker_stream, &function_id, last_dpl_time).await?;
    if handshake_result != 0 {
        error!("Worker handshake failed with code: {}", handshake_result);
        let response =
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 18\r\n\r\nWorker unavailable";
        stream.write_all(response.as_bytes()).await?;
        return Ok(());
    }
    info!("Handshake successful, proxying HTTP request");

    // Forward HTTP request to worker
    let mut total_sent = 0;
    worker_stream
        .write_all(&request_data[..headers_end])
        .await?;
    total_sent += headers_end;
    if !body.is_empty() {
        worker_stream.write_all(&body).await?;
        total_sent += body.len();
    }
    info!("Forwarded {} bytes to worker {}", total_sent, worker_addr);

    // Read response from worker and send to client
    let mut response_buffer = vec![0; 4096];
    let mut total_response = 0;
    loop {
        let n = worker_stream.read(&mut response_buffer).await?;
        if n == 0 {
            break;
        }
        stream.write_all(&response_buffer[..n]).await?;
        total_response += n;
    }
    info!(
        "Relayed {} bytes from worker to client {}",
        total_response, addr
    );
    let elapsed = start_time.elapsed();
    info!("Connection from {} closed (duration: {:?})", addr, elapsed);
    Ok(())
}

fn parse_http_request(
    request: &str,
) -> Result<(String, String), Box<dyn std::error::Error + Send + Sync>> {
    let lines: Vec<&str> = request.lines().collect();
    if lines.is_empty() {
        return Err("Empty request".into());
    }

    let first_line = lines[0];
    let parts: Vec<&str> = first_line.split_whitespace().collect();
    if parts.len() < 2 {
        return Err("Invalid HTTP request line".into());
    }

    let url = parts[1];

    // Parse URL: /:function_id/:path
    let url_parts: Vec<&str> = url.trim_start_matches('/').splitn(2, '/').collect();

    if url_parts.is_empty() {
        return Err("No function_id in URL".into());
    }

    let function_id = url_parts[0].to_string();
    let path = if url_parts.len() > 1 {
        format!("/{}", url_parts[1])
    } else {
        "/".to_string()
    };

    Ok((function_id, path))
}

async fn perform_handshake(
    worker_stream: &mut TcpStream,
    function_id: &str,
    deployment_timestamp: u64,
) -> Result<u8, Box<dyn std::error::Error + Send + Sync>> {
    // Convert function_id to UUID (dummy implementation)
    let function_uuid = convert_function_id_to_uuid(function_id)?;

    // Prepare handshake message
    let mut handshake = Vec::new();

    // 4 bits version (1) - we'll use a full byte and mask
    let version: u8 = 1;
    handshake.push(version);

    // 128 bits (16 bytes) UUID
    handshake.extend_from_slice(function_uuid.as_bytes());

    // 64 bits (8 bytes) timestamp
    handshake.extend_from_slice(&deployment_timestamp.to_be_bytes());

    // Send handshake
    worker_stream.write_all(&handshake).await?;
    info!(
        "Sent handshake: version={}, uuid={}, timestamp={}",
        version, function_uuid, deployment_timestamp
    );

    // Read response (expecting 1 byte)
    let mut response = [0u8; 1];
    worker_stream.read_exact(&mut response).await?;

    Ok(response[0])
}

fn convert_function_id_to_uuid(
    function_id: &str,
) -> Result<Uuid, Box<dyn std::error::Error + Send + Sync>> {
    // Try to parse as UUID first
    if let Ok(uuid) = Uuid::parse_str(function_id) {
        return Ok(uuid);
    }

    // If not a valid UUID, create a deterministic UUID from the function_id
    // Using a simple hash-based approach
    let mut bytes = [0u8; 16];
    let id_bytes = function_id.as_bytes();

    for (i, &byte) in id_bytes.iter().enumerate() {
        if i >= 16 {
            break;
        }
        bytes[i] = byte;
    }

    // Fill remaining bytes with a pattern if function_id is shorter than 16 bytes
    for i in id_bytes.len()..16 {
        bytes[i] = (i as u8).wrapping_mul(17); // Simple pattern
    }

    Ok(Uuid::from_bytes(bytes))
}
