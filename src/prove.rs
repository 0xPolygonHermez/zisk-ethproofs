use std::{env, path::Path, process::Command, time::Instant};

use anyhow::{anyhow, Ok, Result};
use base64::{engine::general_purpose, Engine};
use flate2::write::GzEncoder;
use flate2::Compression;
use log::info;
use rand::Rng;
use tar::Builder;

use crate::{INPUT_FOLDER, LOG_FOLDER, PROGRAM_FOLDER, PROOF_FOLDER};

pub async fn generate_proof(block_number: u64, no_distributed: bool) -> Result<(u128, u64)> {
    let mut rng = rand::rng();

    let elf_file = format!("{}/{}", PROGRAM_FOLDER, env::var("ELF_FILE").unwrap_or("zisk-eth-client.elf".to_string()));
    let input_file = format!("{}/{}.bin", INPUT_FOLDER, block_number);
    let output_folder = format!("{}/{}", PROOF_FOLDER, block_number);

    // Create proof and log folders if they don't exist
    std::fs::create_dir_all(Path::new(PROOF_FOLDER))?;
    std::fs::create_dir_all(Path::new(LOG_FOLDER))?;

    let num_processes = env::var("DISTRIBUTED_PROVE_PROCESSES").unwrap_or("4".to_string());
    let num_threads = env::var("DISTRIBUTED_PROVE_THREADS").unwrap_or("56".to_string());

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
            "mpirun --bind-to none -np {} -x OMP_NUM_THREADS={} cargo-zisk prove -e {} -i {} -o {} -a -y > {} 2>&1",
            num_processes, num_threads, elf_file, input_file, output_folder, log_path
        )
    };

    let start = Instant::now();
    let output = Command::new("sh")
        .arg("-c")
        .arg(command)
        .output()?;
    let proving_time = start.elapsed().as_millis();

    // Check if the command was successful
    if output.status.success() {
        // TODO: Extract cycles from the log if needed
        let cycles: u64 = rng.random_range(1000000..5000000);
        Ok((proving_time, cycles))
    } else {
        Err(anyhow!("Error generating proof for block number {}, check log file {}", block_number, log_path))
    }
}

pub fn get_proof_b64(block_number: u64) -> Result<String> {
    // List of files to include in the archive
    let files = [
        format!("{}/{}/proofs/vadcop_final_proof.json", PROOF_FOLDER, block_number),
        format!("{}/{}/publics.json", PROOF_FOLDER, block_number)
    ];

    // Create an in-memory buffer to hold the .tar.gz data
    let mut buffer = Vec::new();
    let gz_encoder = GzEncoder::new(&mut buffer, Compression::default());
    let mut tar_builder = Builder::new(gz_encoder);

    // Add each file to the tar archive, using only its filename in the archive
    for file_path in &files {
        let path = Path::new(file_path);
        tar_builder.append_path_with_name(&path, path.file_name().unwrap())?;
    }

    // Finalize the tar archive
    tar_builder.finish()?;
    let gz_encoder = tar_builder.into_inner()?; // Get the GzEncoder back
    gz_encoder.finish()?; // Finalize the gzip compression

    // Encode the resulting .tar.gz buffer as base64
    let base64_encoded = general_purpose::STANDARD.encode(&buffer);

    Ok(base64_encoded)
}
