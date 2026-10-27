//! Upbound command routing and downbound message whitelist (design 5.5 / 5.7).
//!
//! Stage1 boundary: the A-group side effects and outbound JSON are wired in the
//! signaling/webrtc assembly (stage 5.2/5.3, task #35); this module owns the
//! classifier, the builders and the input sink trait.

pub mod clipboard;
pub mod commands;
pub mod input_stub;
pub mod output;
pub mod win_input;

pub use clipboard::{ClipboardSink, read_clipboard_update};
pub use commands::{Action, classify};
pub use input_stub::InputSinkRef;
pub use win_input::WinInputSink;
pub use output::{
    connection, cursor, pipeline, server_settings, stream_info, stream_stats, system, StreamInfo,
    StreamStats,
};
