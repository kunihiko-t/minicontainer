//! MiniContainer UART control-frame boundary.

#![forbid(unsafe_code)]

pub mod decoder;
pub mod error;

pub use decoder::{Decoder, Frame, encode_frame};
pub use error::ProtocolError;
