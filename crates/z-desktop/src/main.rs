#![deny(unsafe_code)]

mod app;
mod shared;
mod ui;

fn main() {
    // Opt-in timings are restricted to explicit sync-phase events. Enabling
    // general engine INFO logging could include wallet-specific metadata.
    if std::env::var_os("Z_STACK_SYNC_TIMING").as_deref() == Some(std::ffi::OsStr::new("1")) {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(
                "off,z_stack_sync_timing=info",
            ))
            .with_target(false)
            .try_init();
    }
    let _ = rustls::crypto::ring::default_provider().install_default();
    app::run();
}
