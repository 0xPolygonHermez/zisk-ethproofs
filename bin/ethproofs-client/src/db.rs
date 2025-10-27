use log::{debug, error, info};
use sqlx::{mysql::MySqlPoolOptions, MySql, Pool};
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct BlockProof {
    pub block_number: u64,    // NOT NULL, UNIQUE
    pub zisk_version: String, // varchar(255)
    pub hardware: String,     // varchar(255)
    pub proof: String,        // LONGTEXT
    pub proving_time: u32,    // INT UNSIGNED
    pub steps: u64,           // BIGINT UNSIGNED
}

/// Configuración del escritor.
#[derive(Debug, Clone)]
pub struct DbBlockProofsConfig {
    pub queue_capacity: usize,
    pub max_connections: u32,
    pub acquire_timeout: Duration,
    pub max_retries: usize,
    pub base_backoff: Duration,
}

impl Default for DbBlockProofsConfig {
    fn default() -> Self {
        Self {
            queue_capacity: 10,
            max_connections: 5,
            acquire_timeout: Duration::from_secs(5),
            max_retries: 5,
            base_backoff: Duration::from_millis(200),
        }
    }
}

#[derive(Clone)]
pub struct DbBlockProofs {
    tx: mpsc::Sender<BlockProof>,
}

impl DbBlockProofs {
    pub async fn new(dsn: &str, cfg: DbBlockProofsConfig) -> anyhow::Result<Self> {
        info!("Connecting to block proofs DB at {}", dsn);
        let pool = MySqlPoolOptions::new()
            .max_connections(cfg.max_connections)
            .acquire_timeout(cfg.acquire_timeout)
            .connect(dsn)
            .await?;

        let (tx, rx) = mpsc::channel::<BlockProof>(cfg.queue_capacity);

        let _ = tokio::spawn(db_block_proofs_worker(pool, rx, cfg));

        Ok(Self { tx })
    }

    pub async fn enqueue(
        &self,
        block_proof: BlockProof,
    ) -> Result<(), tokio::sync::mpsc::error::SendError<BlockProof>> {
        self.tx.send(block_proof).await
    }
}

async fn db_block_proofs_worker(
    pool: Pool<MySql>,
    mut rx: mpsc::Receiver<BlockProof>,
    cfg: DbBlockProofsConfig,
) {
    let insert_sql = r#"
            INSERT INTO block_proofs
                (block_number, zisk_version, hardware, proving_time, steps, proof)
            VALUES
                (?, ?, ?, ?, ?, ?)
            ON DUPLICATE KEY UPDATE
                zisk_version = VALUES(zisk_version),
                hardware = VALUES(hardware),
                proving_time = VALUES(proving_time),
                steps = VALUES(steps),
                proof = VALUES(proof)
        "#;

    loop {
        tokio::select! {
            biased;

            maybe_block_proof = rx.recv() => {
                match maybe_block_proof {
                    Some(block_proof) => {
                        debug!("Inserting DB block proof, block_number: {}", block_proof.block_number);
                        if let Err(e) = insert_with_retries(&pool, insert_sql, &block_proof, cfg.max_retries, cfg.base_backoff).await {
                            error!("Failed to insert DB block proof, error: {}, block_number: {}", e, block_proof.block_number);
                        }
                    }
                    None => {
                        break;
                    }
                }
            }
        }
    }
}

async fn insert_with_retries(
    pool: &Pool<MySql>,
    sql: &str,
    block_proof: &BlockProof,
    max_retries: usize,
    base_backoff: Duration,
) -> anyhow::Result<()> {
    let mut attempt = 0usize;
    loop {
        let res = sqlx::query(sql)
            .bind(block_proof.block_number)
            .bind(&block_proof.zisk_version)
            .bind(&block_proof.hardware)
            .bind(block_proof.proving_time as u64)
            .bind(block_proof.steps)
            .bind(&block_proof.proof)
            .execute(pool)
            .await;

        match res {
            Ok(_) => return Ok(()),
            Err(e) => {
                attempt += 1;

                if attempt > max_retries {
                    debug!(
                        "Max retries reached ({}) for inserting block proof into DB, giving up",
                        max_retries
                    );
                    return Err(e.into());
                }

                error!(
                    "Failed to insert block proof into DB, error: {}, attempt: {}/{}",
                    e, attempt, max_retries
                );

                let backoff =
                    base_backoff.saturating_mul(2u32.saturating_pow((attempt - 1) as u32));
                tokio::time::sleep(backoff.min(Duration::from_secs(5))).await;
            }
        }
    }
}
