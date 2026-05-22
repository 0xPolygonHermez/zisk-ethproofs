use log::{error, info};
use tokio::process::Command;

/// Spawn a shell script in the background, passing the given arguments.
/// Errors launching or running the script are logged but never returned.
pub fn run_hook(script_path: &str, args: Vec<String>) {
    let script = script_path.to_string();
    tokio::spawn(async move {
        match Command::new(&script).args(&args).status().await {
            Ok(status) => {
                if status.success() {
                    info!("Hook script {} executed successfully", script);
                } else {
                    error!("Hook script {} exited with status {}", script, status);
                }
            }
            Err(e) => {
                error!("Failed to execute hook script {}: {}", script, e);
            }
        }
    });
}
