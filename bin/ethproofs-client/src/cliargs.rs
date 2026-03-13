use clap::{Args, Parser, Subcommand, ValueEnum};

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
pub enum HintsMode {
    File,
    Socket,
}

#[derive(Clone, Subcommand, Debug)]
pub enum Commands {
    #[command(hide = true)]
    InputServer,
}

#[derive(Clone, Debug, Args)]
#[command(next_help_heading = "Folder")]
pub struct FolderArgs {
    /// Directory to read input files from in folder input generation mode
    #[arg(long = "folder.path", default_value = "inputs_queue")]
    pub path: String,

    /// Interval in seconds between sending input files in folder input generation mode
    #[arg(long = "folder.interval", default_value = "12")]
    pub interval: u64,

    /// Initial timestamp to use for the first input file in folder input generation mode
    #[arg(long = "folder.initial-timestamp", default_value = "0")]
    pub initial_timestamp: u64,

    /// Simulated input processed time in milliseconds in folder input generation mode
    #[arg(long = "folder.input-time", default_value = "0")]
    pub input_time: u64,
}

#[derive(Clone, Debug, Args)]
#[command(next_help_heading = "Skipped blocks")]
pub struct SkippedArgs {
    /// Number of skipped blocks before triggering an alert
    #[arg(long = "skipped.threshold", default_value = "5")]
    pub threshold: u32,

    /// Panic when skipped blocks exceed the threshold
    #[arg(long = "skipped.panic", default_value = "false")]
    pub panic: bool,
}

#[cfg(zisk_hints)]
#[derive(Clone, Debug, Args)]
#[command(next_help_heading = "Hints")]
pub struct HintsArgs {
    /// Hints generation mode: 'file' or 'socket'
    #[arg(long = "hints.mode", value_enum, default_value_t = HintsMode::Socket)]
    pub mode: HintsMode,

    /// Hints socket path (only when using 'socket' hints mode)
    #[arg(
        long = "hints.socket",
        default_value = "/tmp/hints.sock",
        required_if_eq("hints.mode", "Socket")
    )]
    pub socket: String,

    /// Enable debug hint file generation (only when using 'socket' hints mode)
    #[arg(long = "hints.debug", default_value_t = false, required_if_eq("hints.mode", "Socket"))]
    pub debug: bool,

    /// Hints debug file path (only used if hints.debug is true)
    #[arg(long = "hints.debug-path", default_value = "./hints_debug", requires = "hints.debug")]
    pub debug_path: String,
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
    pub metrics: bool,

    /// Keep the input file after processing
    #[arg(short = 'i', long)]
    pub keep_input: bool,

    /// Environment config file
    #[arg(short = 'e', long, default_value = ".env")]
    pub env_file: String,

    #[command(flatten)]
    pub skipped: SkippedArgs,

    #[command(flatten)]
    pub folder: FolderArgs,

    #[cfg(zisk_hints)]
    #[command(flatten)]
    pub hints: HintsArgs,
}

impl CliArgs {
    pub fn telegram_enabled(&self, event: TelegramEvent) -> bool {
        self.telegram_alert.iter().any(|e| *e == event)
    }
}
