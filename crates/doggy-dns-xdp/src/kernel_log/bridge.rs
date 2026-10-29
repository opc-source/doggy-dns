//! Userspace side of `aya-log`: turns kernel records into `tracing` events.
//!
//! Linux-only: the parent module gates this file because it depends on
//! `aya-log`, which is built against `aya`.

use aya_log::EbpfLogger;
use log::{Level, Log, Metadata, Record};

use super::resolve_level;

/// Bridges the `log` records that [`EbpfLogger`] reads out of the kernel ring
/// buffer into `tracing`, so kernel-side events land in the same pipeline as
/// the rest of the server's logs.
///
/// The eBPF program is already level-filtered in the kernel via the baked
/// `AYA_LOG_LEVEL` global, so every record that arrives here is forwarded.
pub struct TracingLog;

impl Log for TracingLog {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        // `tracing` needs a compile-time level and target, so the level is
        // matched here and the kernel-side module path travels in the message.
        // Records are rare: the eBPF program only logs on failure paths.
        let target = record.target();
        let message = record.args();
        match record.level() {
            Level::Error => {
                tracing::error!(target: "doggy_dns_xdp::kernel", "{target}: {message}")
            }
            Level::Warn => tracing::warn!(target: "doggy_dns_xdp::kernel", "{target}: {message}"),
            Level::Info => tracing::info!(target: "doggy_dns_xdp::kernel", "{target}: {message}"),
            Level::Debug => tracing::debug!(target: "doggy_dns_xdp::kernel", "{target}: {message}"),
            Level::Trace => tracing::trace!(target: "doggy_dns_xdp::kernel", "{target}: {message}"),
        }
    }

    fn flush(&self) {}
}

/// The kernel log consumer handed out by [`crate::program::XdpManager::take_log`].
///
/// Named alias so the bridge stays an implementation detail on the public
/// surface.
pub type XdpKernelLogger = EbpfLogger<TracingLog>;

/// Kernel-side verbosity, selected before the ELF is loaded and baked into
/// the program's `AYA_LOG_LEVEL` global.
pub(crate) fn kernel_log_level() -> u8 {
    let raw = std::env::var(aya_log::LEVEL).ok();
    let (level, supported) = resolve_level(raw.as_deref());
    if !supported {
        tracing::warn!(
            value = raw.as_deref().unwrap_or_default(),
            "unsupported AYA_LOG_LEVEL value, clamping to warn"
        );
    }
    level.as_u8()
}

/// Drain kernel-side records into `tracing` on the current runtime.
///
/// The task lives as long as the ring buffer: `AsyncFd` owns the logger, and
/// dropping the runtime cancels the task, so no explicit stop signal is
/// needed. Records are forwarded synchronously from a runtime task because
/// [`EbpfLogger::flush`] only decodes what is already in the ring.
///
/// # Panics
///
/// Panics when called outside a Tokio runtime, like [`tokio::spawn`].
pub fn spawn_log_flush(logger: XdpKernelLogger) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // SAFETY: `EbpfLogger` owns the ring-buffer fd for the whole lifetime
        // of the `AsyncFd`, never swaps the descriptor, and `as_raw_fd` keeps
        // returning that same fd, which satisfies `register_with_interest`.
        let mut async_fd = match unsafe {
            tokio::io::unix::AsyncFd::register_with_interest(logger, tokio::io::Interest::READABLE)
        } {
            Ok(fd) => fd,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "failed to register the XDP log ring buffer, kernel logs disabled"
                );
                return;
            }
        };
        loop {
            let mut guard = match async_fd.readable_mut().await {
                Ok(guard) => guard,
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "XDP log ring buffer read failed, stopping the kernel log drain"
                    );
                    return;
                }
            };
            guard.get_inner_mut().flush();
            guard.clear_ready();
        }
    })
}
