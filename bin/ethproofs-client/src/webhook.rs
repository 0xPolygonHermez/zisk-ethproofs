
use log::{error, debug, info};
use std::io::prelude::*;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use zisk_common::save_proof;
use zisk_distributed_common::WebhookPayloadDto;

fn handle_client(mut stream: TcpStream, proving_block_shared: Arc<Mutex<u64>>) {
    let mut buf = Vec::new();

    // Read headers until CRLFCRLF
    let mut tmp = [0u8; 8192];
    let headers_end: usize;
    loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            headers_end = pos + 4;
            break;
        }
        let n = match stream.read(&mut tmp) {
            Ok(n) => n,
            Err(e) => {
                http_response(&mut stream, 400, "Bad Request: error reading headers");
                error!("Bad Request: error reading headers: {e}");
                return;
            }
        };
        if n == 0 {
            http_response(&mut stream, 400, "Bad Request: headers not complete");
            error!("Bad Request: headers not complete");
            return;
        }
        buf.extend_from_slice(&tmp[..n]);
    }

    // Get header as lines
    let (header_bytes, after_headers) = buf.split_at(headers_end);
    let header_text = String::from_utf8_lossy(header_bytes);
    let header_lines = header_text.split("\r\n");

    // Get content length from headers
    let mut content_length: Option<usize> = None;
    for line in header_lines {
        if line.is_empty() { continue; }
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim();
            match key.as_str() {
                "content-length" => content_length = val.parse::<usize>().ok(),
                _ => {}
            }
        }
    }

    let Some(cl) = content_length else {
        http_response(&mut stream, 411, "Length Required: missing Content-Length");
        error!("Length Required: missing Content-Length");
        return;
    };

    // Read exactly Content-Length bytes
    let mut body = after_headers.to_vec();
    while body.len() < cl {
        let mut tmp = [0u8; 8192];
        let n = match stream.read(&mut tmp) {
            Ok(n) => n,
            Err(e) => {
                http_response(&mut stream, 400, "Bad Request: error reading body");
                error!("Bad Request: error reading body: {e}");
                return;
            }
        };
        if n == 0 {
            http_response(&mut stream, 400, "Bad Request: connection closed before body completed");
            error!("Bad Request: connection closed before body completed");
            return;
        }
        body.extend_from_slice(&tmp[..n]);
        if body.len() > cl {
            body.truncate(cl);
            break;
        }
    }

    // Deserialize JSON -> WebhookPayloadDto
    info!("POST received: {} bytes", body.len());
    let payload: WebhookPayloadDto = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            http_response(&mut stream, 400, "Bad Request: invalid JSON body");
            error!("JSON deserialization failed: {e}");
            return;
        }
    };

    let proved_block = *proving_block_shared.lock().unwrap();
    debug!("Block number: {}, job id: {}, success: {}, duration: {}, steps: {}", proved_block, payload.job_id, payload.success, payload.duration_ms, payload.executed_steps.unwrap_or(0));

    match save_proof(payload.job_id.as_str(), PathBuf::from("./output"), &payload.proof.unwrap(), true) {
        Ok(_) => http_response(&mut stream, 200, "OK"),
        Err(e) => {
            error!("Failed to save proof for job {}: {}", payload.job_id, e);
            http_response(&mut stream, 500, "Internal Server Error: failed to save proof");
        }
    }

}

/// Write HTTP response with given status and text message.
fn http_response(stream: &mut TcpStream, status: u16, msg: &str) {
    let body = msg.as_bytes();

    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        405 => "Method Not Allowed",
        411 => "Length Required",
        415 => "Unsupported Media Type",
        501 => "Not Implemented",
        _ => "OK",
    };

    let resp = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status, status_text, body.len()
    );

    if let Err(e) = stream.write_all(resp.as_bytes()) {
        error!("Failed to write HTTP response header: {}", e);
        return;
    };
    if let Err(e) = stream.write_all(body) {
        error!("Failed to write HTTP response body: {}", e);
        return;
    };
    if let Err(e) = stream.flush() {
        error!("Failed to flush HTTP response: {}", e);
    };
}

pub async fn start_webhook_server(proving_block_shared: Arc<Mutex<u64>>) {
    let listener = TcpListener::bind("0.0.0.0:8051").unwrap();
    info!("Webhook server running at http://127.0.0.1:8051/");

    // Accept incoming connections
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                handle_client(stream, Arc::clone(&proving_block_shared));
            }
            Err(e) => {
                error!("Connection failed: {}", e);
            }
        }
    }
}
