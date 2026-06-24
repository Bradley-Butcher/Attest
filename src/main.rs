use clap::Parser;

fn main() -> anyhow::Result<()> {
    attest_contracts::run(attest_contracts::Cli::parse())
}
