use clap::error::ErrorKind;
use clap::{Args, CommandFactory, Error, Parser, Subcommand, ValueEnum};

#[derive(Clone, Debug, ValueEnum, Eq, PartialEq, Hash)]
pub enum TelegramEvent {
    Started,
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
    #[arg(
        id = "input_mode",
        short = 'n',
        long = "input.mode",
        value_enum,
        default_value_t = InputGen::Rpc
    )]
    pub mode: InputGen,

    /// Modulus to apply to select blocks in rpc input generation mode
    #[arg(id = "input_block_modulus", long = "input.block-modulus", default_value_t = 1)]
    pub block_modulus: u64,

    /// Folder to store generated input files
    #[arg(id = "input_folder", long = "input.folder", default_value = "inputs")]
    pub folder: String,

    /// Keep generated input files after processing them
    #[arg(id = "input_keep", short = 'i', long = "input.keep", default_value_t = false)]
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
    #[arg(id = "folder_path", long = "folder.path", default_value = "inputs_test")]
    pub path: String,

    /// Initial timestamp to use for the first input file in folder input generation mode
    #[arg(id = "folder_initial_timestamp", long = "folder.initial-timestamp", default_value = "0")]
    pub initial_timestamp: u64,

    /// Simulated input processed time in milliseconds in folder input generation mode
    #[arg(id = "folder_input_time", long = "folder.input-time", default_value = "0")]
    pub input_time: u64,

    /// Comma-separated list of input file names to process in folder input generation mode
    #[arg(
        id = "folder_input_files",
        long = "folder.input-files",
        use_value_delimiter = true,
        num_args = 1..
    )]
    pub input_files: Vec<String>,
}

#[derive(Clone, Debug, Args)]
#[command(next_help_heading = "Ethproofs submission")]
pub struct EthproofsArgs {
    /// Enable submission of proofs to Ethproofs
    #[arg(
        id = "ethproofs_submit",
        short = 's',
        long = "ethproofs.submit",
        requires_all = ["ethproofs_api_url", "ethproofs_api_token", "ethproofs_cluster_id"]
    )]
    pub submit: bool,

    /// Ethproofs API URL
    #[arg(id = "ethproofs_api_url", long = "ethproofs.api-url", requires = "ethproofs_submit")]
    pub api_url: Option<String>,

    /// Ethproofs API token
    #[arg(id = "ethproofs_api_token", long = "ethproofs.api-token", requires = "ethproofs_submit")]
    pub api_token: Option<String>,

    /// Ethproofs cluster ID
    #[arg(
        id = "ethproofs_cluster_id",
        long = "ethproofs.cluster-id",
        requires = "ethproofs_submit"
    )]
    pub cluster_id: Option<u32>,
}

#[derive(Clone, Debug, Args)]
#[command(next_help_heading = "Telegram alerts")]
pub struct TelegramArgs {
    /// Telegram events to send alerts for (can specify multiple)
    #[arg(
        id = "telegram_alert",
        short = 'a',
        long = "telegram.alert",
        value_enum,
        use_value_delimiter = true,
        num_args = 1..,
        requires_all = ["telegram_bot_token", "telegram_chat_id"]
    )]
    pub alert: Vec<TelegramEvent>,

    /// Telegram bot token for sending alerts
    #[arg(id = "telegram_bot_token", long = "telegram.bot-token", requires = "telegram_alert")]
    pub bot_token: Option<String>,

    /// Telegram chat ID for sending alerts
    #[arg(id = "telegram_chat_id", long = "telegram.chat-id", requires = "telegram_alert")]
    pub chat_id: Option<String>,

    /// Prefix for Telegram alert messages
    #[arg(
        id = "telegram_message_prefix",
        long = "telegram.message-prefix",
        default_value = "[EthProofs Client Alert]"
    )]
    pub message_prefix: String,
}

#[derive(Clone, Debug, Args)]
#[command(next_help_heading = "Skipped blocks alerts")]
pub struct SkippedArgs {
    /// Number of skipped blocks before triggering an alert
    #[arg(id = "skipped_threshold", long = "skipped.threshold", default_value = "5")]
    pub threshold: u32,

    /// Panic when skipped blocks exceed the threshold
    #[arg(id = "skipped_panic", long = "skipped.panic", default_value = "false")]
    pub panic: bool,
}

#[derive(Clone, Debug, Args)]
#[command(next_help_heading = "Metrics")]
pub struct MetricsArgs {
    /// Enable Prometheus metrics server
    #[arg(id = "metrics_enabled", short = 'm', long = "metrics.enabled")]
    pub enabled: bool,

    /// Port for Prometheus metrics server
    #[arg(
        id = "metrics_port",
        long = "metrics.port",
        default_value_t = 8384,
        requires = "metrics_enabled"
    )]
    pub port: u16,
}

#[derive(Clone, Debug, Args)]
#[command(next_help_heading = "Database")]
pub struct DbArgs {
    /// Enable insertion of block proof data into a database
    #[arg(
        id = "db_enabled",
        long = "db.enabled",
        requires_all = ["db_dsn", "db_hardware", "db_zisk_version"]
    )]
    pub enabled: bool,

    /// Database connection string (DSN) for inserting block proof data
    #[arg(id = "db_dsn", long = "db.dsn", requires = "db_enabled")]
    pub dsn: Option<String>,

    /// Hardware information to include in DB entries
    #[arg(id = "db_hardware", long = "db.hardware", requires = "db_enabled")]
    pub hardware: Option<String>,

    /// ZisK version to include in DB entries
    #[arg(id = "db_zisk_version", long = "db.zisk-version", requires = "db_enabled")]
    pub zisk_version: Option<String>,
}

#[derive(Clone, Debug, Args)]
#[command(next_help_heading = "Hooks")]
pub struct HooksArgs {
    /// Path to a shell script to execute when proof generation fails
    #[arg(id = "hooks_proof_failed", long = "hooks.proof-failed")]
    pub proof_failed: Option<String>,

    /// Path to a shell script to execute when blocks are skipped over the threshold
    #[arg(id = "hooks_blocks_skipped", long = "hooks.blocks-skipped")]
    pub blocks_skipped: Option<String>,
}

#[derive(Clone, Debug, Args)]
#[command(next_help_heading = "Proof storage")]
pub struct ProofArgs {
    /// Save the generated proof to disk
    #[arg(id = "proof_save", long = "proof.save", default_value_t = false)]
    pub save: bool,

    /// Directory where proofs are saved (only used if proof.save is true)
    #[arg(
        id = "proof_folder",
        long = "proof.folder",
        default_value = "proofs",
        requires = "proof_save"
    )]
    pub folder: String,
}

#[cfg(zisk_hints)]
#[derive(Clone, Debug, Args)]
#[command(next_help_heading = "Hints")]
pub struct HintsArgs {
    /// Hints generation mode: 'file' or 'socket'
    #[arg(
        id = "hints_mode",
        long = "hints.mode",
        value_enum,
        default_value_t = HintsMode::Socket
    )]
    pub mode: HintsMode,

    /// Hints socket path (only when using 'socket' hints mode)
    #[arg(
        id = "hints_socket",
        long = "hints.socket",
        default_value = "/tmp/hints.sock",
        required_if_eq("hints_mode", "Socket")
    )]
    pub socket: String,

    /// Enable debug hint file generation (only when using 'socket' hints mode)
    #[arg(
        id = "hints_debug",
        long = "hints.debug",
        default_value_t = false,
        required_if_eq("hints_mode", "Socket")
    )]
    pub debug: bool,

    /// Hints debug folder path (only used if hints.debug is true)
    #[arg(
        id = "hints_debug_folder",
        long = "hints.debug-folder",
        default_value = "./hints_debug",
        requires = "hints_debug"
    )]
    pub debug_folder: String,
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

    /// Path to the guest ELF file
    #[arg(long, short = 'g', default_value = "./elf/zec-reth.elf")]
    pub guest: String,

    /// ZisK coordinator URL
    #[arg(long, short = 'c', default_value = "http://localhost:50051")]
    pub coordinator_url: String,

    /// Prove timeout in seconds
    #[arg(long, short = 't', default_value_t = 600)]
    pub prove_timeout: u64,

    /// Exit process with code 1 when proof generation fails
    #[arg(long)]
    pub exit_on_error: bool,

    /// Maximum run time in minutes. When set, the application exits once this
    /// time has elapsed
    #[arg(long, short = 'r')]
    pub run_time: Option<u64>,

    #[command(flatten)]
    pub inputs: InputsArgs,

    #[command(flatten)]
    pub rpc: RpcArgs,

    #[command(flatten)]
    pub folder: FolderArgs,

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

    #[command(flatten)]
    pub proof: ProofArgs,

    #[command(flatten)]
    pub hooks: HooksArgs,

    #[cfg(zisk_hints)]
    #[command(flatten)]
    pub hints: HintsArgs,
}

impl CliArgs {
    pub fn telegram_enabled(&self, event: TelegramEvent) -> bool {
        self.telegram.alert.iter().any(|e| *e == event)
    }

    /// Validate combinations that clap cannot express declaratively.
    pub fn validate(&self) -> Result<(), Error> {
        // Submitting to Ethproofs requires real (RPC-driven) blocks; the
        // folder-based input mode replays pre-generated inputs and would
        // submit stale or synthetic proofs.
        if self.ethproofs.submit && self.inputs.mode == InputGen::Folder {
            return Err(Self::command().error(
                ErrorKind::ArgumentConflict,
                "'--ethproofs.submit' cannot be used with '--input.mode folder'",
            ));
        }

        // The run-time limit is driven by the WS block subscription, so it only makes sense
        // when generating inputs from RPC.
        if self.run_time.is_some() && self.inputs.mode != InputGen::Rpc {
            return Err(Self::command().error(
                ErrorKind::ArgumentConflict,
                "'--run-time' can only be used with '--input.mode rpc'",
            ));
        }
        Ok(())
    }
}
