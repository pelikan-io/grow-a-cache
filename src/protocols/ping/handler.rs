//! Ping protocol handler for the Tokio runtime.

use bytes::BytesMut;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::storage::Storage;

const MAX_LINE_LENGTH: usize = 1024;

/// Handle a ping protocol connection.
///
/// This is deliberately simple: read lines, respond to PING with PONG.
/// No storage interaction - purely for health checks and latency testing.
pub async fn handle_connection(
    stream: TcpStream,
    _storage: Arc<Storage>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::with_capacity(MAX_LINE_LENGTH);

    loop {
        line.clear();

        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            // EOF
            break;
        }

        // Trim the line ending
        let trimmed = line.trim_end();

        // Parse command
        let response = if trimmed.eq_ignore_ascii_case("PING") {
            BytesMut::from("PONG\r\n")
        } else if let Some(message) = trimmed
            .strip_prefix("PING ")
            .or_else(|| trimmed.strip_prefix("ping "))
        {
            let mut resp = BytesMut::with_capacity(6 + message.len());
            resp.extend_from_slice(b"PONG ");
            resp.extend_from_slice(message.as_bytes());
            resp.extend_from_slice(b"\r\n");
            resp
        } else if trimmed.eq_ignore_ascii_case("QUIT") {
            writer.write_all(b"OK\r\n").await?;
            break;
        } else {
            BytesMut::from("ERROR unknown command\r\n")
        };

        writer.write_all(&response).await?;
    }

    Ok(())
}
