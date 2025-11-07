use ethers::types::U256;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BlockCommand { Queued, Input }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BlockInfo {
    pub block_number: u64,
    pub timestamp: U256,
    pub block_hash: String,
    pub tx_count: usize,
    pub mgas: u64,
}

impl BlockInfo {
    pub fn short_hash(&self) -> String { short_hash(&self.block_hash) }
    pub fn filename(&self) -> String { format!("{}_{}.bin", self.block_number, self.short_hash()) }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BlockMessage { pub command: BlockCommand, pub info: BlockInfo }

impl BlockMessage {
    pub fn new_queued(block_number: u64) -> Self {
        Self { command: BlockCommand::Queued, info: BlockInfo { block_number, timestamp: U256::zero(), block_hash: String::new(), tx_count: 0, mgas: 0 } }
    }
    pub fn new_input(block_number: u64, timestamp: U256, block_hash: String, tx_count: usize, mgas: u64) -> Self {
        Self { command: BlockCommand::Input, info: BlockInfo { block_number, timestamp, block_hash, tx_count, mgas } }
    }
}

pub fn short_hash(hash: &str) -> String { hash.chars().take(6).collect() }
