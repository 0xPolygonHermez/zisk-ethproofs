use std::time::Duration;

use anyhow::{anyhow, Result};
use log::debug;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::time::sleep;
use url::Url;

#[derive(Clone, Debug)]
pub struct EthProofsApi {
    client: Client,
    url: String,
    token: String,
}

#[allow(unused)]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Cluster {
    id: u32,
    nickname: String,
    description: String,
    hardware: String,
    cycle_type: String,
    proof_type: String,
    cluster_configuration: Vec<InstanceConfiguration>,
}

#[allow(unused)]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SingleMachine {
    nickname: String,
    description: String,
    hardware: String,
    cycle_type: String,
    proof_type: String,
    instance_type: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstanceConfiguration {
    instance_type_id: u32,
    instance_count: u32,
    aws_instance_pricing: AwsInstancePricing,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AwsInstancePricing {
    id: u32,
    instance_type: String,
    region: String,
    hourly_price: f64,
    instance_memory: f64,
    vcpu: u32,
    instance_storage: String,
    created_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProofQueued {
    cluster_id: u32,
    block_number: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProofProving {
    cluster_id: u32,
    block_number: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProofProved {
    block_number: u64,
    cluster_id: u32,
    proving_time: u128,
    proving_cycles: u64,
    proof: String,
    verifier_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProofId {
    proof_id: u64,
}

impl EthProofsApi {
    const MAX_RETRIES: usize = 3;
    const RETRY_DELAY_MS: u64 = 500;

    pub fn new(url: String, token: String) -> Self {
        let client = Client::builder()
            .pool_idle_timeout(None)
            .tcp_keepalive(Duration::from_secs(60))
            .http2_keep_alive_while_idle(true)
            .http2_keep_alive_interval(Duration::from_secs(30))
            .http2_adaptive_window(true)
            .pool_max_idle_per_host(8)
            .tcp_nodelay(true)
            .build()
            .expect("Failed to build  client");

        EthProofsApi { client, url, token }
    }

    async fn send_with_retries(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        let mut last_err = anyhow!("Ethproofs API request Unknown error");

        for attempt in 1..=Self::MAX_RETRIES {
            match request.try_clone().unwrap().send().await {
                Ok(resp) => {
                    if resp.status().is_success() {
                        return Ok(resp);
                    } else {
                        last_err =
                            anyhow!("Ethproofs API request error, status code {}", resp.status());
                    }
                }
                Err(e) => {
                    last_err = anyhow!("Ethproofs API network error {}", e);
                }
            }

            debug!(
                "Ethproofs API request attempt {}/{} failed, retrying in {} ms...",
                attempt,
                Self::MAX_RETRIES,
                Self::RETRY_DELAY_MS
            );
            sleep(Duration::from_millis(Self::RETRY_DELAY_MS * attempt as u64)).await;
        }

        Err(last_err)
    }

    #[allow(unused)]
    pub async fn get_clusters(&self) -> Result<Vec<Cluster>> {
        let url = Url::parse(&format!("{}/clusters", self.url))?;
        debug!("get_clusters GET {}", url);

        let request = self.client.get(url).bearer_auth(&self.token);

        let response = self.send_with_retries(request).await?;
        let clusters: Vec<Cluster> = response.json().await?;
        for cluster in &clusters {
            debug!("Cluster id: {}, nickname: {}", cluster.id, cluster.nickname);
        }
        Ok(clusters)
    }

    #[allow(unused)]
    pub async fn add_cluster(&self, cluster: Cluster) -> Result<u32> {
        let url = Url::parse(&format!("{}/clusters", self.url))?;
        debug!("add_cluster POST {}", url);

        let request = self.client.post(url).bearer_auth(&self.token).json(&cluster);

        let response = self.send_with_retries(request).await?;
        let cluster_id: u32 = response.json().await?;
        debug!("Added cluster, id: {}, nickname: {}", cluster_id, cluster.nickname);
        Ok(cluster_id)
    }

    #[allow(unused)]
    pub async fn add_single_machine(&self, single_machine: SingleMachine) -> Result<u32> {
        let url = Url::parse(&format!("{}/single-machine", self.url))?;
        debug!("add_single_machine POST {}", url);

        let request = self.client.post(url).bearer_auth(&self.token).json(&single_machine);

        let response = self.send_with_retries(request).await?;
        let cluster_id: u32 = response.json().await?;
        debug!("Added single machine, id: {}, nickname: {}", cluster_id, single_machine.nickname);
        Ok(cluster_id)
    }

    pub async fn proof_queued(&self, cluster_id: u32, block_number: u64) -> Result<u64> {
        let url = Url::parse(&format!("{}/proofs/queued", self.url))?;
        debug!("proof_queued POST {}", url);

        let request = self
            .client
            .post(url)
            .bearer_auth(&self.token)
            .json(&ProofQueued { cluster_id, block_number });

        let response = self.send_with_retries(request).await?;
        let proof_id: ProofId = response.json().await?;
        debug!(
            "Proof queued, id: {}, block_number: {}, cluster_id: {}",
            proof_id.proof_id, block_number, cluster_id
        );
        Ok(proof_id.proof_id)
    }

    pub async fn proof_proving(&self, cluster_id: u32, block_number: u64) -> Result<u64> {
        let url = Url::parse(&format!("{}/proofs/proving", self.url))?;
        debug!("proof_proving POST {}", url);

        let request = self
            .client
            .post(url)
            .bearer_auth(&self.token)
            .json(&ProofProving { cluster_id, block_number });

        let response = self.send_with_retries(request).await?;
        let proof_id: ProofId = response.json().await?;
        debug!(
            "Proof proving, id: {}, block_number: {}, cluster_id: {}",
            proof_id.proof_id, block_number, cluster_id
        );
        Ok(proof_id.proof_id)
    }

    pub async fn proof_proved(
        &self,
        cluster_id: u32,
        block_number: u64,
        time: u128,
        cycles: u64,
        proof: &String,
        verifier_id: String,
    ) -> Result<u64> {
        let url = Url::parse(&format!("{}/proofs/proved", self.url))?;
        debug!("proof_proved POST {}", url);

        let request = self.client.post(url).bearer_auth(&self.token).json(&ProofProved {
            block_number,
            cluster_id,
            proving_time: time,
            proving_cycles: cycles,
            proof: proof.to_string(),
            verifier_id,
        });

        let response = self.send_with_retries(request).await?;
        let proof_id: ProofId = response.json().await?;
        debug!(
            "Proof proved, id: {}, block_number: {}, cluster_id: {}",
            proof_id.proof_id, block_number, cluster_id
        );
        Ok(proof_id.proof_id)
    }
}
