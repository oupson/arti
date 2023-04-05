#![doc = include_str!("../README.md")]
// @@ begin lint list maintained by maint/add_warning @@
// I'll run add_warning before we merge XXXX
//! <!-- @@ end lint list maintained by maint/add_warning @@ -->

#![allow(dead_code)] // XXXX

mod cancel;
mod msgs;
mod session;
mod streams;

#[cfg(feature = "tokio")]
pub mod listen;
