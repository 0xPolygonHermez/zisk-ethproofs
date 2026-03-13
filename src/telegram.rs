use anyhow::Result;
use log::{debug, error};
use reqwest::Client;

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
pub async fn send_telegram_alert(message: &str, alert_type: AlertType, app_state: &AppState) -> Result<()> {
    // Set the icon based on the alert type
    let icon = match alert_type {
        AlertType::Success => "✅",
        AlertType::Error => "❌",
        AlertType::Warning => "⚠️",
        AlertType::Info => "ℹ️",
    };

    let bot_token = app_state.cliargs.telegram.bot_token.clone().unwrap();
    let chat_id = app_state.cliargs.telegram.chat_id.clone().unwrap();

    // Format the message with the icon
    let full_message = format!("{} {} {}", icon, app_state.cliargs.telegram.message_prefix, message);

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
