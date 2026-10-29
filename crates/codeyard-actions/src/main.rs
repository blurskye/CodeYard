//! CLI entry point following actions/runner's `Runner.Listener/Runner.cs`.
use clap::Parser;
use codeyard_actions::runner::{run, Options};

#[tokio::main]
async fn main() {
    let options = Options::parse();
    match run(options).await {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(error) => {
            eprintln!("codeyard-actions: {error:#}");
            std::process::exit(if error.is::<codeyard_actions::runner::Cancelled>() {
                codeyard_actions::runner::CANCELLED_EXIT_CODE
            } else { 1 });
        }
    }
}
