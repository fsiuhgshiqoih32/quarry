//! The command-line interface as a console program. On Windows `quarry.exe` is a GUI app, so
//! terminals don't wait for it; scripts there should call `quarry-cli.exe` instead.

use clap::Parser;

fn main() {
    let args = quarry::cli::Cli::parse();
    let code = match args.command {
        Some(cmd) => quarry::cli::run(cmd),
        None => {
            eprintln!("quarry-cli: give a command, for example `quarry-cli search report`; see --help");
            2
        }
    };
    std::process::exit(code);
}
