use std::{fs::{create_dir_all, File}, io::{BufRead, BufReader, Write}, path::Path, process::{Command,Stdio}};

use anyhow::{Ok, Result};
use log::debug;

use crate::{state::AppState, LOG_FOLDER, OUTPUT_FOLDER};

pub async fn generate_proof(block_number: u64, state: AppState) -> Result<()> {
    let input_file = format!("{}.bin", block_number);

    create_dir_all(Path::new(OUTPUT_FOLDER))?;
    create_dir_all(Path::new(LOG_FOLDER))?;

    let command = format!("zisk-coordinator prove --input {} --compute-capacity {}", input_file, state.compute_capacity);
    debug!("Starting proof generation for block number {} with command: {}", block_number, command);
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;

    let stdout = child.stdout.take().expect("Failed to capture stdout");
    let reader = BufReader::new(stdout);
    debug!("Waiting for proof generation to start for block number {}", block_number);

    let _status = child.wait()?;

    let mut captured_output = Vec::new();
    let mut job_id = None;
    for line in reader.lines() {
        let line = line?;
        debug!("{}", line);
        captured_output.push(format!("{}\n", line));

        let prefix = "Proof job started successfully with job_id:";
        if let Some(pos) = line.find(prefix) {
            job_id = Some(line[pos + prefix.len()..].trim().to_string());
        }
    }

    // If job_id is None, it means proof generation did not start successfully
    if job_id.is_none() {
        let log_path = format!("{}/{}.log", LOG_FOLDER, block_number);
        let mut log_file = File::create(&log_path)?;
        write!(log_file, "{}", captured_output.concat())?;

        debug!("Proof generation failed to start for block number {}, log saved at {}", block_number, log_path);
        //return Err(anyhow!("Failed to start proof generation for block number {}. Log saved at {}", block_number, log_path));
        return Ok(());
    }

    debug!("Proof generation started for block number {}, job_id: {}", block_number, job_id.as_ref().unwrap());

    Ok(())
}
