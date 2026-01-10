//! Echo protocol parser.
//!
//! Length-prefixed binary protocol for throughput testing:
//! - `<length>\r\n<data>` → `<length>\r\n<data>`
//! - `QUIT\r\n` → close connection

pub mod parser;
