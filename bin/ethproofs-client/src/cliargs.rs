use clap::Parser;

// Command line arguments
#[derive(Clone, Parser)]
pub struct CliArgs {
    /// Enable submit proofs to ethproofs
    #[arg(short = 's', long)]
    pub submit_ethproofs: bool,

    /// Send telegram alert when block is submitted to ethproofs
    #[arg(short = 'a', long)]
    pub submit_alert: bool,

    /// Keep input file
    #[arg(short = 'i', long)]
    pub keep_input: bool,
}