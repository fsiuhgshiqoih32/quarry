// Release builds on Windows are GUI apps (no console window). `quarry-cli.exe` is the console twin
// for scripts; `quarry.exe search ...` still works from a terminal by attaching to it.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod gui;

use clap::Parser;
use quarry::cli::{self, Cli, Command};

fn gui(launch: gui::Launch) -> i32 {
    match gui::run(launch) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("quarry: {e}");
            1
        }
    }
}

fn main() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
    let args = Cli::parse();
    let code = match args.command {
        None => gui(gui::Launch { hidden: args.hidden, query: args.query, smoke_test: false }),
        Some(Command::SelfTest { gui: true }) => match cli::run(Command::SelfTest { gui: true }) {
            0 => gui(gui::Launch { hidden: false, query: None, smoke_test: true }),
            failed => failed,
        },
        Some(cmd) => cli::run(cmd),
    };
    std::process::exit(code);
}
