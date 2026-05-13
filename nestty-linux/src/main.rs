mod app;
mod background;
mod gui_client;
mod panel;
mod plugin_panel;
mod search;
mod socket;
mod split;
mod statusbar;
mod tabs;
mod terminal;
mod webview;
mod window;

// service_supervisor + trigger_sink live in `nestty-daemon`; this crate
// imports them via `nestty_daemon::{...}` and `crate::socket` re-exports
// the shared transport types.

fn main() {
    // Default to `warn` so a no-daemon launch is silent on stderr;
    // RUST_LOG=info / debug surfaces gui_client register/reconnect
    // diagnostics and other log:: messages when needed.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("nestty {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    if args.iter().any(|a| a == "--init-config") {
        match nestty_core::config::NesttyConfig::write_default() {
            Ok(path) => {
                println!("Config written to: {}", path.display());
                return;
            }
            Err(e) => {
                eprintln!("Failed to write config: {e}");
                std::process::exit(1);
            }
        }
    }

    if args.iter().any(|a| a == "--config-path") {
        println!(
            "{}",
            nestty_core::config::NesttyConfig::config_path().display()
        );
        return;
    }

    app::run();
}
