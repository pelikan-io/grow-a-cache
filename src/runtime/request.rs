//! Request dispatch for the custom runtime.
//!
//! Provides synchronous request processing that dispatches to protocol handlers
//! and works with raw byte buffers (no async runtime required).

use crate::protocols::memcached::parser::{Command, ParseResult, Parser, Response};
use crate::protocols::resp::parser as resp_parser;
use crate::storage::{Storage, StorageResult};
use std::sync::Arc;

/// Protocol type for command processing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Memcached,
    Resp,
    Ping,
    Echo,
}

/// Result of processing a buffer.
pub enum ProcessResult {
    /// Need more data to complete parsing.
    NeedData,
    /// Large value detected - need chain buffers for accumulation.
    /// The event loop should allocate chain buffers and continue reading.
    NeedChain {
        /// Bytes consumed by the command header.
        command_len: usize,
        /// Expected value size (from command header).
        value_len: usize,
    },
    /// Successfully processed, response written to output buffer.
    /// Returns bytes consumed from input.
    Response {
        consumed: usize,
        response_len: usize,
    },
    /// Large response that doesn't fit in a single buffer.
    /// The event loop should use a write chain for this response.
    LargeResponse {
        consumed: usize,
        response_data: Vec<u8>,
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
pub fn process_memcached(
    input: &[u8],
    output: &mut [u8],
    storage: &Arc<Storage>,
    max_value_size: usize,
) -> ProcessResult {
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
                    // Check max value size
                    if *bytes > max_value_size {
                        let response = Response::client_error("value too large");
                        let len = copy_response(&response, output);
                        // Consume the command but not the data (connection will be closed)
                        return ProcessResult::Response {
                            consumed,
                            response_len: len,
                        };
                    }

                    let data_end = consumed + bytes + 2; // +2 for \r\n
                    if input.len() < data_end {
                        // Check if value is larger than buffer - need chain
                        if *bytes > output.len() {
                            return ProcessResult::NeedChain {
                                command_len: consumed,
                                value_len: *bytes,
                            };
                        }
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
                Command::Get { .. } | Command::Gets { .. } => {
                    let response = execute_command(&command, storage);

                    // Check if response fits in output buffer
                    if response.len() > output.len() {
                        return ProcessResult::LargeResponse {
                            consumed,
                            response_data: response,
                        };
                    }

                    let len = copy_response(&response, output);
                    ProcessResult::Response {
                        consumed,
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
            // Check max value size early
            if data_bytes > max_value_size {
                let response = Response::client_error("value too large");
                let len = copy_response(&response, output);
                return ProcessResult::Response {
                    consumed: command_bytes + 2, // Skip past command line
                    response_len: len,
                };
            }

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
                // Check if we need chain buffers for large value
                if data_bytes > output.len() {
                    return ProcessResult::NeedChain {
                        command_len: command_bytes,
                        value_len: data_bytes,
                    };
                }
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
pub fn process_resp(
    input: &[u8],
    output: &mut [u8],
    storage: &Arc<Storage>,
    max_value_size: usize,
) -> ProcessResult {
    match resp_parser::parse(input) {
        resp_parser::ParseResult::Complete(frame, consumed) => {
            // Check for large values in SET command
            if let resp_parser::Frame::Array(Some(args)) = &frame {
                if args.len() >= 3 {
                    if let resp_parser::Frame::Bulk(Some(cmd)) = &args[0] {
                        if cmd.eq_ignore_ascii_case(b"SET") {
                            if let resp_parser::Frame::Bulk(Some(value)) = &args[2] {
                                if value.len() > max_value_size {
                                    let response =
                                        resp_parser::Frame::error("ERR value too large");
                                    let encoded = response.encode();
                                    let len = encoded.len().min(output.len());
                                    output[..len].copy_from_slice(&encoded[..len]);
                                    return ProcessResult::Response {
                                        consumed,
                                        response_len: len,
                                    };
                                }
                            }
                        }
                    }
                }
            }

            let response = execute_resp_command(&frame, storage);
            let encoded = response.encode();

            // Check if response fits in output buffer
            if encoded.len() > output.len() {
                return ProcessResult::LargeResponse {
                    consumed,
                    response_data: encoded.to_vec(),
                };
            }

            let len = encoded.len();
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

/// Process a Ping protocol buffer.
///
/// Simple line-based protocol:
/// - `PING\r\n` → `PONG\r\n`
/// - `PING <msg>\r\n` → `PONG <msg>\r\n`
/// - `QUIT\r\n` → close connection
#[allow(unused_variables)]
pub fn process_ping(input: &[u8], output: &mut [u8], storage: &Arc<Storage>) -> ProcessResult {
    // Find line ending
    let line_end = match find_crlf(input) {
        Some(pos) => pos,
        None => return ProcessResult::NeedData,
    };

    let line = &input[..line_end];
    let consumed = line_end + 2; // include \r\n

    // Parse command (case-insensitive)
    let response = if line.eq_ignore_ascii_case(b"PING") {
        b"PONG\r\n".as_slice()
    } else if line.eq_ignore_ascii_case(b"QUIT") {
        return ProcessResult::Quit;
    } else if line.len() > 5
        && (line[..5].eq_ignore_ascii_case(b"PING ") || line[..5].eq_ignore_ascii_case(b"ping "))
    {
        // PING with message: echo back with PONG prefix
        let msg = &line[5..];
        let response_len = 5 + msg.len() + 2; // "PONG " + msg + "\r\n"
        if response_len > output.len() {
            return ProcessResult::Error;
        }
        output[..5].copy_from_slice(b"PONG ");
        output[5..5 + msg.len()].copy_from_slice(msg);
        output[5 + msg.len()..response_len].copy_from_slice(b"\r\n");
        return ProcessResult::Response {
            consumed,
            response_len,
        };
    } else {
        b"ERROR unknown command\r\n".as_slice()
    };

    let len = response.len().min(output.len());
    output[..len].copy_from_slice(&response[..len]);

    ProcessResult::Response {
        consumed,
        response_len: len,
    }
}

/// Process an Echo protocol buffer.
///
/// Length-prefixed binary protocol:
/// - `<length>\r\n<data>` → `<length>\r\n<data>`
/// - `QUIT\r\n` → close connection
#[allow(unused_variables)]
pub fn process_echo(
    input: &[u8],
    output: &mut [u8],
    storage: &Arc<Storage>,
    max_value_size: usize,
) -> ProcessResult {
    // Find line ending for the length prefix
    let line_end = match find_crlf(input) {
        Some(pos) => pos,
        None => return ProcessResult::NeedData,
    };

    let line = &input[..line_end];

    // Check for QUIT
    if line.eq_ignore_ascii_case(b"QUIT") {
        return ProcessResult::Quit;
    }

    // Parse length
    let length_str = match std::str::from_utf8(line) {
        Ok(s) => s,
        Err(_) => return ProcessResult::Error,
    };

    let length: usize = match length_str.parse() {
        Ok(len) => len,
        Err(_) => {
            let err = b"ERROR invalid length\r\n";
            let len = err.len().min(output.len());
            output[..len].copy_from_slice(&err[..len]);
            return ProcessResult::Response {
                consumed: line_end + 2,
                response_len: len,
            };
        }
    };

    // Check max value size
    if length > max_value_size {
        let err = b"ERROR value too large\r\n";
        let len = err.len().min(output.len());
        output[..len].copy_from_slice(&err[..len]);
        return ProcessResult::Response {
            consumed: line_end + 2,
            response_len: len,
        };
    }

    // Check if we have enough data
    let header_len = line_end + 2; // length line + \r\n
    let total_needed = header_len + length;
    if input.len() < total_needed {
        // Check if value is larger than buffer - need chain
        if length > output.len() {
            return ProcessResult::NeedChain {
                command_len: header_len,
                value_len: length,
            };
        }
        return ProcessResult::NeedData;
    }

    // Echo back: length + \r\n + data
    let response_header = format!("{length}\r\n");
    let header_bytes = response_header.as_bytes();
    let response_len = header_bytes.len() + length;

    // Check if response fits in output buffer
    if response_len > output.len() {
        let mut response_data = Vec::with_capacity(response_len);
        response_data.extend_from_slice(header_bytes);
        response_data.extend_from_slice(&input[header_len..total_needed]);
        return ProcessResult::LargeResponse {
            consumed: total_needed,
            response_data,
        };
    }

    output[..header_bytes.len()].copy_from_slice(header_bytes);
    output[header_bytes.len()..response_len].copy_from_slice(&input[header_len..total_needed]);

    ProcessResult::Response {
        consumed: total_needed,
        response_len,
    }
}

/// Find \r\n in buffer, returning the position of \r.
fn find_crlf(buffer: &[u8]) -> Option<usize> {
    (0..buffer.len().saturating_sub(1)).find(|&i| buffer[i] == b'\r' && buffer[i + 1] == b'\n')
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
