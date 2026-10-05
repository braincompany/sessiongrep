mod cli;
mod tui;

fn main() {
    restore_default_sigpipe();
    if let Err(err) = cli::run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

/// Rust ignores SIGPIPE, so printing into a closed pipe (`sessiongrep list | head`)
/// panics. Restore the default so the process exits quietly, like other CLIs.
#[cfg(unix)]
fn restore_default_sigpipe() {
    // SAFETY: runs first thing in main, before any other threads exist.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn restore_default_sigpipe() {}
