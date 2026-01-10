//! Echo protocol parser.

/// Parsed echo command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Echo data back (header consumed, data follows).
    Echo {
        /// Length of data to echo.
        length: usize,
        /// Bytes consumed by the header (length + \r\n).
        header_len: usize,
    },
    /// Quit command.
    Quit,
}

/// Parse result.
#[derive(Debug)]
pub enum ParseResult {
    /// Successfully parsed command.
    Complete(Command),
    /// Need more data for header.
    Incomplete,
    /// Invalid length format.
    InvalidLength,
}

/// Parse an echo protocol command from the input buffer.
///
/// Returns the parsed command. For Echo commands, the caller must ensure
/// sufficient data is available before processing.
pub fn parse(input: &[u8]) -> ParseResult {
    // Find line ending for the length prefix
    let line_end = match find_crlf(input) {
        Some(pos) => pos,
        None => return ParseResult::Incomplete,
    };

    let line = &input[..line_end];

    // Check for QUIT
    if line.eq_ignore_ascii_case(b"QUIT") {
        return ParseResult::Complete(Command::Quit);
    }

    // Parse length
    let length_str = match std::str::from_utf8(line) {
        Ok(s) => s,
        Err(_) => return ParseResult::InvalidLength,
    };

    let length: usize = match length_str.parse() {
        Ok(len) => len,
        Err(_) => return ParseResult::InvalidLength,
    };

    let header_len = line_end + 2; // length line + \r\n

    ParseResult::Complete(Command::Echo { length, header_len })
}

/// Format an echo response header.
pub fn response_header(length: usize, output: &mut [u8]) -> usize {
    let header = format!("{length}\r\n");
    let bytes = header.as_bytes();
    if output.len() < bytes.len() {
        return 0;
    }
    output[..bytes.len()].copy_from_slice(bytes);
    bytes.len()
}

/// Format an error response.
pub fn response_error(msg: &str) -> Vec<u8> {
    format!("ERROR {msg}\r\n").into_bytes()
}

/// Find \r\n in buffer, returning the position of \r.
fn find_crlf(buffer: &[u8]) -> Option<usize> {
    (0..buffer.len().saturating_sub(1)).find(|&i| buffer[i] == b'\r' && buffer[i + 1] == b'\n')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_echo() {
        match parse(b"100\r\n") {
            ParseResult::Complete(Command::Echo { length, header_len }) => {
                assert_eq!(length, 100);
                assert_eq!(header_len, 5);
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_parse_quit() {
        match parse(b"QUIT\r\n") {
            ParseResult::Complete(Command::Quit) => {}
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_incomplete() {
        match parse(b"100") {
            ParseResult::Incomplete => {}
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_invalid_length() {
        match parse(b"abc\r\n") {
            ParseResult::InvalidLength => {}
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_response_header() {
        let mut buf = [0u8; 20];
        let len = response_header(12345, &mut buf);
        assert_eq!(&buf[..len], b"12345\r\n");
    }
}
