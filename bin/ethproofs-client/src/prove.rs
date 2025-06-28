use std::{env, fs::{File, create_dir_all}, io::{BufRead, BufReader, Write}, path::Path, process::{Command,Stdio}};

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

pub async fn generate_proof(block_number: u64, no_distributed: bool, input_folder: String) -> Result<ProofResult> {
    let elf_file = format!(
        "{}/{}",
        PROGRAM_FOLDER,
        env::var("ELF_FILE").expect("ELF_FILE must be set")
    );
    let input_file = format!("{}/{}.bin", input_folder, block_number);
    let output_folder = format!("{}/{}", OUTPUT_FOLDER, block_number);

    create_dir_all(Path::new(OUTPUT_FOLDER))?;
    create_dir_all(Path::new(LOG_FOLDER))?;

    let num_processes =
        env::var("DISTRIBUTED_PROVE_PROCESSES").expect("DISTRIBUTED_PROVE_PROCESSES must be set");
    let num_threads =
        env::var("DISTRIBUTED_PROVE_THREADS").expect("DISTRIBUTED_PROVE_THREADS must be set");

    let command = if no_distributed {
        info!("Generating proof without distributed proving");
        format!(
            "cargo-zisk prove -e {} -i {} -o {} -a -u",
            elf_file, input_file, output_folder
        )
    } else {
        format!(
            "mpirun --allow-run-as-root --bind-to none -np {} -x OMP_NUM_THREADS={} cargo-zisk prove -e {} -i {} -o {} -a -u",
            num_processes, num_threads, elf_file, input_file, output_folder
        )
    };

    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;

    let stdout = child.stdout.take().expect("Failed to capture stdout");
    let reader = BufReader::new(stdout);

    let mut proved = false;
    let mut captured_output = Vec::new();

    for line in reader.lines() {
        let line = line?;
        println!("{}", line);
        captured_output.push(format!("{}\n", line));

        if line.contains("PROVE SUMMARY") {
            proved = true;
        }
    }

    let _status = child.wait()?; // No usamos el exit code para decidir éxito

    if proved {
        let file = File::open(format!("{}/result.json", output_folder))?;
        let reader = BufReader::new(file);
        let proof_result: ProofResult = serde_json::from_reader(reader)?;
        Ok(proof_result)
    } else {
        let log_path = format!("{}/{}.log", LOG_FOLDER, block_number);
        let mut log_file = File::create(&log_path)?;
        write!(log_file, "{}", captured_output.concat())?;

        Err(anyhow!("Proof verification failed for block number {}. Log saved at {}", block_number, log_path))
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
