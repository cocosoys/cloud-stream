//! Platform identity and host metrics for the stage1 stream server.

pub mod host;
pub mod hwenc;
pub mod sysinfo;
pub mod virtual_mic;

pub use host::node_name;
pub use sysinfo::{HostSample, Sampler};
