use dotenvy::dotenv;
use log::{error, info, warn};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;
use uuid::Uuid;

mod env;
mod logger;

#[derive(Clone, Debug)]
struct Worker {
    host: String,
    port: u16,
}

type WorkerMap = Arc<RwLock<HashMap<String, Worker>>>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenv();
    logger::build_logger().init();

    // Initialize worker map with default worker
    let workers: WorkerMap = Arc::new(RwLock::new(HashMap::new()));
    {
        let mut w = workers.write().await;
        w.insert(
            "default".to_string(),
            Worker {
                host: "0.0.0.0".to_string(),
                port: 6969,
            },
        );
    }

    // Start the gateway server
    let listener = TcpListener::bind("0.0.0.0:8000").await?;
    info!("Gateway server listening on 0.0.0.0:8000");

    loop {
        let (stream, addr) = listener.accept().await?;
        let workers_clone = Arc::clone(&workers);

        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, addr, workers_clone).await {
                error!("Error handling connection from {}: {}", addr, e);
            }
        });
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    addr: SocketAddr,
    workers: WorkerMap,
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
    let body_len = request_data.len() - headers_end;
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

    // Get worker for this function (using default for now)
    let worker = {
        let workers_read = workers.read().await;
        workers_read
            .get("default")
            .cloned()
            .ok_or("No worker available")?
    };
    let worker_addr = format!("{}:{}", worker.host, worker.port);
    let mut worker_stream = TcpStream::connect(&worker_addr)
        .await
        .map_err(|e| format!("Failed to connect to worker at {}: {}", worker_addr, e))?;
    info!("Connected to worker at {}", worker_addr);

    // Perform handshake
    let handshake_result = perform_handshake(&mut worker_stream, &function_id).await?;
    if handshake_result != 0 {
        error!("Worker handshake failed with code: {}", handshake_result);
        let response =
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 19\r\n\r\nWorker unavailable";
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
) -> Result<u8, Box<dyn std::error::Error + Send + Sync>> {
    // Convert function_id to UUID (dummy implementation)
    let function_uuid = convert_function_id_to_uuid(function_id)?;

    // Get deployment timestamp (dummy value: 1970-01-01)
    let deployment_timestamp = get_function_deployment_timestamp(&function_uuid);

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

fn get_function_deployment_timestamp(_function_uuid: &Uuid) -> u64 {
    // Dummy implementation: always return Unix timestamp for 1970-01-01 00:00:00 UTC
    0
}
