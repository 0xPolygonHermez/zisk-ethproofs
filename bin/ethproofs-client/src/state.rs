
use std::{env, sync::{Arc, Mutex}};

use clap::Parser;
use dotenv::dotenv;

use crate::{api::EthProofsApi, db::{self, DbBlockProofs}, DEFAULT_INPUTS_FOLDER};
use crate::cliargs::CliArgs;

#[derive(Clone)]
pub struct AppState {
    pub cliargs: CliArgs,
    pub proving_block: Arc<Mutex<u64>>,
    pub ethproofs_client: Option<EthProofsApi>,
    pub ethproofs_cluster_id: Option<u32>,
    pub inputs_folder: String,
    pub input_gen_server_url: String,
    pub compute_capacity: u32,
    pub db_block_proofs: Option<DbBlockProofs>,
}

impl AppState {
    pub async fn new() -> Self {
        // Load environment variables from .env file
        dotenv().ok();

        // Parse the command line arguments
        let cliargs = CliArgs::parse();

        let (ethproofs_client, ethproofs_cluster_id) = if cliargs.submit_ethproofs {
            let ethproofs_url = match env::var("ETHPROOFS_API_URL") {
                Ok(url) => url,
                Err(_) => panic!("ETHPROOFS_API_URL not set"),
            };
            let ethproofs_token = match env::var("ETHPROOFS_API_TOKEN") {
                Ok(token) => token,
                Err(_) => panic!("ETHPROOFS_API_TOKEN not set"),
            };
            let ethproofs_cluster_id = match env::var("ETHPROOFS_CLUSTER_ID") {
                Ok(cid_str) => match cid_str.parse::<u32>() {
                    Ok(cid) => cid,
                    Err(_) => panic!("ETHPROOFS_CLUSTER_ID is not a valid u32"),
                },
                Err(_) => panic!("ETHPROOFS_CLUSTER_ID not set"),
            };

            (Some(EthProofsApi::new(ethproofs_url, ethproofs_token)), Some(ethproofs_cluster_id))
        } else {
            (None, None)
        };

        let inputs_folder = env::var("INPUTS_FOLDER").unwrap_or(DEFAULT_INPUTS_FOLDER.to_string());
        let input_gen_server_url = env::var("INPUT_GEN_SERVER_URL").expect("INPUT_GEN_SERVER_URL must be set");
        let proving_block = Arc::new(Mutex::new(0u64));

        let compute_capacity = match env::var("COMPUTE_CAPACITY") {
            Ok(capacity_str) => match capacity_str.parse::<u32>() {
                Ok(capacity) => capacity,
                Err(_) => panic!("COMPUTE_CAPACITY is not a valid value"),
            },
            Err(_) => panic!("COMPUTE_CAPACITY not set"),
        };

        let db_dsn = if cliargs.insert_db {
            match env::var("DB_DSN") {
                Ok(dsn) => Some(dsn),
                Err(_) => panic!("DB_DSN not set"),
            }
        } else {
            None
        };

        let db_block_proofs = if let Some(dsn) = &db_dsn {
            Some(db::DbBlockProofs::new(dsn, db::DbBlockProofsConfig::default()).await.expect("Failed to create DbBlockProofs"))
        } else {
            None
        };

        Self {
            cliargs,
            proving_block,
            ethproofs_client,
            ethproofs_cluster_id,
            inputs_folder,
            input_gen_server_url,
            compute_capacity,
            db_block_proofs,
        }
    }
}