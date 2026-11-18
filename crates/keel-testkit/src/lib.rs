//! Test doubles for keel's transport layer.
//!
//! [`FauxProvider`] is a real HTTP server on a real socket that serves a
//! scripted list of [`Turn`]s. Tests use it instead of hand-rolling a listener
//! per case, and it makes the awkward cases expressible: hold a chunk until the
//! test releases it, stall after the headers, close before the terminal event,
//! answer with a 429, and inspect exactly what the client sent.
//!
//! [`Cassette`] replays a recorded exchange: the exact request keel sent and
//! the raw response it got. [`Recorder`] produces one from a local proxy.
//!
//! This crate is a development dependency. It is not part of the shipped
//! binary.

pub mod cassette;
pub mod faux;
pub mod record;

pub use cassette::{
    CASSETTE_VERSION, Cassette, CassetteBuilder, CassetteError, CassetteMode, CassetteRequest,
    CassetteResponse, CassetteServer, Interaction, Miss,
};
pub use faux::{Chunk, FauxProvider, RecordedRequest, Turn};
pub use record::{Recorder, cassette_mode, is_loopback, recording_mode};
