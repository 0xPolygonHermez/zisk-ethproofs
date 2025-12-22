use std::env;

use anyhow::Result;
use log::{debug, error};
use reqwest::Client;

// Define the types of Telegran alerts that can be sent
#[allow(dead_code)]
pub enum AlertType {
    Success,
    Error,
    Info,
    Warning,
}

// Send an alert to a Telegram chat using the Telegram Bot API
pub async fn send_telegram_alert(message: &str, alert_type: AlertType) -> Result<()> {
    // Check if the TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID environment variables are set, if not return
    let bot_token = match env::var("TELEGRAM_BOT_TOKEN").ok() {
        Some(token) => token,
        None => return Err(anyhow::anyhow!("TELEGRAM_BOT_TOKEN not set")),
    };
    let chat_id = match env::var("TELEGRAM_CHAT_ID").ok() {
        Some(id) => id,
        None => return Err(anyhow::anyhow!("TELEGRAM_CHAT_ID not set")),
    };
    let pre_msg = match env::var("TELEGRAM_PREFIX_MSG") {
        Ok(msg) => format!("{}:", msg),
        Err(_) => String::from(""),
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
