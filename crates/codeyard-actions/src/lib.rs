//! Runs GitHub Actions workflows on one machine, the way `act` does, so Codeyard can start it
//! as a separate process. Three parts:
//!
//! - [`expressions`]: the `${{ }}` language, its contexts and functions.
//! - [`workflow`]: workflow files, the order of their jobs, and turning a job into the runs
//!   its matrix makes.
//! - [`runner`]: running those jobs and their steps, on the host or in Podman containers.
pub mod command_codec;
pub mod expressions;
pub mod fs;
pub mod progress;
pub mod process;
pub mod runner;
pub mod values;
pub mod vm;
pub mod workflow;
