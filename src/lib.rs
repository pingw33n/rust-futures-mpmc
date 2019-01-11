extern crate futures;

use std::fmt;

pub mod array;
mod util;

pub use array::array;

pub struct SendError<T>(T);

impl<T> fmt::Debug for SendError<T> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_tuple("SendError")
            .field(&"...")
            .finish()
    }
}

impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "send failed because all receivers are gone")
    }
}

pub struct TrySendError<T>(TrySendErrorKind<T>);

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum TrySendErrorKind<T> {
    Full(T),
    Disconnected(T),
}

impl<T> TrySendError<T> {
    pub fn into_inner(self) -> T {
        use self::TrySendErrorKind::*;
        match self.0 {
            | Full(v)
            | Disconnected(v) => v,
        }
    }
}

impl<T> fmt::Debug for TrySendError<T> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_tuple("TrySendError")
            .field(&"...")
            .finish()
    }
}

impl<T> fmt::Display for TrySendError<T> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        use self::TrySendErrorKind::*;
        match &self.0 {
            Full(_) => write!(fmt, "send failed because channel is full"),
            Disconnected(_) => write!(fmt, "send failed because all receivers are gone"),
        }
    }
}