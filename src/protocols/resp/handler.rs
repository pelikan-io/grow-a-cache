//! RESP protocol connection handler.
//!
//! Handles incoming RESP connections, parses commands, and executes them
//! against the storage backend.

use super::parser::{parse, Frame, ParseResult};
use crate::storage::{Storage, StorageResult};
use bytes::{Bytes, BytesMut};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{trace, warn};

/// Read buffer size
const BUFFER_SIZE: usize = 16 * 1024;

/// RESP command types
#[derive(Debug)]
enum RespCommand {
    Ping { message: Option<Bytes> },
    Get { key: Bytes },
    Set { key: Bytes, value: Bytes, ex: Option<u64>, nx: bool, xx: bool },
    Del { keys: Vec<Bytes> },
    Hello { version: u8 },
    Command,  // Redis COMMAND command (for client compatibility)
}

/// RESP connection handler
struct RespHandler {
    storage: Arc<Storage>,
    resp_version: u8,
}

impl RespHandler {
    fn new(storage: Arc<Storage>) -> Self {
        RespHandler {
            storage,
            resp_version: 2,
        }
    }

    /// Parse a command from a frame
    fn parse_command(&self, frame: Frame) -> Result<RespCommand, String> {
        let frames = match frame {
            Frame::Array(Some(frames)) => frames,
            Frame::Array(None) => return Err("ERR null command".to_string()),
            _ => return Err("ERR expected array".to_string()),
        };

        if frames.is_empty() {
            return Err("ERR empty command".to_string());
        }

        // First element should be the command name
        let cmd_name = match &frames[0] {
            Frame::Bulk(Some(data)) => {
                std::str::from_utf8(data)
                    .map_err(|_| "ERR invalid command name".to_string())?
                    .to_uppercase()
            }
            _ => return Err("ERR expected bulk string for command name".to_string()),
        };

        match cmd_name.as_str() {
            "PING" => {
                let message = if frames.len() > 1 {
                    match &frames[1] {
                        Frame::Bulk(Some(data)) => Some(data.clone()),
                        _ => None,
                    }
                } else {
                    None
                };
                Ok(RespCommand::Ping { message })
            }

            "GET" => {
                if frames.len() < 2 {
                    return Err("ERR wrong number of arguments for 'get' command".to_string());
                }
                let key = match &frames[1] {
                    Frame::Bulk(Some(data)) => data.clone(),
                    _ => return Err("ERR invalid key".to_string()),
                };
                Ok(RespCommand::Get { key })
            }

            "SET" => {
                if frames.len() < 3 {
                    return Err("ERR wrong number of arguments for 'set' command".to_string());
                }
                let key = match &frames[1] {
                    Frame::Bulk(Some(data)) => data.clone(),
                    _ => return Err("ERR invalid key".to_string()),
                };
                let value = match &frames[2] {
                    Frame::Bulk(Some(data)) => data.clone(),
                    _ => return Err("ERR invalid value".to_string()),
                };

                // Parse optional arguments
                let mut ex = None;
                let mut nx = false;
                let mut xx = false;
                let mut i = 3;

                while i < frames.len() {
                    let opt = match &frames[i] {
                        Frame::Bulk(Some(data)) => {
                            std::str::from_utf8(data)
                                .map_err(|_| "ERR invalid option".to_string())?
                                .to_uppercase()
                        }
                        _ => return Err("ERR invalid option".to_string()),
                    };

                    match opt.as_str() {
                        "EX" => {
                            i += 1;
                            if i >= frames.len() {
                                return Err("ERR syntax error".to_string());
                            }
                            let seconds = match &frames[i] {
                                Frame::Bulk(Some(data)) => {
                                    std::str::from_utf8(data)
                                        .map_err(|_| "ERR invalid expire time".to_string())?
                                        .parse::<u64>()
                                        .map_err(|_| "ERR invalid expire time".to_string())?
                                }
                                Frame::Integer(n) => *n as u64,
                                _ => return Err("ERR invalid expire time".to_string()),
                            };
                            ex = Some(seconds);
                        }
                        "PX" => {
                            i += 1;
                            if i >= frames.len() {
                                return Err("ERR syntax error".to_string());
                            }
                            let millis = match &frames[i] {
                                Frame::Bulk(Some(data)) => {
                                    std::str::from_utf8(data)
                                        .map_err(|_| "ERR invalid expire time".to_string())?
                                        .parse::<u64>()
                                        .map_err(|_| "ERR invalid expire time".to_string())?
                                }
                                Frame::Integer(n) => *n as u64,
                                _ => return Err("ERR invalid expire time".to_string()),
                            };
                            // Convert milliseconds to seconds (rounding up)
                            ex = Some((millis + 999) / 1000);
                        }
                        "NX" => nx = true,
                        "XX" => xx = true,
                        _ => return Err(format!("ERR unsupported option: {}", opt)),
                    }
                    i += 1;
                }

                Ok(RespCommand::Set { key, value, ex, nx, xx })
            }

            "DEL" => {
                if frames.len() < 2 {
                    return Err("ERR wrong number of arguments for 'del' command".to_string());
                }
                let mut keys = Vec::with_capacity(frames.len() - 1);
                for frame in &frames[1..] {
                    match frame {
                        Frame::Bulk(Some(data)) => keys.push(data.clone()),
                        _ => return Err("ERR invalid key".to_string()),
                    }
                }
                Ok(RespCommand::Del { keys })
            }

            "HELLO" => {
                let version = if frames.len() > 1 {
                    match &frames[1] {
                        Frame::Bulk(Some(data)) => {
                            std::str::from_utf8(data)
                                .map_err(|_| "ERR invalid protocol version".to_string())?
                                .parse::<u8>()
                                .map_err(|_| "ERR invalid protocol version".to_string())?
                        }
                        Frame::Integer(n) => *n as u8,
                        _ => return Err("ERR invalid protocol version".to_string()),
                    }
                } else {
                    2
                };
                Ok(RespCommand::Hello { version })
            }

            "COMMAND" => Ok(RespCommand::Command),

            _ => Err(format!("ERR unknown command '{}'", cmd_name)),
        }
    }

    /// Execute a command and return the response frame
    fn execute(&mut self, cmd: RespCommand) -> Frame {
        match cmd {
            RespCommand::Ping { message } => {
                match message {
                    Some(msg) => Frame::bulk(msg),
                    None => Frame::simple("PONG"),
                }
            }

            RespCommand::Get { key } => {
                let key_str = match std::str::from_utf8(&key) {
                    Ok(s) => s,
                    Err(_) => return Frame::error("ERR invalid key encoding"),
                };

                match self.storage.get(key_str) {
                    Some(item) => Frame::bulk(Bytes::from(item.value)),
                    None => Frame::null(),
                }
            }

            RespCommand::Set { key, value, ex, nx, xx } => {
                let key_str = match std::str::from_utf8(&key) {
                    Ok(s) => s,
                    Err(_) => return Frame::error("ERR invalid key encoding"),
                };

                let ttl = ex.unwrap_or(0);

                // Handle NX/XX conditions
                let result = if nx && xx {
                    // Both NX and XX is invalid (would never succeed)
                    return Frame::error("ERR XX and NX options at the same time are not compatible");
                } else if nx {
                    // Only set if key doesn't exist
                    self.storage.add(key_str, value.to_vec(), 0, ttl)
                } else if xx {
                    // Only set if key exists
                    self.storage.replace(key_str, value.to_vec(), 0, ttl)
                } else {
                    // Normal set
                    self.storage.set(key_str, value.to_vec(), 0, ttl)
                };

                match result {
                    StorageResult::Stored => Frame::simple("OK"),
                    StorageResult::NotStored => Frame::null(),
                    _ => Frame::null(),
                }
            }

            RespCommand::Del { keys } => {
                let mut count = 0i64;
                for key in keys {
                    let key_str = match std::str::from_utf8(&key) {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    if self.storage.delete(key_str) == StorageResult::Deleted {
                        count += 1;
                    }
                }
                Frame::integer(count)
            }

            RespCommand::Hello { version } => {
                self.resp_version = version.clamp(2, 3);

                // Return server information
                // For RESP3, this would be a map; for RESP2, an array of key-value pairs
                Frame::array(vec![
                    Frame::bulk(Bytes::from_static(b"server")),
                    Frame::bulk(Bytes::from_static(b"grow-a-cache")),
                    Frame::bulk(Bytes::from_static(b"version")),
                    Frame::bulk(Bytes::from_static(b"0.1.0")),
                    Frame::bulk(Bytes::from_static(b"proto")),
                    Frame::integer(self.resp_version as i64),
                ])
            }

            RespCommand::Command => {
                // Return empty array for COMMAND (client compatibility)
                Frame::array(vec![])
            }
        }
    }
}

/// Handle a single RESP client connection
pub async fn handle_connection(
    mut stream: TcpStream,
    storage: Arc<Storage>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut buffer = BytesMut::with_capacity(BUFFER_SIZE);
    let mut handler = RespHandler::new(storage);

    loop {
        // Read more data if needed
        if buffer.is_empty() {
            let n = stream.read_buf(&mut buffer).await?;
            if n == 0 {
                trace!("Connection closed by client");
                return Ok(());
            }
        }

        // Try to parse a frame
        match parse(&buffer) {
            ParseResult::Complete(frame, consumed) => {
                trace!(?frame, "Processing RESP command");

                // Parse and execute command
                let response = match handler.parse_command(frame) {
                    Ok(cmd) => handler.execute(cmd),
                    Err(msg) => Frame::error(msg),
                };

                // Send response
                let response_bytes = response.encode();
                stream.write_all(&response_bytes).await?;

                // Advance buffer
                buffer.advance(consumed);
            }

            ParseResult::Incomplete => {
                // Need more data
                let n = stream.read_buf(&mut buffer).await?;
                if n == 0 {
                    if buffer.is_empty() {
                        trace!("Connection closed by client");
                        return Ok(());
                    } else {
                        warn!("Connection closed with incomplete frame");
                        return Ok(());
                    }
                }
            }

            ParseResult::Error(e) => {
                warn!(error = %e, "RESP parse error");
                let response = Frame::error(format!("ERR {}", e));
                stream.write_all(&response.encode()).await?;
                buffer.clear();
            }
        }
    }
}

/// Extension trait for BytesMut to advance the buffer
trait BytesMutExt {
    fn advance(&mut self, cnt: usize);
}

impl BytesMutExt for BytesMut {
    fn advance(&mut self, cnt: usize) {
        <BytesMut as bytes::Buf>::advance(self, cnt);
    }
}
