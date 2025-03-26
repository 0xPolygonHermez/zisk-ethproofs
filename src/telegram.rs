use anyhow::Result;
use log::{error, info};
use reqwest::Client;
use std::env;

#[allow(dead_code)]
pub enum AlertType {
    Success,
    Error,
    Info,
    Warning,
}

pub async fn send_telegram_alert(message: &str, alert_type: AlertType) -> Result<()> {
    let icon = match alert_type {
        AlertType::Success => "✅",
        AlertType::Error => "❌",
        AlertType::Warning => "⚠️",
        AlertType::Info => "ℹ️",
    };

    let full_message = format!("{} {}", icon, message);

    let bot_token = env::var("TELEGRAM_BOT_TOKEN")?;
    let chat_id = env::var("TELEGRAM_CHAT_ID")?;

    let url = format!("https://api.telegram.org/bot{}/sendMessage", bot_token);

    let client = Client::new();
    let res = client.post(&url)
        .json(&serde_json::json!({
            "chat_id": chat_id,
            "text": full_message
        }))
        .send()
        .await?;

    if res.status().is_success() {
        info!("Telegram alert sent successfully, message: {}", message);
    } else {
        error!("Failed to send Telegram alert, error {:?}", res.text().await?);
    }

    Ok(())
}