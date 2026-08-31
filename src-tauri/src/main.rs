// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if scribe_lib::cli::is_cli_invocation(&args) {
        std::process::exit(
            if scribe_lib::cli::run(args) == std::process::ExitCode::SUCCESS {
                0
            } else {
                1
            },
        );
    }
    scribe_lib::run();
}
