// `then_some`
#![allow(unstable_name_collisions)]

mod event;
mod inbound_socket;

//
// Exports
//

pub use event::event::{Content, Event, Header, Headers};
pub use event::ty::EventType;
pub use inbound_socket::InboundSocket;

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
