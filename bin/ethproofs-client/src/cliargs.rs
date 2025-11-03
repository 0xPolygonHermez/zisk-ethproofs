use clap::{Parser, ValueEnum};

#[derive(Clone, Debug, ValueEnum, Eq, PartialEq, Hash)]
pub enum TelegramEvent {
    BlockProved,
    SkippedThreshold,
    ProofFailed,
}

// Command line arguments
#[derive(Clone, Parser)]
pub struct CliArgs {
    /// Enable submit proofs to EthProofs
    #[arg(short = 's', long)]
    pub submit_ethproofs: bool,

    /// Send Telegram alerts for specified events
    #[arg(short = 't', long, use_value_delimiter = true, num_args = 1..)]
    pub telegram_alert: Vec<TelegramEvent>,

    /// Insert block proofs into database
    #[arg(short = 'd', long)]
    pub insert_db: bool,

    /// Skip proving step (for testing purposes)
    #[arg(short = 'k', long)]
    pub skip_proving: bool,

    /// Enable Prometheus metrics server
    #[arg(short = 'm', long)]
    pub enable_metrics: bool,

    /// Keep input file
    #[arg(short = 'i', long)]
    pub keep_input: bool,

    /// Blocks skipped alert threshold
    #[arg(short = 'b', long, default_value_t = 5)]
    pub skipped_threshold: u32,

    /// Panic on skipped blocks exceeding threshold
    #[arg(short = 'p', long)]
    pub panic_on_skipped: bool,
}

impl CliArgs {
    pub fn telegram_enabled(&self, event: TelegramEvent) -> bool {
        self.telegram_alert.iter().any(|e| *e == event)
    }
}
