use anyhow::Result;
use clap::Parser;

use crate::{cliargs::{CliArgs, Commands}, client::run_client, input_server::run_input_server};

mod api;
mod cliargs;
mod client;
mod db;
mod input;
mod metrics;
mod process;
mod prove;
mod input_server;
mod state;
mod telegram;
mod webhook;


#[tokio::main]
async fn main() -> Result<()> {
    let cliargs = CliArgs::parse();

    match cliargs.command {
        Some(Commands::InputServer) => {
            run_input_server().await?;
        }
        None => {
            run_client().await?;
        }
    }

    Ok(())
}
