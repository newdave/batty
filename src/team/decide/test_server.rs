//! Tiny blocking HTTP server for tests. Serves one canned response on a
//! loopback port so no test ever reaches the real Gateway.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Serve one canned HTTP response. Returns the endpoint URL and a channel
/// that yields the raw request the server received.
pub(crate) fn serve_once(
    status_line: &'static str,
    body: String,
    delay: Duration,
) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/v1/decisions", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut received = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = stream.read(&mut buf).unwrap_or(0);
            if n == 0 {
                break;
            }
            received.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&received);
            if let Some(split) = text.find("\r\n\r\n") {
                let content_length = text[..split]
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if received.len() >= split + 4 + content_length {
                    break;
                }
            }
        }
        let _ = tx.send(String::from_utf8_lossy(&received).into_owned());
        thread::sleep(delay);
        let reply = format!(
            "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(reply.as_bytes());
    });
    (endpoint, rx, handle)
}
