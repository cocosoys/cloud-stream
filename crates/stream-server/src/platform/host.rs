//! Host identity helpers for the `stream_info` snapshot.
//!
//! `encode_node` surfaces the Windows computer name the panel shows beside the
//! encoder row; the capture backend short name is chosen together with the
//! capturer in `pipeline::capture`.

/// Computer name for `stream_info.encode_node`. Windows `COMPUTERNAME` first,
/// POSIX `HOSTNAME` second; falls back to an empty string rather than calling
/// the deprecated `whoami::hostname()` (it returns `String`, not `Option`).
pub fn node_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_default()
}

/// Short backend label written into `stream_info.capture`. The GDI path is the
/// DXGI recovery fallback, not a separate capturer kind in stage1. Wired to the
/// real backend report in #37; kept available until then.
#[allow(dead_code)]
pub fn capture_backend_name(is_gdi: bool) -> &'static str {
    if is_gdi {
        "gdi"
    } else {
        "dxgi"
    }
}
