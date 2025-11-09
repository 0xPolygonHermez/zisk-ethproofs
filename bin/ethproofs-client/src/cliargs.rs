use clap::{Parser, ValueEnum};
use input::GuestProgram;

#[derive(Clone, Debug, ValueEnum, Eq, PartialEq, Hash)]
pub enum TelegramEvent {
    BlockProved,
    SkippedThreshold,
    ProofFailed,
}

#[derive(Clone, Debug, ValueEnum, Eq, PartialEq, Hash)]
pub enum InputGen {
    Server,
    Local,
}

// Command line arguments
#[derive(Clone, Parser)]
pub struct CliArgs {
    /// Input generation method: 'server' or 'local'
    #[arg(short = 'n', long, value_enum, default_value_t = InputGen::Server)]
    pub input_gen: InputGen,

    /// Guest program for which to generate inputs
    #[clap(short = 'g', long, value_enum, default_value_t = GuestProgram::Rsp)]
    pub guest: GuestProgram,

    /// Enable proof submission to Ethproofs
    #[arg(short = 's', long)]
    pub submit_ethproofs: bool,

    /// Send Telegram alerts for specified events
    #[arg(short = 't', long, use_value_delimiter = true, num_args = 1..)]
    pub telegram_alert: Vec<TelegramEvent>,

    /// Insert block proof data into the database
    #[arg(short = 'd', long)]
    pub insert_db: bool,

    /// Skip the proving step (useful for testing)
    #[arg(short = 'k', long)]
    pub skip_proving: bool,

    /// Enable Prometheus metrics server
    #[arg(short = 'm', long)]
    pub enable_metrics: bool,

    /// Keep the input file after processing
    #[arg(short = 'i', long)]
    pub keep_input: bool,

    /// Number of skipped blocks before triggering an alert
    #[arg(short = 'b', long, default_value_t = 5)]
    pub skipped_threshold: u32,

    /// Panic when skipped blocks exceed the threshold
    #[arg(short = 'p', long)]
    pub panic_on_skipped: bool,

    /// Environment config file
    #[arg(short = 'e', long, default_value = ".env")]
    pub env_file: String,
}

impl CliArgs {
    pub fn telegram_enabled(&self, event: TelegramEvent) -> bool {
        self.telegram_alert.iter().any(|e| *e == event)
    }
}
