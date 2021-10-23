//! TCP Fast Open (TFO) in Rust for [tokio](https://crates.io/crates/tokio)

mod sys;

pub mod listener;
pub mod stream;

pub use self::{listener::TfoListener, stream::TfoStream};
