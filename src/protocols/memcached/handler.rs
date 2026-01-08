//! Memcached protocol connection handler.
//!
//! Handles incoming connections, parses commands, and executes them
//! against the storage backend.

use super::parser::{Command, ParseResult, Parser, Response};
use crate::storage::{Storage, StorageResult};
use bytes::BytesMut;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{trace, warn};

/// Read buffer size
const BUFFER_SIZE: usize = 16 * 1024;

/// Handle a single client connection
pub async fn handle_connection(
    mut stream: TcpStream,
    storage: Arc<Storage>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut buffer = BytesMut::with_capacity(BUFFER_SIZE);

    loop {
        // Read more data if buffer is empty or we need more
        if buffer.is_empty() || !buffer.ends_with(b"\r\n") {
            let n = stream.read_buf(&mut buffer).await?;
            if n == 0 {
                // Connection closed
                trace!("Connection closed by client");
                return Ok(());
            }
        }

        // Try to parse a command
        match Parser::parse(&buffer) {
            ParseResult::Complete(command, bytes_consumed) => {
                trace!(?command, "Processing command");

                // Handle quit command specially
                if matches!(command, Command::Quit) {
                    buffer.advance(bytes_consumed);
                    return Ok(());
                }

                // Execute the command
                let response = execute_command(&command, &storage).await;

                // Send response
                if let Some(response_data) = response {
                    stream.write_all(&response_data).await?;
                }

                buffer.advance(bytes_consumed);
            }

            ParseResult::NeedData {
                command_bytes,
                data_bytes,
            } => {
                // We need the data block
                let total_needed = command_bytes + data_bytes + 2; // +2 for trailing \r\n

                // Read more data if needed
                while buffer.len() < total_needed {
                    let n = stream.read_buf(&mut buffer).await?;
                    if n == 0 {
                        return Ok(());
                    }
                }

                // Now parse the complete command with data
                match Parser::parse_with_data(&buffer) {
                    ParseResult::Complete(command, total_bytes) => {
                        trace!(?command, "Processing storage command");

                        // Extract data from buffer
                        let data = buffer[command_bytes..command_bytes + data_bytes].to_vec();

                        // Execute storage command
                        let response = execute_storage_command(&command, &storage, data).await;

                        // Send response
                        if let Some(response_data) = response {
                            stream.write_all(&response_data).await?;
                        }

                        buffer.advance(total_bytes);
                    }
                    ParseResult::Error(e) => {
                        warn!(error = %e, "Parse error");
                        let response = Response::client_error(&e.to_string());
                        stream.write_all(&response).await?;

                        // Try to recover by finding next command
                        if let Some(pos) = find_recovery_point(&buffer) {
                            buffer.advance(pos);
                        } else {
                            buffer.clear();
                        }
                    }
                    _ => unreachable!(),
                }
            }

            ParseResult::Error(super::parser::ParseError::Incomplete) => {
                // Need more data, continue reading
                let n = stream.read_buf(&mut buffer).await?;
                if n == 0 {
                    return Ok(());
                }
            }

            ParseResult::Error(e) => {
                warn!(error = %e, "Parse error");
                let response = Response::client_error(&e.to_string());
                stream.write_all(&response).await?;

                // Try to recover by finding next command
                if let Some(pos) = find_recovery_point(&buffer) {
                    buffer.advance(pos);
                } else {
                    buffer.clear();
                }
            }
        }
    }
}

/// Find a recovery point after a parse error (next \r\n)
fn find_recovery_point(buffer: &[u8]) -> Option<usize> {
    for i in 0..buffer.len().saturating_sub(1) {
        if buffer[i] == b'\r' && buffer[i + 1] == b'\n' {
            return Some(i + 2);
        }
    }
    None
}

/// Execute a non-storage command
async fn execute_command(command: &Command, storage: &Arc<Storage>) -> Option<BytesMut> {
    match command {
        Command::Get { keys } => {
            let keys_ref: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
            let items = storage.get_multi(&keys_ref);

            let mut response = BytesMut::new();
            for (key, item) in items {
                let value_response = Response::value(&key, item.flags, &item.value, None);
                response.extend_from_slice(&value_response);
            }
            response.extend_from_slice(Response::end());
            Some(response)
        }

        Command::Gets { keys } => {
            let keys_ref: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
            let items = storage.get_multi(&keys_ref);

            let mut response = BytesMut::new();
            for (key, item) in items {
                let value_response =
                    Response::value(&key, item.flags, &item.value, Some(item.cas_unique));
                response.extend_from_slice(&value_response);
            }
            response.extend_from_slice(Response::end());
            Some(response)
        }

        Command::Delete { key, noreply } => {
            let result = storage.delete(key);
            if *noreply {
                None
            } else {
                Some(BytesMut::from(match result {
                    StorageResult::Deleted => Response::deleted(),
                    _ => Response::not_found(),
                }))
            }
        }

        Command::Incr {
            key,
            value,
            noreply,
        } => {
            let result = handle_incr_decr(storage, key, *value, true);
            if *noreply {
                None
            } else {
                Some(result)
            }
        }

        Command::Decr {
            key,
            value,
            noreply,
        } => {
            let result = handle_incr_decr(storage, key, *value, false);
            if *noreply {
                None
            } else {
                Some(result)
            }
        }

        Command::FlushAll { delay, noreply } => {
            if *delay > 0 {
                let storage_clone = Arc::clone(storage);
                let delay_secs = *delay;
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(delay_secs)).await;
                    storage_clone.flush_all();
                });
            } else {
                storage.flush_all();
            }

            if *noreply {
                None
            } else {
                Some(BytesMut::from(Response::ok()))
            }
        }

        Command::Stats => {
            let stats = storage.stats();
            let mut response = BytesMut::new();

            response
                .extend_from_slice(&Response::stat("curr_items", &stats.item_count.to_string()));
            response.extend_from_slice(&Response::stat("bytes", &stats.memory_used.to_string()));
            response.extend_from_slice(&Response::stat(
                "limit_maxbytes",
                &stats.max_memory.to_string(),
            ));
            response.extend_from_slice(&Response::stat("cas_badval", "0"));
            response.extend_from_slice(&Response::stat("cas_hits", "0"));
            response.extend_from_slice(&Response::stat("cas_misses", "0"));
            response.extend_from_slice(Response::end());
            Some(response)
        }

        Command::Version => Some(BytesMut::from(Response::version())),

        Command::Quit => None, // Handled in connection loop

        // Storage commands should not reach here
        _ => Some(BytesMut::from(Response::error())),
    }
}

/// Execute a storage command (set, add, replace, append, prepend, cas)
async fn execute_storage_command(
    command: &Command,
    storage: &Arc<Storage>,
    data: Vec<u8>,
) -> Option<BytesMut> {
    match command {
        Command::Set {
            key,
            flags,
            exptime,
            noreply,
            ..
        } => {
            let result = storage.set(key, data, *flags, *exptime);
            if *noreply {
                None
            } else {
                Some(BytesMut::from(match result {
                    StorageResult::Stored => Response::stored(),
                    _ => Response::not_stored(),
                }))
            }
        }

        Command::Add {
            key,
            flags,
            exptime,
            noreply,
            ..
        } => {
            let result = storage.add(key, data, *flags, *exptime);
            if *noreply {
                None
            } else {
                Some(BytesMut::from(match result {
                    StorageResult::Stored => Response::stored(),
                    _ => Response::not_stored(),
                }))
            }
        }

        Command::Replace {
            key,
            flags,
            exptime,
            noreply,
            ..
        } => {
            let result = storage.replace(key, data, *flags, *exptime);
            if *noreply {
                None
            } else {
                Some(BytesMut::from(match result {
                    StorageResult::Stored => Response::stored(),
                    _ => Response::not_stored(),
                }))
            }
        }

        Command::Append { key, noreply, .. } => {
            let result = storage.append(key, &data);
            if *noreply {
                None
            } else {
                Some(BytesMut::from(match result {
                    StorageResult::Stored => Response::stored(),
                    _ => Response::not_stored(),
                }))
            }
        }

        Command::Prepend { key, noreply, .. } => {
            let result = storage.prepend(key, &data);
            if *noreply {
                None
            } else {
                Some(BytesMut::from(match result {
                    StorageResult::Stored => Response::stored(),
                    _ => Response::not_stored(),
                }))
            }
        }

        Command::Cas {
            key,
            flags,
            exptime,
            cas_unique,
            noreply,
            ..
        } => {
            let result = storage.cas(key, data, *flags, *exptime, *cas_unique);
            if *noreply {
                None
            } else {
                Some(BytesMut::from(match result {
                    StorageResult::Stored => Response::stored(),
                    StorageResult::CasMismatch => Response::exists(),
                    StorageResult::NotFound => Response::not_found(),
                    _ => Response::not_stored(),
                }))
            }
        }

        _ => Some(BytesMut::from(Response::error())),
    }
}

/// Handle incr/decr commands
fn handle_incr_decr(storage: &Arc<Storage>, key: &str, delta: u64, is_incr: bool) -> BytesMut {
    // Get current value
    match storage.get(key) {
        None => BytesMut::from(Response::not_found()),
        Some(item) => {
            // Parse current value as number
            let current_str = match std::str::from_utf8(&item.value) {
                Ok(s) => s.trim(),
                Err(_) => {
                    return Response::client_error(
                        "cannot increment or decrement non-numeric value",
                    )
                }
            };

            let current: u64 = match current_str.parse() {
                Ok(n) => n,
                Err(_) => {
                    return Response::client_error(
                        "cannot increment or decrement non-numeric value",
                    )
                }
            };

            // Calculate new value
            let new_value = if is_incr {
                current.wrapping_add(delta)
            } else {
                current.saturating_sub(delta)
            };

            // Store new value
            let new_value_str = new_value.to_string();
            storage.set(key, new_value_str.as_bytes().to_vec(), item.flags, 0);

            Response::numeric(new_value)
        }
    }
}

/// Extension trait for BytesMut to advance the buffer
trait BytesMutExt {
    fn advance(&mut self, cnt: usize);
}

impl BytesMutExt for BytesMut {
    fn advance(&mut self, cnt: usize) {
        // Use the Buf trait method to advance the buffer
        <BytesMut as bytes::Buf>::advance(self, cnt);
    }
}
