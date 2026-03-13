use clap::{Parser, Subcommand, ValueEnum};

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
    Folder,
}

#[derive(Clone, Debug, ValueEnum, Eq, PartialEq, Hash)]
pub enum Hints {
    File,
    Socket,
}

#[derive(Clone, Subcommand, Debug)]
pub enum Commands {
    #[command(hide = true)]
    InputServer,
}

// Command line arguments
#[derive(Clone, Parser)]
pub struct CliArgs {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Input generation method: 'server', 'local' or 'folder'
    #[arg(short = 'n', long, value_enum, default_value_t = InputGen::Server)]
    pub input_gen: InputGen,

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

    /// Enable hints generation
    #[cfg(zisk_hints)]
    #[clap(long, value_enum, default_value_t = Hints::Socket)]
    pub hints: Hints,

    /// Hints socket path (only when using 'socket' hints mode)
    #[cfg(zisk_hints)]
    #[clap(long, default_value = "/tmp/hints.sock", required_if_eq("hints", "Socket"))]
    pub hints_socket: String,

    /// Enable debug hint file generation (only when using 'socket' hints mode)
    #[cfg(zisk_hints)]
    #[clap(long, default_value_t = false, required_if_eq("hints", "Socket"))]
    pub hints_debug: bool,

    /// Hints debug file path (only used if hints_debug is true)
    #[cfg(zisk_hints)]
    #[clap(long, default_value = "./hints_debug", requires = "hints_debug")]
    pub hints_debug_path: String,

    /// Directory to read input files from in local mode (only affects 'folder' mode)
    #[clap(long, default_value = "inputs_queue")]
    pub inputs_queue: String,

    /// Interval in seconds between sending files in local mode (only affects 'folder' mode)
    #[clap(long, default_value = "12")]
    pub interval_secs: u64,

    /// Initial timestamp to use for the first file in local mode (only affects 'folder' mode)
    #[clap(long, default_value = "0")]
    pub initial_timestamp: u64,

    /// Simulated processed time in milliseconds (only affects 'folder' mode)
    #[clap(long, default_value = "0")]
    pub simulated_input_time: u64,
}

impl CliArgs {
    pub fn telegram_enabled(&self, event: TelegramEvent) -> bool {
        self.telegram_alert.iter().any(|e| *e == event)
    }
}
