//! Session loop, route resolution and event assembly.
//!
//! Dependency rule: this is the top of the provider stack. Types, protocols and
//! routes flow upward into it; nothing here leaks into them.

pub mod assemble;
pub mod builder;
pub mod session;

pub use assemble::{Assembler, Turn};
pub use builder::build_route;
pub use session::{Session, StepOutcome};
