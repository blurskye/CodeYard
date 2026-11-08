//! CLI entry point following actions/runner's `Runner.Listener/Runner.cs`.
use clap::Parser;
use codeyard_runner::runner::{run, Options};

fn main() {
    let options = Options::parse();
    // Composite actions nest up to twenty levels, and each level polls the next on the same
    // stack. Unoptimized builds need more than Tokio's 2 MiB worker stack for that.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(32 * 1024 * 1024)
        .build();
    let runtime = match runtime {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("codeyard-actions: cannot start the async runtime: {error}");
            std::process::exit(1);
        }
    };
    match runtime.block_on(run(options)) {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(error) => {
            eprintln!("codeyard-actions: {error:#}");
            std::process::exit(if error.is::<codeyard_runner::runner::Cancelled>() {
                codeyard_runner::runner::CANCELLED_EXIT_CODE
            } else {
                1
            });
        }
    }
}
