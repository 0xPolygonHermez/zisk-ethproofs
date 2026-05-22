use anyhow::Result;
use log::{debug, error, warn};
use reqwest::Client;

use crate::cliargs::{TelegramArgs, TelegramEvent};
use crate::state::AppState;

// Define the types of Telegran alerts that can be sent
#[allow(dead_code)]
pub enum AlertType {
    Success,
    Error,
    Info,
    Warning,
}

// Send an alert to a Telegram chat using the Telegram Bot API
pub async fn send_telegram_alert(
    args: &TelegramArgs,
    message: &str,
    alert_type: AlertType,
) -> Result<()> {
    let bot_token = match args.bot_token.as_ref() {
        Some(token) => token,
        None => return Err(anyhow::anyhow!("telegram.bot-token not set")),
    };
    let chat_id = match args.chat_id.as_ref() {
        Some(id) => id,
        None => return Err(anyhow::anyhow!("telegram.chat-id not set")),
    };
    let pre_msg = if args.message_prefix.is_empty() {
        String::new()
    } else {
        format!("{}:", args.message_prefix)
    };

    // Set the icon based on the alert type
    let icon = match alert_type {
        AlertType::Success => "✅",
        AlertType::Error => "❌",
        AlertType::Warning => "⚠️",
        AlertType::Info => "ℹ️",
    };

    // Format the message with the icon
    let full_message = format!("{} {} {}", icon, pre_msg, message);

    // Send the message to the Telegram chat
    let url = format!("https://api.telegram.org/bot{}/sendMessage", bot_token);
    let client = Client::new();
    let res = client
        .post(&url)
        .json(&serde_json::json!({
            "chat_id": chat_id,
            "text": full_message
        }))
        .send()
        .await?;

    // Check if the request was successful
    if res.status().is_success() {
        debug!("Telegram alert sent successfully, message: {}", message);
    } else {
        error!("Failed to send Telegram alert, error {:?}", res.text().await?);
    }

    Ok(())
}

/// Spawn a telegram alert in the background and return its JoinHandle so
/// callers that need to await its completion (e.g. before process exit) can.
fn spawn_alert(
    app_state: &AppState,
    msg: String,
    alert_type: AlertType,
) -> tokio::task::JoinHandle<()> {
    let telegram_args = app_state.cliargs.telegram.clone();
    tokio::spawn(async move {
        if let Err(e) = send_telegram_alert(&telegram_args, &msg, alert_type).await {
            warn!("Failed to send Telegram alert: {}, error: {}", msg, e);
        }
    })
}

/// Send a Telegram "Started" alert if enabled
pub fn send_started_alert(
    app_state: &AppState,
    mode: &str,
) -> Option<tokio::task::JoinHandle<()>> {
    if !app_state.cliargs.telegram_enabled(TelegramEvent::Started) {
        return None;
    }
    let msg = format!("EthProofs client started ({} mode)", mode);
    Some(spawn_alert(app_state, msg, AlertType::Info))
}

/// Send a Telegram "BlockProved" alert if enabled
pub fn send_block_proved_alert(
    app_state: &AppState,
    block_number: u64,
    proving_time_ms: u64,
    proving_cycles: u64,
) -> Option<tokio::task::JoinHandle<()>> {
    if !app_state.cliargs.telegram_enabled(TelegramEvent::BlockProved) {
        return None;
    }
    let msg = format!(
        "Proof generated for block {}, proving_time: {}s, cycles: {}",
        block_number,
        proving_time_ms / 1000,
        proving_cycles,
    );
    Some(spawn_alert(app_state, msg, AlertType::Success))
}

/// Send a Telegram "SkippedThreshold" alert if enabled
pub fn send_skipped_threshold_alert(
    app_state: &AppState,
    proving_block_number: u64,
    block_number: u64,
    skipped_count: u64,
) -> Option<tokio::task::JoinHandle<()>> {
    if !app_state.cliargs.telegram_enabled(TelegramEvent::SkippedThreshold) {
        return None;
    }
    let msg = format!(
        "Skipped {} consecutive blocks. Currently proving block {}, next queued block is {}.",
        skipped_count, proving_block_number, block_number
    );
    Some(spawn_alert(app_state, msg, AlertType::Warning))
}

/// Send a Telegram alert when proving resumes after a skipped-threshold alert
pub fn send_skipped_resumed_alert(
    app_state: &AppState,
    proving_block_number: u64,
) -> Option<tokio::task::JoinHandle<()>> {
    if !app_state.cliargs.telegram_enabled(TelegramEvent::SkippedThreshold) {
        return None;
    }
    let msg = format!("Resumed proving. Now proving block {}.", proving_block_number);
    Some(spawn_alert(app_state, msg, AlertType::Info))
}

/// Send a Telegram "ProofFailed" alert if enabled
pub fn send_proof_failed_alert(
    app_state: &AppState,
    msg: String,
) -> Option<tokio::task::JoinHandle<()>> {
    if !app_state.cliargs.telegram_enabled(TelegramEvent::ProofFailed) {
        return None;
    }
    Some(spawn_alert(app_state, msg, AlertType::Error))
}

/// Send a Telegram alert when proving resumes after a proof-failed alert
pub fn send_proof_resumed_alert(
    app_state: &AppState,
    block_number: u64,
) -> Option<tokio::task::JoinHandle<()>> {
    if !app_state.cliargs.telegram_enabled(TelegramEvent::ProofFailed) {
        return None;
    }
    let msg = format!("Resumed proving. Now proving block {}.", block_number);
    Some(spawn_alert(app_state, msg, AlertType::Info))
}
