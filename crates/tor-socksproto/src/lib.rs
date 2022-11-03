#![cfg_attr(docsrs, feature(doc_auto_cfg, doc_cfg))]
#![doc = include_str!("../README.md")]
// @@ begin lint list maintained by maint/add_warning @@
#![cfg_attr(not(ci_arti_stable), allow(renamed_and_removed_lints))]
#![cfg_attr(not(ci_arti_nightly), allow(unknown_lints))]
#![deny(missing_docs)]
#![warn(noop_method_call)]
#![deny(unreachable_pub)]
#![warn(clippy::all)]
#![deny(clippy::await_holding_lock)]
#![deny(clippy::cargo_common_metadata)]
#![deny(clippy::cast_lossless)]
#![deny(clippy::checked_conversions)]
#![warn(clippy::cognitive_complexity)]
#![deny(clippy::debug_assert_with_mut_call)]
#![deny(clippy::exhaustive_enums)]
#![deny(clippy::exhaustive_structs)]
#![deny(clippy::expl_impl_clone_on_copy)]
#![deny(clippy::fallible_impl_from)]
#![deny(clippy::implicit_clone)]
#![deny(clippy::large_stack_arrays)]
#![warn(clippy::manual_ok_or)]
#![deny(clippy::missing_docs_in_private_items)]
#![deny(clippy::missing_panics_doc)]
#![warn(clippy::needless_borrow)]
#![warn(clippy::needless_pass_by_value)]
#![warn(clippy::option_option)]
#![warn(clippy::rc_buffer)]
#![deny(clippy::ref_option_ref)]
#![warn(clippy::semicolon_if_nothing_returned)]
#![warn(clippy::trait_duplication_in_bounds)]
#![deny(clippy::unnecessary_wraps)]
#![warn(clippy::unseparated_literal_suffix)]
#![deny(clippy::unwrap_used)]
#![allow(clippy::let_unit_value)] // This can reasonably be done for explicitness
#![allow(clippy::significant_drop_in_scrutinee)] // arti/-/merge_requests/588/#note_2812945
#![allow(clippy::result_large_err)] // temporary workaround for arti#587
//! <!-- @@ end lint list maintained by maint/add_warning @@ -->

mod err;
mod handshake;
mod msg;

pub use err::Error;
pub use handshake::Action;

#[cfg(feature = "proxy-handshake")]
#[cfg_attr(docsrs, doc(cfg(feature = "proxy-handshake")))]
pub use handshake::proxy::SocksProxyHandshake;

#[cfg(feature = "client-handshake")]
#[cfg_attr(docsrs, doc(cfg(feature = "client-handshake")))]
pub use handshake::client::SocksClientHandshake;

#[deprecated(since = "0.5.2", note = "Use SocksProxyHandshake instead.")]
#[cfg(feature = "proxy-handshake")]
#[cfg_attr(docsrs, doc(cfg(feature = "proxy-handshake")))]
pub use SocksProxyHandshake as SocksHandshake;

pub use msg::{
    SocksAddr, SocksAuth, SocksCmd, SocksReply, SocksRequest, SocksStatus, SocksVersion,
};
pub use tor_error::Truncated;

/// A Result type for the tor_socksproto crate.
pub type Result<T> = std::result::Result<T, Error>;

/// A Result type for the tor_socksproto crate, including the possibility of a
/// truncated message.
///
/// This is a separate type from Result because a truncated message is not a
/// true error: it just means that you need to read more bytes and try again.
pub type TResult<T> = std::result::Result<Result<T>, Truncated>;
