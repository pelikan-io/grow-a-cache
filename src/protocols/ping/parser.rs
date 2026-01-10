//! Ping protocol parser.

/// Parsed ping command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Simple ping (no message).
    Ping,
    /// Ping with a message to echo back.
    PingMsg(Vec<u8>),
    /// Quit command.
    Quit,
}

/// Parse result.
#[derive(Debug)]
pub enum ParseResult {
    /// Successfully parsed command with bytes consumed.
    Complete(Command, usize),
    /// Need more data.
    Incomplete,
    /// Protocol error (unknown command).
    Error,
}

/// Parse a ping protocol command from the input buffer.
pub fn parse(input: &[u8]) -> ParseResult {
    // Find line ending
    let line_end = match find_crlf(input) {
        Some(pos) => pos,
        None => return ParseResult::Incomplete,
    };

    let line = &input[..line_end];
    let consumed = line_end + 2; // include \r\n

    // Parse command (case-insensitive)
    if line.eq_ignore_ascii_case(b"PING") {
        ParseResult::Complete(Command::Ping, consumed)
    } else if line.eq_ignore_ascii_case(b"QUIT") {
        ParseResult::Complete(Command::Quit, consumed)
    } else if line.len() > 5 && line[..5].eq_ignore_ascii_case(b"PING ") {
        let msg = line[5..].to_vec();
        ParseResult::Complete(Command::PingMsg(msg), consumed)
    } else {
        ParseResult::Error
    }
}

/// Format a PONG response.
pub fn response_pong() -> &'static [u8] {
    b"PONG\r\n"
}

/// Format a PONG response with message.
pub fn response_pong_msg(msg: &[u8], output: &mut [u8]) -> usize {
    let needed = 5 + msg.len() + 2; // "PONG " + msg + "\r\n"
    if output.len() < needed {
        return 0;
    }
    output[..5].copy_from_slice(b"PONG ");
    output[5..5 + msg.len()].copy_from_slice(msg);
    output[5 + msg.len()..needed].copy_from_slice(b"\r\n");
    needed
}

/// Format an error response.
pub fn response_error() -> &'static [u8] {
    b"ERROR unknown command\r\n"
}

/// Find \r\n in buffer, returning the position of \r.
fn find_crlf(buffer: &[u8]) -> Option<usize> {
    (0..buffer.len().saturating_sub(1)).find(|&i| buffer[i] == b'\r' && buffer[i + 1] == b'\n')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ping() {
        match parse(b"PING\r\n") {
            ParseResult::Complete(Command::Ping, 6) => {}
            other => panic!("unexpected: {:?}", other),
        }

        match parse(b"ping\r\n") {
            ParseResult::Complete(Command::Ping, 6) => {}
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_parse_ping_msg() {
        match parse(b"PING hello\r\n") {
            ParseResult::Complete(Command::PingMsg(msg), 12) => {
                assert_eq!(msg, b"hello");
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_parse_quit() {
        match parse(b"QUIT\r\n") {
            ParseResult::Complete(Command::Quit, 6) => {}
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_incomplete() {
        match parse(b"PING") {
            ParseResult::Incomplete => {}
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_unknown_command() {
        match parse(b"FOO\r\n") {
            ParseResult::Error => {}
            other => panic!("unexpected: {:?}", other),
        }
    }
}
