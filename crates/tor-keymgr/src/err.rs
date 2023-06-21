//! An error type for the `tor-keymgr` crate.

use tor_error::HasKind;

use dyn_clone::DynClone;

use std::error::Error as StdError;
use std::fmt;

/// An Error type for this crate.
pub type Error = Box<dyn KeystoreError>;

/// An error returned by a [`KeyStore`](crate::KeyStore).
// TODO hs: replace Error with KeyStoreError and create an `ArtiNativeKeyStoreError: KeyStoreError`
// type for ArtiNativeKeyStore.
pub trait KeystoreError:
    HasKind + AsRef<dyn StdError> + DynClone + fmt::Debug + fmt::Display + Send + Sync + 'static
{
}

// Generate a Clone impl for Box<dyn KeystoreError>
dyn_clone::clone_trait_object!(KeystoreError);

impl KeystoreError for tor_error::Bug {}

impl<K: KeystoreError + Send + Sync> From<K> for Error {
    fn from(k: K) -> Self {
        Box::new(k)
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        let e: &dyn StdError = self;
        e.source()
    }
}
