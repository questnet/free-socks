// `then_some`
#![allow(unstable_name_collisions)]

mod event;
mod hangup_cause;
mod event_socket;
pub mod query;

//
// Exports
//

pub use event::{
    message::{Content, Header, Headers, Message},
    ty::EventType,
    FromMessage,
};
pub use hangup_cause::HangupCause;
pub use event_socket::EventSocket;

//
// Tools
//

const LF: u8 = b'\n';

mod sequence {
    pub fn find_first(all: &[u8], sequence: &[u8]) -> Option<usize> {
        all.windows(sequence.len()).position(|w| *w == *sequence)
    }
}

// nightly features pulled in

pub trait ThenSome {
    fn then_some<T>(self, t: T) -> Option<T>;
}

impl ThenSome for bool {
    fn then_some<T>(self, t: T) -> Option<T> {
        if self {
            Some(t)
        } else {
            None
        }
    }
}
