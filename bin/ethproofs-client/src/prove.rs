use std::{env, fs::{create_dir_all, File}, io::{BufRead, BufReader, Write}, net::TcpStream, path::Path, process::{Command,Stdio}};

use anyhow::{anyhow, Context, Ok, Result};
use base64::{engine::general_purpose, Engine};
use flate2::write::GzEncoder;
use flate2::Compression;
use log::info;
use serde::Deserialize;
use serde_json::Value;
use server::{ZiskRequest, ZiskResponse, ZiskStatusRequest};
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

pub async fn wait_prove_done() -> Result<()> {
    let command = String::from("cargo-zisk prove-client status --port 6100");

    let mut proved = false;
    let start_time = std::time::Instant::now();

    while !proved {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&command)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;

        let stdout = child.stdout.take().expect("Failed to capture stdout");
        let reader = BufReader::new(stdout);

        let mut captured_output = Vec::new();

        for line in reader.lines() {
            let line = line?;
            println!("{}", line);
            captured_output.push(format!("{}\n", line));

            if let Some(start_idx) = line.find('{') {
                let json_str = &line[start_idx..];

                info!("Received JSON: {}", json_str);
                if let std::result::Result::Ok(json) = serde_json::from_str::<Value>(json_str) {
                    let node = json.get("node").and_then(|n| n.as_u64());
                    let result = json.get("result").and_then(|r| r.as_str());
                    let code = json.get("code").and_then(|c| c.as_u64());

                    info!("Parsed JSON: node: {:?}, result: {:?}, code: {:?}", node, result, code);
                    if node == Some(0) && result == Some("idle") && code == Some(0) {
                        info!("Proof generated for block");
                        proved = true;
                        return Ok(());
                    }
                }            
            }        
        }

        if start_time.elapsed().as_secs() > 300 {
            return Err(anyhow!("Proof generation timed out after 300 seconds"));
        }
    }

    Ok(())    
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
        // format!(
        //     "mpirun --allow-run-as-root --bind-to none -np {} -x OMP_NUM_THREADS={} cargo-zisk prove -e {} -i {} -o {} -a -u",
        //     num_processes, num_threads, elf_file, input_file, output_folder
        // )
        format!(
            "mpirun --allow-run-as-root --bind-to none -np {} -x OMP_NUM_THREADS={} cargo-zisk prove-client prove -i {} -a --port 6100 -p {}",
            num_processes, num_threads, input_file, block_number
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

    let mut proving = false;
    let mut captured_output = Vec::new();

    for line in reader.lines() {
        let line = line?;
        println!("{}", line);
        captured_output.push(format!("{}\n", line));

        if let Some(start_idx) = line.find('{') {
            let json_str = &line[start_idx..];

            info!("Received JSON: {}", json_str);
            if let std::result::Result::Ok(json) = serde_json::from_str::<Value>(json_str) {
                let node = json.get("node").and_then(|n| n.as_u64());
                let result = json.get("result").and_then(|r| r.as_str());
                let code = json.get("code").and_then(|c| c.as_u64());

                info!("Parsed JSON: node: {:?}, result: {:?}, code: {:?}", node, result, code);
                if node == Some(0) && result == Some("in_progress") && code == Some(0) {
                    info!("Proof generation accepted for block number {}", block_number);
                    proving = true;
                    break;
                }
            }
        }
    }

    if !proving {
        return Err(anyhow!("Failed to start proof generation for block number {}", block_number));
    }

    let _status = child.wait()?;

    wait_prove_done().await?;

    if proving {
        let file = File::open(format!("{}/result.json", output_folder))
            .context(format!(
                "Failed to open result.json for block number {}",
                block_number
            ))?;

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
            "{}/{}/vadcop_final_proof.bin",
            OUTPUT_FOLDER, block_number
        ),
        // TODO: Uncomment when we have publics.json
        //format!("{}/{}/publics.json", OUTPUT_FOLDER, block_number),
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
