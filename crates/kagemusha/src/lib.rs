//! Internals of the `kagemusha` binary, the PID 1 agent for AWS Lambda
//! MicroVMs. The modules are public so the binary and its integration
//! tests can drive them; they are not a stable library API and may change
//! in any release without a semver-major bump. Run the binary instead —
//! see the README.

#[cfg(not(unix))]
compile_error!("kagemusha supports unix targets only (Linux in production, macOS for development)");

pub mod config;
pub mod ctx;
pub mod hooks;
pub(crate) mod hooksdir;
pub(crate) mod identity;
pub mod meter;
pub mod supervisor;
pub mod telemetry;
#[cfg(test)]
mod test_support;
pub mod types;
