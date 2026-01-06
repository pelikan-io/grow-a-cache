//! RESP (Redis Serialization Protocol) parser.
//!
//! Implements parsing for RESP2 and RESP3 protocol frames.
//! RESP is a binary-safe protocol that uses length-prefixed strings.

use bytes::{Bytes, BytesMut};

/// RESP frame types
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    /// Simple string: +OK\r\n
    Simple(String),
    /// Error: -ERR message\r\n
    Error(String),
    /// Integer: :1000\r\n
    Integer(i64),
    /// Bulk string: $5\r\nhello\r\n or $-1\r\n (null)
    Bulk(Option<Bytes>),
    /// Array: *2\r\n... or *-1\r\n (null)
    Array(Option<Vec<Frame>>),
}

impl Frame {
    /// Encode a frame to bytes
    pub fn encode(&self) -> BytesMut {
        let mut buf = BytesMut::new();
        self.encode_into(&mut buf);
        buf
    }

    /// Encode a frame into an existing buffer
    pub fn encode_into(&self, buf: &mut BytesMut) {
        match self {
            Frame::Simple(s) => {
                buf.extend_from_slice(b"+");
                buf.extend_from_slice(s.as_bytes());
                buf.extend_from_slice(b"\r\n");
            }
            Frame::Error(s) => {
                buf.extend_from_slice(b"-");
                buf.extend_from_slice(s.as_bytes());
                buf.extend_from_slice(b"\r\n");
            }
            Frame::Integer(n) => {
                buf.extend_from_slice(b":");
                buf.extend_from_slice(n.to_string().as_bytes());
                buf.extend_from_slice(b"\r\n");
            }
            Frame::Bulk(None) => {
                buf.extend_from_slice(b"$-1\r\n");
            }
            Frame::Bulk(Some(data)) => {
                buf.extend_from_slice(b"$");
                buf.extend_from_slice(data.len().to_string().as_bytes());
                buf.extend_from_slice(b"\r\n");
                buf.extend_from_slice(data);
                buf.extend_from_slice(b"\r\n");
            }
            Frame::Array(None) => {
                buf.extend_from_slice(b"*-1\r\n");
            }
            Frame::Array(Some(frames)) => {
                buf.extend_from_slice(b"*");
                buf.extend_from_slice(frames.len().to_string().as_bytes());
                buf.extend_from_slice(b"\r\n");
                for frame in frames {
                    frame.encode_into(buf);
                }
            }
        }
    }

    /// Create a simple string response
    pub fn simple<S: Into<String>>(s: S) -> Frame {
        Frame::Simple(s.into())
    }

    /// Create an error response
    pub fn error<S: Into<String>>(s: S) -> Frame {
        Frame::Error(s.into())
    }

    /// Create a null bulk string response
    pub fn null() -> Frame {
        Frame::Bulk(None)
    }

    /// Create a bulk string response
    pub fn bulk<B: Into<Bytes>>(data: B) -> Frame {
        Frame::Bulk(Some(data.into()))
    }

    /// Create an integer response
    pub fn integer(n: i64) -> Frame {
        Frame::Integer(n)
    }

    /// Create an array response
    pub fn array(frames: Vec<Frame>) -> Frame {
        Frame::Array(Some(frames))
    }
}

/// Parse result
#[derive(Debug)]
pub enum ParseResult {
    /// Successfully parsed a frame with bytes consumed
    Complete(Frame, usize),
    /// Need more data
    Incomplete,
    /// Parse error
    Error(String),
}

/// Parse a RESP frame from a buffer
pub fn parse(buffer: &[u8]) -> ParseResult {
    if buffer.is_empty() {
        return ParseResult::Incomplete;
    }

    match buffer[0] {
        b'+' => parse_simple_string(buffer),
        b'-' => parse_error(buffer),
        b':' => parse_integer(buffer),
        b'$' => parse_bulk_string(buffer),
        b'*' => parse_array(buffer),
        _ => ParseResult::Error(format!("Unknown frame type: {}", buffer[0] as char)),
    }
}

/// Find CRLF in buffer, return position of \r
fn find_crlf(buffer: &[u8]) -> Option<usize> {
    (0..buffer.len().saturating_sub(1)).find(|&i| buffer[i] == b'\r' && buffer[i + 1] == b'\n')
}

/// Parse a simple string: +OK\r\n
fn parse_simple_string(buffer: &[u8]) -> ParseResult {
    if let Some(end) = find_crlf(buffer) {
        let s = match std::str::from_utf8(&buffer[1..end]) {
            Ok(s) => s.to_string(),
            Err(_) => return ParseResult::Error("Invalid UTF-8 in simple string".to_string()),
        };
        ParseResult::Complete(Frame::Simple(s), end + 2)
    } else {
        ParseResult::Incomplete
    }
}

/// Parse an error: -ERR message\r\n
fn parse_error(buffer: &[u8]) -> ParseResult {
    if let Some(end) = find_crlf(buffer) {
        let s = match std::str::from_utf8(&buffer[1..end]) {
            Ok(s) => s.to_string(),
            Err(_) => return ParseResult::Error("Invalid UTF-8 in error".to_string()),
        };
        ParseResult::Complete(Frame::Error(s), end + 2)
    } else {
        ParseResult::Incomplete
    }
}

/// Parse an integer: :1000\r\n
fn parse_integer(buffer: &[u8]) -> ParseResult {
    if let Some(end) = find_crlf(buffer) {
        let s = match std::str::from_utf8(&buffer[1..end]) {
            Ok(s) => s,
            Err(_) => return ParseResult::Error("Invalid UTF-8 in integer".to_string()),
        };
        match s.parse::<i64>() {
            Ok(n) => ParseResult::Complete(Frame::Integer(n), end + 2),
            Err(_) => ParseResult::Error(format!("Invalid integer: {s}")),
        }
    } else {
        ParseResult::Incomplete
    }
}

/// Parse a bulk string: $5\r\nhello\r\n or $-1\r\n
fn parse_bulk_string(buffer: &[u8]) -> ParseResult {
    if let Some(len_end) = find_crlf(buffer) {
        let len_str = match std::str::from_utf8(&buffer[1..len_end]) {
            Ok(s) => s,
            Err(_) => return ParseResult::Error("Invalid UTF-8 in bulk string length".to_string()),
        };

        let len: i64 = match len_str.parse() {
            Ok(n) => n,
            Err(_) => {
                return ParseResult::Error(format!("Invalid bulk string length: {len_str}"))
            }
        };

        // Null bulk string
        if len < 0 {
            return ParseResult::Complete(Frame::Bulk(None), len_end + 2);
        }

        let len = len as usize;
        let data_start = len_end + 2;
        let data_end = data_start + len;
        let total_len = data_end + 2; // +2 for trailing \r\n

        if buffer.len() < total_len {
            return ParseResult::Incomplete;
        }

        // Verify trailing CRLF
        if buffer[data_end] != b'\r' || buffer[data_end + 1] != b'\n' {
            return ParseResult::Error("Bulk string missing trailing CRLF".to_string());
        }

        let data = Bytes::copy_from_slice(&buffer[data_start..data_end]);
        ParseResult::Complete(Frame::Bulk(Some(data)), total_len)
    } else {
        ParseResult::Incomplete
    }
}

/// Parse an array: *2\r\n$3\r\nfoo\r\n$3\r\nbar\r\n or *-1\r\n
fn parse_array(buffer: &[u8]) -> ParseResult {
    if let Some(len_end) = find_crlf(buffer) {
        let len_str = match std::str::from_utf8(&buffer[1..len_end]) {
            Ok(s) => s,
            Err(_) => return ParseResult::Error("Invalid UTF-8 in array length".to_string()),
        };

        let len: i64 = match len_str.parse() {
            Ok(n) => n,
            Err(_) => return ParseResult::Error(format!("Invalid array length: {len_str}")),
        };

        // Null array
        if len < 0 {
            return ParseResult::Complete(Frame::Array(None), len_end + 2);
        }

        let len = len as usize;
        let mut offset = len_end + 2;
        let mut frames = Vec::with_capacity(len);

        for _ in 0..len {
            if offset >= buffer.len() {
                return ParseResult::Incomplete;
            }

            match parse(&buffer[offset..]) {
                ParseResult::Complete(frame, consumed) => {
                    frames.push(frame);
                    offset += consumed;
                }
                ParseResult::Incomplete => return ParseResult::Incomplete,
                ParseResult::Error(e) => return ParseResult::Error(e),
            }
        }

        ParseResult::Complete(Frame::Array(Some(frames)), offset)
    } else {
        ParseResult::Incomplete
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_string() {
        let buffer = b"+OK\r\n";
        match parse(buffer) {
            ParseResult::Complete(Frame::Simple(s), consumed) => {
                assert_eq!(s, "OK");
                assert_eq!(consumed, 5);
            }
            _ => panic!("Expected simple string"),
        }
    }

    #[test]
    fn test_parse_error() {
        let buffer = b"-ERR unknown command\r\n";
        match parse(buffer) {
            ParseResult::Complete(Frame::Error(s), consumed) => {
                assert_eq!(s, "ERR unknown command");
                assert_eq!(consumed, 22);
            }
            _ => panic!("Expected error"),
        }
    }

    #[test]
    fn test_parse_integer() {
        let buffer = b":1000\r\n";
        match parse(buffer) {
            ParseResult::Complete(Frame::Integer(n), consumed) => {
                assert_eq!(n, 1000);
                assert_eq!(consumed, 7);
            }
            _ => panic!("Expected integer"),
        }
    }

    #[test]
    fn test_parse_negative_integer() {
        let buffer = b":-42\r\n";
        match parse(buffer) {
            ParseResult::Complete(Frame::Integer(n), _) => {
                assert_eq!(n, -42);
            }
            _ => panic!("Expected integer"),
        }
    }

    #[test]
    fn test_parse_bulk_string() {
        let buffer = b"$5\r\nhello\r\n";
        match parse(buffer) {
            ParseResult::Complete(Frame::Bulk(Some(data)), consumed) => {
                assert_eq!(&data[..], b"hello");
                assert_eq!(consumed, 11);
            }
            _ => panic!("Expected bulk string"),
        }
    }

    #[test]
    fn test_parse_null_bulk_string() {
        let buffer = b"$-1\r\n";
        match parse(buffer) {
            ParseResult::Complete(Frame::Bulk(None), consumed) => {
                assert_eq!(consumed, 5);
            }
            _ => panic!("Expected null bulk string"),
        }
    }

    #[test]
    fn test_parse_empty_bulk_string() {
        let buffer = b"$0\r\n\r\n";
        match parse(buffer) {
            ParseResult::Complete(Frame::Bulk(Some(data)), consumed) => {
                assert_eq!(data.len(), 0);
                assert_eq!(consumed, 6);
            }
            _ => panic!("Expected empty bulk string"),
        }
    }

    #[test]
    fn test_parse_array() {
        let buffer = b"*2\r\n$3\r\nfoo\r\n$3\r\nbar\r\n";
        match parse(buffer) {
            ParseResult::Complete(Frame::Array(Some(frames)), consumed) => {
                assert_eq!(frames.len(), 2);
                assert_eq!(consumed, 22);
                match &frames[0] {
                    Frame::Bulk(Some(data)) => assert_eq!(&data[..], b"foo"),
                    _ => panic!("Expected bulk string"),
                }
                match &frames[1] {
                    Frame::Bulk(Some(data)) => assert_eq!(&data[..], b"bar"),
                    _ => panic!("Expected bulk string"),
                }
            }
            _ => panic!("Expected array"),
        }
    }

    #[test]
    fn test_parse_null_array() {
        let buffer = b"*-1\r\n";
        match parse(buffer) {
            ParseResult::Complete(Frame::Array(None), consumed) => {
                assert_eq!(consumed, 5);
            }
            _ => panic!("Expected null array"),
        }
    }

    #[test]
    fn test_parse_empty_array() {
        let buffer = b"*0\r\n";
        match parse(buffer) {
            ParseResult::Complete(Frame::Array(Some(frames)), consumed) => {
                assert_eq!(frames.len(), 0);
                assert_eq!(consumed, 4);
            }
            _ => panic!("Expected empty array"),
        }
    }

    #[test]
    fn test_parse_incomplete() {
        let buffer = b"+OK";
        match parse(buffer) {
            ParseResult::Incomplete => {}
            _ => panic!("Expected incomplete"),
        }

        let buffer = b"$5\r\nhel";
        match parse(buffer) {
            ParseResult::Incomplete => {}
            _ => panic!("Expected incomplete"),
        }

        let buffer = b"*2\r\n$3\r\nfoo\r\n";
        match parse(buffer) {
            ParseResult::Incomplete => {}
            _ => panic!("Expected incomplete"),
        }
    }

    #[test]
    fn test_encode_simple_string() {
        let frame = Frame::simple("OK");
        assert_eq!(&frame.encode()[..], b"+OK\r\n");
    }

    #[test]
    fn test_encode_error() {
        let frame = Frame::error("ERR unknown");
        assert_eq!(&frame.encode()[..], b"-ERR unknown\r\n");
    }

    #[test]
    fn test_encode_integer() {
        let frame = Frame::integer(42);
        assert_eq!(&frame.encode()[..], b":42\r\n");
    }

    #[test]
    fn test_encode_bulk_string() {
        let frame = Frame::bulk(Bytes::from_static(b"hello"));
        assert_eq!(&frame.encode()[..], b"$5\r\nhello\r\n");
    }

    #[test]
    fn test_encode_null() {
        let frame = Frame::null();
        assert_eq!(&frame.encode()[..], b"$-1\r\n");
    }

    #[test]
    fn test_encode_array() {
        let frame = Frame::array(vec![
            Frame::bulk(Bytes::from_static(b"foo")),
            Frame::bulk(Bytes::from_static(b"bar")),
        ]);
        assert_eq!(&frame.encode()[..], b"*2\r\n$3\r\nfoo\r\n$3\r\nbar\r\n");
    }
}
