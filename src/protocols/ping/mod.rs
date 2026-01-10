//! Ping protocol parser.
//!
//! Simple line-based protocol for latency testing:
//! - `PING\r\n` → `PONG\r\n`
//! - `PING <msg>\r\n` → `PONG <msg>\r\n`
//! - `QUIT\r\n` → close connection

pub mod parser;
