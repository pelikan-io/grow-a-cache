//! Protocol processing for the custom runtime.
//!
//! Provides synchronous protocol parsing and response generation
//! that works with raw byte buffers (no async runtime required).

use crate::protocols::memcached::parser::{Command, ParseResult, Parser, Response};
use crate::protocols::resp::parser as resp_parser;
use crate::storage::{Storage, StorageResult};
use std::sync::Arc;

/// Protocol type for command processing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Memcached,
    Resp,
}

/// Result of processing a buffer.
pub enum ProcessResult {
    /// Need more data to complete parsing.
    NeedData,
    /// Successfully processed, response written to output buffer.
    /// Returns bytes consumed from input.
    Response {
        consumed: usize,
        response_len: usize,
    },
    /// Client sent quit command.
    Quit,
    /// Protocol error, connection should be closed.
    Error,
}

/// Process a Memcached protocol buffer.
///
/// Parses commands from `input`, executes them against `storage`,
/// and writes responses to `output`.
///
/// Returns the number of bytes consumed from input and written to output.
pub fn process_memcached(input: &[u8], output: &mut [u8], storage: &Arc<Storage>) -> ProcessResult {
    match Parser::parse(input) {
        ParseResult::Complete(command, consumed) => {
            if matches!(command, Command::Quit) {
                return ProcessResult::Quit;
            }

            // Check if this is a storage command that needs data
            match &command {
                Command::Set { bytes, .. }
                | Command::Add { bytes, .. }
                | Command::Replace { bytes, .. }
                | Command::Append { bytes, .. }
                | Command::Prepend { bytes, .. }
                | Command::Cas { bytes, .. } => {
                    let data_end = consumed + bytes + 2; // +2 for \r\n
                    if input.len() < data_end {
                        return ProcessResult::NeedData;
                    }

                    let data = &input[consumed..consumed + bytes];
                    let response = execute_storage_command(&command, storage, data);
                    let len = copy_response(&response, output);

                    ProcessResult::Response {
                        consumed: data_end,
                        response_len: len,
                    }
                }
                _ => {
                    let response = execute_command(&command, storage);
                    let len = copy_response(&response, output);

                    ProcessResult::Response {
                        consumed,
                        response_len: len,
                    }
                }
            }
        }
        ParseResult::NeedData {
            command_bytes,
            data_bytes,
        } => {
            let total_needed = command_bytes + data_bytes + 2;
            if input.len() >= total_needed {
                // We have enough data, re-parse with data
                match Parser::parse_with_data(input) {
                    ParseResult::Complete(command, consumed) => {
                        let data = &input[command_bytes..command_bytes + data_bytes];
                        let response = execute_storage_command(&command, storage, data);
                        let len = copy_response(&response, output);

                        ProcessResult::Response {
                            consumed,
                            response_len: len,
                        }
                    }
                    _ => ProcessResult::NeedData,
                }
            } else {
                ProcessResult::NeedData
            }
        }
        ParseResult::Error(crate::protocols::memcached::parser::ParseError::Incomplete) => {
            ProcessResult::NeedData
        }
        ParseResult::Error(_) => ProcessResult::Error,
    }
}

/// Process a RESP protocol buffer.
pub fn process_resp(input: &[u8], output: &mut [u8], storage: &Arc<Storage>) -> ProcessResult {
    match resp_parser::parse(input) {
        resp_parser::ParseResult::Complete(frame, consumed) => {
            let response = execute_resp_command(&frame, storage);
            let encoded = response.encode();
            let len = encoded.len().min(output.len());
            output[..len].copy_from_slice(&encoded[..len]);

            ProcessResult::Response {
                consumed,
                response_len: len,
            }
        }
        resp_parser::ParseResult::Incomplete => ProcessResult::NeedData,
        resp_parser::ParseResult::Error(_) => ProcessResult::Error,
    }
}

fn execute_command(command: &Command, storage: &Arc<Storage>) -> Vec<u8> {
    match command {
        Command::Get { keys } => {
            let keys_ref: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
            let items = storage.get_multi(&keys_ref);

            let mut response = Vec::new();
            for (key, item) in items {
                response.extend_from_slice(&Response::value(&key, item.flags, &item.value, None));
            }
            response.extend_from_slice(Response::end());
            response
        }

        Command::Gets { keys } => {
            let keys_ref: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
            let items = storage.get_multi(&keys_ref);

            let mut response = Vec::new();
            for (key, item) in items {
                response.extend_from_slice(&Response::value(
                    &key,
                    item.flags,
                    &item.value,
                    Some(item.cas_unique),
                ));
            }
            response.extend_from_slice(Response::end());
            response
        }

        Command::Delete { key, noreply } => {
            let result = storage.delete(key);
            if *noreply {
                Vec::new()
            } else {
                match result {
                    StorageResult::Deleted => Response::deleted().to_vec(),
                    _ => Response::not_found().to_vec(),
                }
            }
        }

        Command::Incr {
            key,
            value,
            noreply,
        } => {
            let result = handle_incr_decr(storage, key, *value, true);
            if *noreply {
                Vec::new()
            } else {
                result
            }
        }

        Command::Decr {
            key,
            value,
            noreply,
        } => {
            let result = handle_incr_decr(storage, key, *value, false);
            if *noreply {
                Vec::new()
            } else {
                result
            }
        }

        Command::FlushAll { delay: _, noreply } => {
            // Note: delayed flush not supported in sync context
            storage.flush_all();
            if *noreply {
                Vec::new()
            } else {
                Response::ok().to_vec()
            }
        }

        Command::Stats => {
            let stats = storage.stats();
            let mut response = Vec::new();
            response
                .extend_from_slice(&Response::stat("curr_items", &stats.item_count.to_string()));
            response.extend_from_slice(&Response::stat("bytes", &stats.memory_used.to_string()));
            response.extend_from_slice(&Response::stat(
                "limit_maxbytes",
                &stats.max_memory.to_string(),
            ));
            response.extend_from_slice(Response::end());
            response
        }

        Command::Version => Response::version().to_vec(),

        Command::Quit => Vec::new(),

        _ => Response::error().to_vec(),
    }
}

fn execute_storage_command(command: &Command, storage: &Arc<Storage>, data: &[u8]) -> Vec<u8> {
    match command {
        Command::Set {
            key,
            flags,
            exptime,
            noreply,
            ..
        } => {
            let result = storage.set(key, data.to_vec(), *flags, *exptime);
            if *noreply {
                Vec::new()
            } else {
                match result {
                    StorageResult::Stored => Response::stored().to_vec(),
                    _ => Response::not_stored().to_vec(),
                }
            }
        }

        Command::Add {
            key,
            flags,
            exptime,
            noreply,
            ..
        } => {
            let result = storage.add(key, data.to_vec(), *flags, *exptime);
            if *noreply {
                Vec::new()
            } else {
                match result {
                    StorageResult::Stored => Response::stored().to_vec(),
                    _ => Response::not_stored().to_vec(),
                }
            }
        }

        Command::Replace {
            key,
            flags,
            exptime,
            noreply,
            ..
        } => {
            let result = storage.replace(key, data.to_vec(), *flags, *exptime);
            if *noreply {
                Vec::new()
            } else {
                match result {
                    StorageResult::Stored => Response::stored().to_vec(),
                    _ => Response::not_stored().to_vec(),
                }
            }
        }

        Command::Append { key, noreply, .. } => {
            let result = storage.append(key, data);
            if *noreply {
                Vec::new()
            } else {
                match result {
                    StorageResult::Stored => Response::stored().to_vec(),
                    _ => Response::not_stored().to_vec(),
                }
            }
        }

        Command::Prepend { key, noreply, .. } => {
            let result = storage.prepend(key, data);
            if *noreply {
                Vec::new()
            } else {
                match result {
                    StorageResult::Stored => Response::stored().to_vec(),
                    _ => Response::not_stored().to_vec(),
                }
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
            let result = storage.cas(key, data.to_vec(), *flags, *exptime, *cas_unique);
            if *noreply {
                Vec::new()
            } else {
                match result {
                    StorageResult::Stored => Response::stored().to_vec(),
                    StorageResult::CasMismatch => Response::exists().to_vec(),
                    StorageResult::NotFound => Response::not_found().to_vec(),
                    _ => Response::not_stored().to_vec(),
                }
            }
        }

        _ => Response::error().to_vec(),
    }
}

fn execute_resp_command(frame: &resp_parser::Frame, storage: &Arc<Storage>) -> resp_parser::Frame {
    use resp_parser::Frame;

    let args = match frame {
        Frame::Array(Some(arr)) => arr,
        _ => return Frame::error("ERR invalid command format"),
    };

    if args.is_empty() {
        return Frame::error("ERR empty command");
    }

    let cmd = match &args[0] {
        Frame::Bulk(Some(s)) => String::from_utf8_lossy(s).to_uppercase(),
        _ => return Frame::error("ERR invalid command"),
    };

    match cmd.as_str() {
        "PING" => {
            if args.len() > 1 {
                args[1].clone()
            } else {
                Frame::simple("PONG")
            }
        }

        "GET" => {
            if args.len() != 2 {
                return Frame::error("ERR wrong number of arguments for 'get' command");
            }
            let key = match &args[1] {
                Frame::Bulk(Some(k)) => String::from_utf8_lossy(k),
                _ => return Frame::error("ERR invalid key"),
            };
            match storage.get(&key) {
                Some(item) => Frame::bulk(item.value),
                None => Frame::null(),
            }
        }

        "SET" => {
            if args.len() < 3 {
                return Frame::error("ERR wrong number of arguments for 'set' command");
            }
            let key = match &args[1] {
                Frame::Bulk(Some(k)) => String::from_utf8_lossy(k).to_string(),
                _ => return Frame::error("ERR invalid key"),
            };
            let value = match &args[2] {
                Frame::Bulk(Some(v)) => v.to_vec(),
                _ => return Frame::error("ERR invalid value"),
            };
            storage.set(&key, value, 0, 0);
            Frame::simple("OK")
        }

        "DEL" => {
            if args.len() < 2 {
                return Frame::error("ERR wrong number of arguments for 'del' command");
            }
            let mut count = 0i64;
            for arg in &args[1..] {
                if let Frame::Bulk(Some(key)) = arg {
                    let key_str = String::from_utf8_lossy(key);
                    if matches!(storage.delete(&key_str), StorageResult::Deleted) {
                        count += 1;
                    }
                }
            }
            Frame::integer(count)
        }

        "EXISTS" => {
            if args.len() < 2 {
                return Frame::error("ERR wrong number of arguments for 'exists' command");
            }
            let mut count = 0i64;
            for arg in &args[1..] {
                if let Frame::Bulk(Some(key)) = arg {
                    let key_str = String::from_utf8_lossy(key);
                    if storage.get(&key_str).is_some() {
                        count += 1;
                    }
                }
            }
            Frame::integer(count)
        }

        "FLUSHALL" | "FLUSHDB" => {
            storage.flush_all();
            Frame::simple("OK")
        }

        "DBSIZE" => Frame::integer(storage.stats().item_count as i64),

        "QUIT" => Frame::simple("OK"),

        _ => Frame::error(format!("ERR unknown command '{cmd}'")),
    }
}

fn handle_incr_decr(storage: &Arc<Storage>, key: &str, delta: u64, is_incr: bool) -> Vec<u8> {
    match storage.get(key) {
        None => Response::not_found().to_vec(),
        Some(item) => {
            let current_str = match std::str::from_utf8(&item.value) {
                Ok(s) => s.trim(),
                Err(_) => {
                    return Response::client_error(
                        "cannot increment or decrement non-numeric value",
                    )
                    .to_vec();
                }
            };

            let current: u64 = match current_str.parse() {
                Ok(n) => n,
                Err(_) => {
                    return Response::client_error(
                        "cannot increment or decrement non-numeric value",
                    )
                    .to_vec();
                }
            };

            let new_value = if is_incr {
                current.wrapping_add(delta)
            } else {
                current.saturating_sub(delta)
            };

            let new_value_str = new_value.to_string();
            storage.set(key, new_value_str.as_bytes().to_vec(), item.flags, 0);

            Response::numeric(new_value).to_vec()
        }
    }
}

fn copy_response(response: &[u8], output: &mut [u8]) -> usize {
    let len = response.len().min(output.len());
    output[..len].copy_from_slice(&response[..len]);
    len
}
