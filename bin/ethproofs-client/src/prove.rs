use std::{env, fs::File, io::BufReader, path::Path, process::Command};

use anyhow::{anyhow, Ok, Result};
use base64::{engine::general_purpose, Engine};
use flate2::write::GzEncoder;
use flate2::Compression;
use log::info;
use serde::Deserialize;
use tar::Builder;

use crate::{LOG_FOLDER, OUTPUT_FOLDER, PROGRAM_FOLDER};

#[derive(Debug, Deserialize)]
pub struct ProofResult {
    pub cycles: u64,
    pub id: String,
    #[serde(deserialize_with = "time_to_millisecs")]
    pub time: u128,
}

fn time_to_millisecs<'de, D>(deserializer: D) -> std::result::Result<u128, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let f = f64::deserialize(deserializer)?;
    std::result::Result::Ok((f.trunc() * 1000.0) as u128)
}

/// Generate the input file for the given block number and return the time taken in milliseconds and the number of cycles
pub async fn generate_proof(block_number: u64, no_distributed: bool, input_folder: String) -> Result<ProofResult> {
    let elf_file = format!(
        "{}/{}",
        PROGRAM_FOLDER,
        env::var("ELF_FILE").expect("ELF_FILE must be set")
    );
    let input_file = format!("{}/{}.bin", input_folder, block_number);
    let output_folder = format!("{}/{}", OUTPUT_FOLDER, block_number);

    // Create proof and log folders if they don't exist
    std::fs::create_dir_all(Path::new(OUTPUT_FOLDER))?;
    std::fs::create_dir_all(Path::new(LOG_FOLDER))?;

    let num_processes =
        env::var("DISTRIBUTED_PROVE_PROCESSES").expect("DISTRIBUTED_PROVE_PROCESSES must be set");
    let num_threads =
        env::var("DISTRIBUTED_PROVE_THREADS").expect("DISTRIBUTED_PROVE_THREADS must be set");

    // Generate path for the log file
    let log_path = format!("{}/{}.log", LOG_FOLDER, block_number);

    let command = if no_distributed {
        info!("Generating proof without distributed proving");
        format!(
            "cargo-zisk prove -e {} -i {} -o {} -a -y > {} 2>&1",
            elf_file, input_file, output_folder, log_path
        )
    } else {
        format!(
            "mpirun --allow-run-as-root --bind-to none -np {} -x OMP_NUM_THREADS={} cargo-zisk prove -e {} -i {} -o {} -a -y > {} 2>&1",
            num_processes, num_threads, elf_file, input_file, output_folder, log_path
        )
    };

    let output = Command::new("sh").arg("-c").arg(command).output()?;

    // Check if the command was successful
    if output.status.success() {
        let file = File::open(format!("{}/result.json", output_folder))?;
        let reader = BufReader::new(file);
        let proof_result: ProofResult = serde_json::from_reader(reader)?;

        Ok(proof_result)
    } else {
        Err(anyhow!(
            "Error generating proof for block number {}, check log file {}",
            block_number,
            log_path
        ))
    }
}

/// Get the proof files for the given block number, compress them into a .tar.gz buffer and return it as base64 encoded string
pub fn get_proof_b64(block_number: u64) -> Result<String> {
    // List of files to include in the archive
    let files = [
        format!(
            "{}/{}/proofs/vadcop_final_proof.json",
            OUTPUT_FOLDER, block_number
        ),
        format!("{}/{}/publics.json", OUTPUT_FOLDER, block_number),
    ];

    // Create an in-memory buffer to hold the .tar.gz data
    let mut buffer = Vec::new();
    let gz_encoder = GzEncoder::new(&mut buffer, Compression::default());
    let mut tar_builder = Builder::new(gz_encoder);

    // Add each file to the tar archive, using only its filename in the archive
    for file_path in &files {
        let path = Path::new(file_path);
        tar_builder.append_path_with_name(path, path.file_name().unwrap())?;
    }

    // Finalize the tar archive
    tar_builder.finish()?;
    let gz_encoder = tar_builder.into_inner()?; // Get the GzEncoder back
    gz_encoder.finish()?; // Finalize the gzip compression

    // Encode the resulting .tar.gz buffer as base64
    let base64_encoded = general_purpose::STANDARD.encode(&buffer);

    Ok(base64_encoded)
}
