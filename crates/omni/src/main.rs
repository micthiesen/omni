//! `omni`: see [`omni::cli`] for the command line.

use std::io::Write as _;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match omni::cli::parse(&args, |key| std::env::var(key).ok()) {
        Ok(command) => command,
        Err(error) => {
            let _ = write!(std::io::stderr().lock(), "{error}\n{}", omni::cli::USAGE);
            return ExitCode::from(1);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = writeln!(std::io::stderr().lock(), "tokio runtime: {error}");
            return ExitCode::from(1);
        }
    };
    ExitCode::from(runtime.block_on(omni::app::main(command)))
}
