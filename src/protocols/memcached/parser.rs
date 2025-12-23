//! Memcached text protocol parser and response generator.
//!
//! Implements parsing for the memcached text protocol commands:
//! - Retrieval: get, gets
//! - Storage: set, add, replace, append, prepend, cas
//! - Deletion: delete
//! - Other: flush_all, stats, version, quit

use bytes::{Bytes, BytesMut};
use std::str;

/// Maximum key length allowed by memcached protocol
pub const MAX_KEY_LENGTH: usize = 250;

/// Parsed memcached command
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Get one or more keys
    Get { keys: Vec<String> },

    /// Get one or more keys with CAS tokens
    Gets { keys: Vec<String> },

    /// Store a value
    Set {
        key: String,
        flags: u32,
        exptime: u64,
        bytes: usize,
        noreply: bool,
    },

    /// Store a value only if key doesn't exist
    Add {
        key: String,
        flags: u32,
        exptime: u64,
        bytes: usize,
        noreply: bool,
    },

    /// Store a value only if key exists
    Replace {
        key: String,
        flags: u32,
        exptime: u64,
        bytes: usize,
        noreply: bool,
    },

    /// Append data to existing value
    Append {
        key: String,
        flags: u32,
        exptime: u64,
        bytes: usize,
        noreply: bool,
    },

    /// Prepend data to existing value
    Prepend {
        key: String,
        flags: u32,
        exptime: u64,
        bytes: usize,
        noreply: bool,
    },

    /// Compare-and-swap: store only if CAS token matches
    Cas {
        key: String,
        flags: u32,
        exptime: u64,
        bytes: usize,
        cas_unique: u64,
        noreply: bool,
    },

    /// Delete a key
    Delete { key: String, noreply: bool },

    /// Increment a numeric value
    Incr {
        key: String,
        value: u64,
        noreply: bool,
    },

    /// Decrement a numeric value
    Decr {
        key: String,
        value: u64,
        noreply: bool,
    },

    /// Flush all items (optionally with delay)
    FlushAll { delay: u64, noreply: bool },

    /// Get server statistics
    Stats,

    /// Get server version
    Version,

    /// Close connection
    Quit,
}

/// Protocol parsing errors
#[derive(Debug, Clone, PartialEq)]
pub enum ParseError {
    /// Need more data to complete parsing
    Incomplete,
    /// Invalid command format
    InvalidCommand(String),
    /// Key is too long
    KeyTooLong(String),
    /// Invalid number format
    InvalidNumber(String),
    /// Unknown command
    UnknownCommand(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Incomplete => write!(f, "Incomplete command"),
            ParseError::InvalidCommand(msg) => write!(f, "Invalid command: {}", msg),
            ParseError::KeyTooLong(key) => write!(f, "Key too long: {}", key),
            ParseError::InvalidNumber(msg) => write!(f, "Invalid number: {}", msg),
            ParseError::UnknownCommand(cmd) => write!(f, "Unknown command: {}", cmd),
        }
    }
}

impl std::error::Error for ParseError {}

/// Result of parsing a command
#[derive(Debug)]
pub enum ParseResult {
    /// Successfully parsed command with bytes consumed
    Complete(Command, usize),
    /// Need data block of specified size after command line
    NeedData {
        command_bytes: usize,
        data_bytes: usize,
    },
    /// Parse error
    Error(ParseError),
}

/// Parser for memcached text protocol
pub struct Parser;

impl Parser {
    /// Parse a command from the buffer
    pub fn parse(buffer: &[u8]) -> ParseResult {
        // Find the end of the command line
        let line_end = match find_crlf(buffer) {
            Some(pos) => pos,
            None => return ParseResult::Error(ParseError::Incomplete),
        };

        let line = match str::from_utf8(&buffer[..line_end]) {
            Ok(s) => s,
            Err(_) => {
                return ParseResult::Error(ParseError::InvalidCommand(
                    "Invalid UTF-8 in command".to_string(),
                ))
            }
        };

        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.is_empty() {
            return ParseResult::Error(ParseError::InvalidCommand("Empty command".to_string()));
        }

        let command_name = parts[0].to_lowercase();
        let command_line_bytes = line_end + 2; // Include \r\n

        match command_name.as_str() {
            "get" => Self::parse_get(&parts, false, command_line_bytes),
            "gets" => Self::parse_get(&parts, true, command_line_bytes),
            "set" => Self::parse_storage(&parts, "set", command_line_bytes),
            "add" => Self::parse_storage(&parts, "add", command_line_bytes),
            "replace" => Self::parse_storage(&parts, "replace", command_line_bytes),
            "append" => Self::parse_storage(&parts, "append", command_line_bytes),
            "prepend" => Self::parse_storage(&parts, "prepend", command_line_bytes),
            "cas" => Self::parse_cas(&parts, command_line_bytes),
            "delete" => Self::parse_delete(&parts, command_line_bytes),
            "incr" => Self::parse_incr_decr(&parts, true, command_line_bytes),
            "decr" => Self::parse_incr_decr(&parts, false, command_line_bytes),
            "flush_all" => Self::parse_flush_all(&parts, command_line_bytes),
            "stats" => ParseResult::Complete(Command::Stats, command_line_bytes),
            "version" => ParseResult::Complete(Command::Version, command_line_bytes),
            "quit" => ParseResult::Complete(Command::Quit, command_line_bytes),
            _ => ParseResult::Error(ParseError::UnknownCommand(command_name)),
        }
    }

    /// Parse get/gets command
    fn parse_get(parts: &[&str], with_cas: bool, command_bytes: usize) -> ParseResult {
        if parts.len() < 2 {
            return ParseResult::Error(ParseError::InvalidCommand(
                "get requires at least one key".to_string(),
            ));
        }

        let mut keys = Vec::new();
        for &key in &parts[1..] {
            if key.len() > MAX_KEY_LENGTH {
                return ParseResult::Error(ParseError::KeyTooLong(key.to_string()));
            }
            keys.push(key.to_string());
        }

        let command = if with_cas {
            Command::Gets { keys }
        } else {
            Command::Get { keys }
        };

        ParseResult::Complete(command, command_bytes)
    }

    /// Parse storage commands (set, add, replace, append, prepend)
    fn parse_storage(parts: &[&str], cmd: &str, command_bytes: usize) -> ParseResult {
        // Format: <command> <key> <flags> <exptime> <bytes> [noreply]
        if parts.len() < 5 {
            return ParseResult::Error(ParseError::InvalidCommand(format!(
                "{} requires key, flags, exptime, and bytes",
                cmd
            )));
        }

        let key = parts[1];
        if key.len() > MAX_KEY_LENGTH {
            return ParseResult::Error(ParseError::KeyTooLong(key.to_string()));
        }

        // Validate flags
        if parts[2].parse::<u32>().is_err() {
            return ParseResult::Error(ParseError::InvalidNumber(format!(
                "Invalid flags: {}",
                parts[2]
            )));
        }

        // Validate exptime
        if parts[3].parse::<u64>().is_err() {
            return ParseResult::Error(ParseError::InvalidNumber(format!(
                "Invalid exptime: {}",
                parts[3]
            )));
        }

        let bytes = match parts[4].parse::<usize>() {
            Ok(b) => b,
            Err(_) => {
                return ParseResult::Error(ParseError::InvalidNumber(format!(
                    "Invalid bytes: {}",
                    parts[4]
                )))
            }
        };

        // noreply is validated in parse_with_data

        ParseResult::NeedData {
            command_bytes,
            data_bytes: bytes,
        }
    }

    /// Parse cas command
    fn parse_cas(parts: &[&str], command_bytes: usize) -> ParseResult {
        // Format: cas <key> <flags> <exptime> <bytes> <cas unique> [noreply]
        if parts.len() < 6 {
            return ParseResult::Error(ParseError::InvalidCommand(
                "cas requires key, flags, exptime, bytes, and cas unique".to_string(),
            ));
        }

        let key = parts[1];
        if key.len() > MAX_KEY_LENGTH {
            return ParseResult::Error(ParseError::KeyTooLong(key.to_string()));
        }

        let bytes = match parts[4].parse::<usize>() {
            Ok(b) => b,
            Err(_) => {
                return ParseResult::Error(ParseError::InvalidNumber(format!(
                    "Invalid bytes: {}",
                    parts[4]
                )))
            }
        };

        ParseResult::NeedData {
            command_bytes,
            data_bytes: bytes,
        }
    }

    /// Parse delete command
    fn parse_delete(parts: &[&str], command_bytes: usize) -> ParseResult {
        // Format: delete <key> [noreply]
        if parts.len() < 2 {
            return ParseResult::Error(ParseError::InvalidCommand(
                "delete requires a key".to_string(),
            ));
        }

        let key = parts[1];
        if key.len() > MAX_KEY_LENGTH {
            return ParseResult::Error(ParseError::KeyTooLong(key.to_string()));
        }

        let noreply = parts.len() > 2 && parts[2].eq_ignore_ascii_case("noreply");

        ParseResult::Complete(
            Command::Delete {
                key: key.to_string(),
                noreply,
            },
            command_bytes,
        )
    }

    /// Parse incr/decr commands
    fn parse_incr_decr(parts: &[&str], is_incr: bool, command_bytes: usize) -> ParseResult {
        // Format: incr|decr <key> <value> [noreply]
        if parts.len() < 3 {
            return ParseResult::Error(ParseError::InvalidCommand(format!(
                "{} requires key and value",
                if is_incr { "incr" } else { "decr" }
            )));
        }

        let key = parts[1];
        if key.len() > MAX_KEY_LENGTH {
            return ParseResult::Error(ParseError::KeyTooLong(key.to_string()));
        }

        let value = match parts[2].parse::<u64>() {
            Ok(v) => v,
            Err(_) => {
                return ParseResult::Error(ParseError::InvalidNumber(format!(
                    "Invalid value: {}",
                    parts[2]
                )))
            }
        };

        let noreply = parts.len() > 3 && parts[3].eq_ignore_ascii_case("noreply");

        let command = if is_incr {
            Command::Incr {
                key: key.to_string(),
                value,
                noreply,
            }
        } else {
            Command::Decr {
                key: key.to_string(),
                value,
                noreply,
            }
        };

        ParseResult::Complete(command, command_bytes)
    }

    /// Parse flush_all command
    fn parse_flush_all(parts: &[&str], command_bytes: usize) -> ParseResult {
        // Format: flush_all [delay] [noreply]
        let mut delay = 0u64;
        let mut noreply = false;

        if parts.len() > 1 {
            if parts[1].eq_ignore_ascii_case("noreply") {
                noreply = true;
            } else {
                delay = parts[1].parse().unwrap_or(0);
                if parts.len() > 2 && parts[2].eq_ignore_ascii_case("noreply") {
                    noreply = true;
                }
            }
        }

        ParseResult::Complete(Command::FlushAll { delay, noreply }, command_bytes)
    }

    /// Parse a complete storage command with data block
    pub fn parse_with_data(buffer: &[u8]) -> ParseResult {
        // First parse the command line
        let line_end = match find_crlf(buffer) {
            Some(pos) => pos,
            None => return ParseResult::Error(ParseError::Incomplete),
        };

        let line = match str::from_utf8(&buffer[..line_end]) {
            Ok(s) => s,
            Err(_) => {
                return ParseResult::Error(ParseError::InvalidCommand(
                    "Invalid UTF-8 in command".to_string(),
                ))
            }
        };

        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.is_empty() {
            return ParseResult::Error(ParseError::InvalidCommand("Empty command".to_string()));
        }

        let command_name = parts[0].to_lowercase();
        let command_line_bytes = line_end + 2;

        // Parse the command to get data size
        let (data_bytes, is_cas) = match command_name.as_str() {
            "set" | "add" | "replace" | "append" | "prepend" => {
                if parts.len() < 5 {
                    return ParseResult::Error(ParseError::InvalidCommand(
                        "Storage command missing parameters".to_string(),
                    ));
                }
                match parts[4].parse::<usize>() {
                    Ok(b) => (b, false),
                    Err(_) => {
                        return ParseResult::Error(ParseError::InvalidNumber(
                            "Invalid bytes".to_string(),
                        ))
                    }
                }
            }
            "cas" => {
                if parts.len() < 6 {
                    return ParseResult::Error(ParseError::InvalidCommand(
                        "cas command missing parameters".to_string(),
                    ));
                }
                match parts[4].parse::<usize>() {
                    Ok(b) => (b, true),
                    Err(_) => {
                        return ParseResult::Error(ParseError::InvalidNumber(
                            "Invalid bytes".to_string(),
                        ))
                    }
                }
            }
            _ => {
                return ParseResult::Error(ParseError::InvalidCommand(
                    "Not a storage command".to_string(),
                ))
            }
        };

        // Check if we have enough data
        let total_needed = command_line_bytes + data_bytes + 2; // +2 for trailing \r\n
        if buffer.len() < total_needed {
            return ParseResult::Error(ParseError::Incomplete);
        }

        // Verify trailing \r\n after data
        if buffer[command_line_bytes + data_bytes] != b'\r'
            || buffer[command_line_bytes + data_bytes + 1] != b'\n'
        {
            return ParseResult::Error(ParseError::InvalidCommand(
                "Data block must end with \\r\\n".to_string(),
            ));
        }

        // Build the complete command
        let key = parts[1];
        if key.len() > MAX_KEY_LENGTH {
            return ParseResult::Error(ParseError::KeyTooLong(key.to_string()));
        }

        let flags = parts[2].parse::<u32>().unwrap_or(0);
        let exptime = parts[3].parse::<u64>().unwrap_or(0);

        let noreply = if is_cas {
            parts.len() > 6 && parts[6].eq_ignore_ascii_case("noreply")
        } else {
            parts.len() > 5 && parts[5].eq_ignore_ascii_case("noreply")
        };

        let command = match command_name.as_str() {
            "set" => Command::Set {
                key: key.to_string(),
                flags,
                exptime,
                bytes: data_bytes,
                noreply,
            },
            "add" => Command::Add {
                key: key.to_string(),
                flags,
                exptime,
                bytes: data_bytes,
                noreply,
            },
            "replace" => Command::Replace {
                key: key.to_string(),
                flags,
                exptime,
                bytes: data_bytes,
                noreply,
            },
            "append" => Command::Append {
                key: key.to_string(),
                flags,
                exptime,
                bytes: data_bytes,
                noreply,
            },
            "prepend" => Command::Prepend {
                key: key.to_string(),
                flags,
                exptime,
                bytes: data_bytes,
                noreply,
            },
            "cas" => {
                let cas_unique = parts[5].parse::<u64>().unwrap_or(0);
                Command::Cas {
                    key: key.to_string(),
                    flags,
                    exptime,
                    bytes: data_bytes,
                    cas_unique,
                    noreply,
                }
            }
            _ => unreachable!(),
        };

        ParseResult::Complete(command, total_needed)
    }

    /// Extract data from buffer for a storage command
    #[allow(dead_code)]
    pub fn extract_data(buffer: &[u8], command_bytes: usize, data_bytes: usize) -> Option<Bytes> {
        let total = command_bytes + data_bytes + 2;
        if buffer.len() < total {
            return None;
        }
        Some(Bytes::copy_from_slice(
            &buffer[command_bytes..command_bytes + data_bytes],
        ))
    }
}

/// Response generator for memcached protocol
pub struct Response;

impl Response {
    /// Generate a VALUE response line
    pub fn value(key: &str, flags: u32, data: &[u8], cas: Option<u64>) -> BytesMut {
        let mut response = BytesMut::new();
        let header = match cas {
            Some(cas_unique) => {
                format!("VALUE {} {} {} {}\r\n", key, flags, data.len(), cas_unique)
            }
            None => format!("VALUE {} {} {}\r\n", key, flags, data.len()),
        };
        response.extend_from_slice(header.as_bytes());
        response.extend_from_slice(data);
        response.extend_from_slice(b"\r\n");
        response
    }

    /// Generate END response
    pub fn end() -> &'static [u8] {
        b"END\r\n"
    }

    /// Generate STORED response
    pub fn stored() -> &'static [u8] {
        b"STORED\r\n"
    }

    /// Generate NOT_STORED response
    pub fn not_stored() -> &'static [u8] {
        b"NOT_STORED\r\n"
    }

    /// Generate EXISTS response (CAS failure)
    pub fn exists() -> &'static [u8] {
        b"EXISTS\r\n"
    }

    /// Generate NOT_FOUND response
    pub fn not_found() -> &'static [u8] {
        b"NOT_FOUND\r\n"
    }

    /// Generate DELETED response
    pub fn deleted() -> &'static [u8] {
        b"DELETED\r\n"
    }

    /// Generate OK response
    pub fn ok() -> &'static [u8] {
        b"OK\r\n"
    }

    /// Generate ERROR response
    pub fn error() -> &'static [u8] {
        b"ERROR\r\n"
    }

    /// Generate CLIENT_ERROR response
    pub fn client_error(msg: &str) -> BytesMut {
        let mut response = BytesMut::new();
        response.extend_from_slice(format!("CLIENT_ERROR {}\r\n", msg).as_bytes());
        response
    }

    /// Generate SERVER_ERROR response
    #[allow(dead_code)]
    pub fn server_error(msg: &str) -> BytesMut {
        let mut response = BytesMut::new();
        response.extend_from_slice(format!("SERVER_ERROR {}\r\n", msg).as_bytes());
        response
    }

    /// Generate VERSION response
    pub fn version() -> &'static [u8] {
        b"VERSION grow-a-cache 0.1.0\r\n"
    }

    /// Generate numeric response (for incr/decr)
    pub fn numeric(value: u64) -> BytesMut {
        let mut response = BytesMut::new();
        response.extend_from_slice(format!("{}\r\n", value).as_bytes());
        response
    }

    /// Generate a STAT line
    pub fn stat(name: &str, value: &str) -> BytesMut {
        let mut response = BytesMut::new();
        response.extend_from_slice(format!("STAT {} {}\r\n", name, value).as_bytes());
        response
    }
}

/// Find \r\n in buffer
fn find_crlf(buffer: &[u8]) -> Option<usize> {
    for i in 0..buffer.len().saturating_sub(1) {
        if buffer[i] == b'\r' && buffer[i + 1] == b'\n' {
            return Some(i);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_get() {
        let buffer = b"get key1 key2 key3\r\n";
        match Parser::parse(buffer) {
            ParseResult::Complete(Command::Get { keys }, bytes) => {
                assert_eq!(keys, vec!["key1", "key2", "key3"]);
                assert_eq!(bytes, 20);
            }
            _ => panic!("Expected Get command"),
        }
    }

    #[test]
    fn test_parse_gets() {
        let buffer = b"gets key1\r\n";
        match Parser::parse(buffer) {
            ParseResult::Complete(Command::Gets { keys }, _) => {
                assert_eq!(keys, vec!["key1"]);
            }
            _ => panic!("Expected Gets command"),
        }
    }

    #[test]
    fn test_parse_set() {
        let buffer = b"set mykey 0 3600 5\r\n";
        match Parser::parse(buffer) {
            ParseResult::NeedData {
                command_bytes,
                data_bytes,
            } => {
                assert_eq!(command_bytes, 20);
                assert_eq!(data_bytes, 5);
            }
            _ => panic!("Expected NeedData"),
        }
    }

    #[test]
    fn test_parse_set_with_data() {
        let buffer = b"set mykey 0 3600 5\r\nhello\r\n";
        match Parser::parse_with_data(buffer) {
            ParseResult::Complete(
                Command::Set {
                    key,
                    flags,
                    exptime,
                    bytes,
                    noreply,
                },
                total,
            ) => {
                assert_eq!(key, "mykey");
                assert_eq!(flags, 0);
                assert_eq!(exptime, 3600);
                assert_eq!(bytes, 5);
                assert!(!noreply);
                assert_eq!(total, 27);
            }
            _ => panic!("Expected Set command"),
        }
    }

    #[test]
    fn test_parse_set_noreply() {
        let buffer = b"set mykey 0 3600 5 noreply\r\nhello\r\n";
        match Parser::parse_with_data(buffer) {
            ParseResult::Complete(Command::Set { noreply, .. }, _) => {
                assert!(noreply);
            }
            _ => panic!("Expected Set command"),
        }
    }

    #[test]
    fn test_parse_cas() {
        let buffer = b"cas mykey 0 3600 5 12345\r\nhello\r\n";
        match Parser::parse_with_data(buffer) {
            ParseResult::Complete(
                Command::Cas {
                    key, cas_unique, ..
                },
                _,
            ) => {
                assert_eq!(key, "mykey");
                assert_eq!(cas_unique, 12345);
            }
            _ => panic!("Expected Cas command"),
        }
    }

    #[test]
    fn test_parse_delete() {
        let buffer = b"delete mykey\r\n";
        match Parser::parse(buffer) {
            ParseResult::Complete(Command::Delete { key, noreply }, _) => {
                assert_eq!(key, "mykey");
                assert!(!noreply);
            }
            _ => panic!("Expected Delete command"),
        }
    }

    #[test]
    fn test_parse_delete_noreply() {
        let buffer = b"delete mykey noreply\r\n";
        match Parser::parse(buffer) {
            ParseResult::Complete(Command::Delete { key, noreply }, _) => {
                assert_eq!(key, "mykey");
                assert!(noreply);
            }
            _ => panic!("Expected Delete command"),
        }
    }

    #[test]
    fn test_parse_flush_all() {
        let buffer = b"flush_all\r\n";
        match Parser::parse(buffer) {
            ParseResult::Complete(Command::FlushAll { delay, noreply }, _) => {
                assert_eq!(delay, 0);
                assert!(!noreply);
            }
            _ => panic!("Expected FlushAll command"),
        }
    }

    #[test]
    fn test_parse_flush_all_with_delay() {
        let buffer = b"flush_all 30\r\n";
        match Parser::parse(buffer) {
            ParseResult::Complete(Command::FlushAll { delay, .. }, _) => {
                assert_eq!(delay, 30);
            }
            _ => panic!("Expected FlushAll command"),
        }
    }

    #[test]
    fn test_parse_stats() {
        let buffer = b"stats\r\n";
        match Parser::parse(buffer) {
            ParseResult::Complete(Command::Stats, _) => {}
            _ => panic!("Expected Stats command"),
        }
    }

    #[test]
    fn test_parse_version() {
        let buffer = b"version\r\n";
        match Parser::parse(buffer) {
            ParseResult::Complete(Command::Version, _) => {}
            _ => panic!("Expected Version command"),
        }
    }

    #[test]
    fn test_parse_quit() {
        let buffer = b"quit\r\n";
        match Parser::parse(buffer) {
            ParseResult::Complete(Command::Quit, _) => {}
            _ => panic!("Expected Quit command"),
        }
    }

    #[test]
    fn test_parse_incomplete() {
        let buffer = b"get key";
        match Parser::parse(buffer) {
            ParseResult::Error(ParseError::Incomplete) => {}
            _ => panic!("Expected Incomplete error"),
        }
    }

    #[test]
    fn test_parse_unknown_command() {
        let buffer = b"unknown command\r\n";
        match Parser::parse(buffer) {
            ParseResult::Error(ParseError::UnknownCommand(cmd)) => {
                assert_eq!(cmd, "unknown");
            }
            _ => panic!("Expected UnknownCommand error"),
        }
    }

    #[test]
    fn test_response_value() {
        let response = Response::value("key1", 0, b"hello", None);
        assert_eq!(&response[..], b"VALUE key1 0 5\r\nhello\r\n");
    }

    #[test]
    fn test_response_value_with_cas() {
        let response = Response::value("key1", 0, b"hello", Some(12345));
        assert_eq!(&response[..], b"VALUE key1 0 5 12345\r\nhello\r\n");
    }

    #[test]
    fn test_key_too_long() {
        let long_key = "k".repeat(MAX_KEY_LENGTH + 1);
        let buffer = format!("get {}\r\n", long_key);
        match Parser::parse(buffer.as_bytes()) {
            ParseResult::Error(ParseError::KeyTooLong(_)) => {}
            _ => panic!("Expected KeyTooLong error"),
        }
    }

    #[test]
    fn test_parse_incr() {
        let buffer = b"incr counter 5\r\n";
        match Parser::parse(buffer) {
            ParseResult::Complete(Command::Incr { key, value, noreply }, _) => {
                assert_eq!(key, "counter");
                assert_eq!(value, 5);
                assert!(!noreply);
            }
            _ => panic!("Expected Incr command"),
        }
    }

    #[test]
    fn test_parse_decr() {
        let buffer = b"decr counter 3 noreply\r\n";
        match Parser::parse(buffer) {
            ParseResult::Complete(Command::Decr { key, value, noreply }, _) => {
                assert_eq!(key, "counter");
                assert_eq!(value, 3);
                assert!(noreply);
            }
            _ => panic!("Expected Decr command"),
        }
    }
}
