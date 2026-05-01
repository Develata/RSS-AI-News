use std::io;

use tracing_subscriber::{EnvFilter, fmt};

#[derive(Debug, Clone)]
pub struct InitOptions {
    pub log_level: String,
    pub log_format: String,
    pub log_file: String,
}

pub fn init(opts: InitOptions) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&opts.log_level));

    if !opts.log_file.trim().is_empty() {
        eprintln!(
            "[observability] log_file persistence not yet implemented; falling back to stderr"
        );
    }

    let builder = fmt().with_env_filter(filter).with_writer(io::stderr);
    if opts.log_format == "json" {
        let _ = builder.json().try_init();
    } else {
        let _ = builder.try_init();
    }
}
