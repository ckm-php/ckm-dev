use ck_core::branding;
use clap::Parser;

/// Manage the local development environment.
#[derive(Parser)]
#[command(name = branding::COMMAND, version)]
struct Cli {}

fn main() {
    Cli::parse();
}
