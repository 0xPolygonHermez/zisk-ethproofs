use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Clone, Debug, ValueEnum, Eq, PartialEq, Hash)]
pub enum TelegramEvent {
    BlockProved,
    SkippedThreshold,
    ProofFailed,
}

#[derive(Clone, Debug, ValueEnum, Eq, PartialEq, Hash)]
pub enum InputGen {
    Rpc,
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
#[command(next_help_heading = "Inputs generation")]
pub struct InputsArgs {
    /// Input generation mode: 'rpc' or 'folder'
    #[arg(short = 'n', long = "input.mode", value_enum, default_value_t = InputGen::Rpc)]
    pub mode: InputGen,

    /// Modulus to apply to select blocks in rpc input generation mode
    #[arg(long = "input.block-modulus", default_value_t = 1, required_if_eq("mode", "Rpc"))]
    pub block_modulus: u64,

    /// Folder to store generated input files in rpc input generation mode
    #[arg(long = "input.folder", default_value = "inputs", requires = "mode")]
    pub folder: String,

    /// Keep generated input files after processing them
    #[arg(short = 'i', long = "input.keep", default_value_t = false, requires = "mode")]
    pub keep: bool,
}

#[derive(Clone, Debug, Args)]
#[command(next_help_heading = "Input RPC generation")]
pub struct RpcArgs {
    /// HTTP URL for Ethereum node RPC connection
    #[arg(long = "rpc.http-url", default_value = "http://localhost:8545")]
    pub http_url: String,

    /// Websocket URL for Ethereum node RPC connection
    #[arg(long = "rpc.ws-url", default_value = "ws://localhost:8546")]
    pub ws_url: String,
}

#[derive(Clone, Debug, Args)]
#[command(next_help_heading = "Input folder generation")]
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
#[command(next_help_heading = "ZisK coordinator")]
pub struct CoordinatorArgs {
    /// ZisK coordinator URL
    #[arg(long = "coordinator.url", default_value = "http://localhost:50051")]
    pub url: String,

    /// Compute capacity
    #[arg(short = 'c', long = "coordinator.compute-capacity", default_value = "10")]
    pub compute_capacity: u32,
}

#[derive(Clone, Debug, Args)]
#[command(next_help_heading = "Ethproofs submission")]
pub struct EthproofsArgs {
    /// Enable submission of proofs to Ethproofs
    #[arg(
        long = "ethproofs.submit",
        requires_all = ["api_url", "api_token", "cluster_id"]
    )]
    pub submit: bool,

    /// Ethproofs API URL
    #[arg(long = "ethproofs.api-url", requires = "submit")]
    pub api_url: Option<String>,

    /// Ethproofs API token
    #[arg(long = "ethproofs.api-token", requires = "submit")]
    pub api_token: Option<String>,

    /// Ethproofs cluster ID
    #[arg(long = "ethproofs.cluster-id", requires = "submit")]
    pub cluster_id: Option<u32>,
}

#[derive(Clone, Debug, Args)]
#[command(next_help_heading = "Telegram alerts")]
pub struct TelegramArgs {
    /// Telegram events to send alerts for (can specify multiple)
    #[arg(
        long = "telegram.alert",
        value_enum,
        use_value_delimiter = true,
        num_args = 1..,
        requires_all = ["bot_token", "chat_id"]
    )]
    pub alert: Vec<TelegramEvent>,

    /// Telegram bot token for sending alerts
    #[arg(long = "telegram.bot-token", requires = "alert")]
    pub bot_token: Option<String>,

    /// Telegram chat ID for sending alerts
    #[arg(long = "telegram.chat-id", requires = "alert")]
    pub chat_id: Option<String>,

    /// Prefix for Telegram alert messages
    #[arg(long = "telegram.message-prefix", default_value = "[EthProofs Client Alert]")]
    pub message_prefix: String,
}

#[derive(Clone, Debug, Args)]
#[command(next_help_heading = "Skipped blocks alerts")]
pub struct SkippedArgs {
    /// Number of skipped blocks before triggering an alert
    #[arg(long = "skipped.threshold", default_value = "5")]
    pub threshold: u32,

    /// Panic when skipped blocks exceed the threshold
    #[arg(long = "skipped.panic", default_value = "false")]
    pub panic: bool,
}

#[derive(Clone, Debug, Args)]
#[command(next_help_heading = "Metrics")]
pub struct MetricsArgs {
    /// Enable Prometheus metrics server
    #[arg(short = 'm', long = "metrics.enabled")]
    pub enabled: bool,

    /// Port for Prometheus metrics server
    #[arg(long = "metrics.port", default_value_t = 8384, requires = "enabled")]
    pub port: u16,
}

#[derive(Clone, Debug, Args)]
#[command(next_help_heading = "Database")]
pub struct DbArgs {
    /// Enable insertion of block proof data into a database
    #[arg(
        long = "db.enabled",
        requires_all = ["dsn", "hardware", "zisk_version"]
    )]
    pub enabled: bool,

    /// Database connection string (DSN) for inserting block proof data
    #[arg(long = "db.dsn", requires = "enabled")]
    pub dsn: Option<String>,

    /// Hardware information to include in DB entries
    #[arg(long = "db.hardware", requires = "enabled")]
    pub hardware: Option<String>,

    /// ZisK version to include in DB entries
    #[arg(long = "db.zisk-version", requires = "enabled")]
    pub zisk_version: Option<String>,
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
#[command(next_line_help = true)]
pub struct CliArgs {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Skip the proving step (useful for testing)
    #[arg(short = 'k', long)]
    pub skip_proving: bool,

    /// Webhook server listen port
    #[arg(long, default_value_t = 8051)]
    pub webhook_port: u16,

    #[command(flatten)]
    pub inputs: InputsArgs,

    #[command(flatten)]
    pub rpc: RpcArgs,

    #[command(flatten)]
    pub folder: FolderArgs,

    #[command(flatten)]
    pub coordinator: CoordinatorArgs,

    #[command(flatten)]
    pub ethproofs: EthproofsArgs,

    #[command(flatten)]
    pub telegram: TelegramArgs,

    #[command(flatten)]
    pub skipped: SkippedArgs,

    #[command(flatten)]
    pub metrics: MetricsArgs,

    #[command(flatten)]
    pub db: DbArgs,

    #[cfg(zisk_hints)]
    #[command(flatten)]
    pub hints: HintsArgs,
}

impl CliArgs {
    pub fn telegram_enabled(&self, event: TelegramEvent) -> bool {
        self.telegram.alert.iter().any(|e| *e == event)
    }
}
