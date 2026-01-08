//! Echo protocol handler for the Tokio runtime.

use bytes::BytesMut;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::storage::Storage;

const MAX_ECHO_SIZE: usize = 16 * 1024 * 1024; // 16MB max echo size

/// Handle an echo protocol connection.
///
/// Protocol: length-prefixed binary data
/// - Read: `<length>\r\n<data>`
/// - Write: `<length>\r\n<data>`
///
/// No storage interaction - purely for I/O throughput testing.
pub async fn handle_connection(
    stream: TcpStream,
    _storage: Arc<Storage>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::with_capacity(32);
    let mut buffer = BytesMut::with_capacity(4096);

    loop {
        line.clear();

        // Read the length line
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            // EOF
            break;
        }

        let trimmed = line.trim();

        // Check for QUIT command
        if trimmed.eq_ignore_ascii_case("QUIT") {
            writer.write_all(b"OK\r\n").await?;
            break;
        }

        // Parse length
        let length: usize = match trimmed.parse() {
            Ok(len) if len <= MAX_ECHO_SIZE => len,
            Ok(_) => {
                writer.write_all(b"ERROR payload too large\r\n").await?;
                continue;
            }
            Err(_) => {
                writer.write_all(b"ERROR invalid length\r\n").await?;
                continue;
            }
        };

        // Read exactly `length` bytes of data
        buffer.clear();
        buffer.reserve(length);

        // Read in chunks until we have all the data
        let mut remaining = length;
        while remaining > 0 {
            let chunk_size = remaining.min(8192);
            let start = buffer.len();
            buffer.resize(start + chunk_size, 0);

            let n = reader.read(&mut buffer[start..start + chunk_size]).await?;
            if n == 0 {
                // Unexpected EOF
                return Err("unexpected EOF while reading payload".into());
            }

            // Adjust buffer to actual bytes read
            buffer.truncate(start + n);
            remaining -= n;
        }

        // Echo back: length + data
        let response_header = format!("{length}\r\n");
        writer.write_all(response_header.as_bytes()).await?;
        writer.write_all(&buffer[..length]).await?;
    }

    Ok(())
}
