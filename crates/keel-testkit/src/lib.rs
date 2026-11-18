//! Test doubles for keel's transport layer.
//!
//! [`FauxProvider`] is a real HTTP server on a real socket that serves a
//! scripted list of [`Turn`]s. Tests use it instead of hand-rolling a listener
//! per case, and it makes the awkward cases expressible: hold a chunk until the
//! test releases it, stall after the headers, close before the terminal event,
//! answer with a 429, and inspect exactly what the client sent.
//!
//! This crate is a development dependency. It is not part of the shipped
//! binary.

pub mod faux;

pub use faux::{Chunk, FauxProvider, RecordedRequest, Turn};
