use clap::Parser;

// Command line arguments
#[derive(Clone, Parser)]
pub struct CliArgs {
    /// Enable submit proofs to EthProofs
    #[arg(short = 's', long)]
    pub submit_ethproofs: bool,

    /// Send telegram alert when block is submitted to EthProofs
    #[arg(short = 'a', long)]
    pub submit_alert: bool,

    /// Insert block proofs into database
    #[arg(short = 'd', long)]
    pub insert_db: bool,

    /// Keep input file
    #[arg(short = 'i', long)]
    pub keep_input: bool,
}
