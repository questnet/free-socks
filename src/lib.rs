// `then_some`
#![allow(unstable_name_collisions)]

mod event;
mod event_socket;
mod hangup_cause;
pub mod query;

//
// Exports
//

pub use event::{
    message::{Content, Header, Headers, Message},
    ty::EventType,
    FromMessage,
};
pub use event_socket::EventSocket;
pub use hangup_cause::HangupCause;

//
// Tools
//

const LF: u8 = b'\n';

mod sequence {
    pub fn find_first(all: &[u8], sequence: &[u8]) -> Option<usize> {
        all.windows(sequence.len()).position(|w| *w == *sequence)
    }
}
