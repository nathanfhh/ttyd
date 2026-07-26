//! ttyd — share your terminal over the web.
//!
//! A Rust port of the original C implementation, keeping the wire protocol, command line
//! interface and observable HTTP behaviour compatible, and adding forward authentication.

pub mod auth;
pub mod cli;
pub mod html;
pub mod http;
pub mod jsonc;
pub mod protocol;
pub mod pty;
pub mod serve;
pub mod state;
pub mod utils;
pub mod ws;
