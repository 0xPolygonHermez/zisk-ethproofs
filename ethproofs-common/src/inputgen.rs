use crate::protocol::BlockInfo;
use anyhow::Result;
use input::{InputGenerator, InputGeneratorResult, Network};
use log::info;
use url::Url;
use std::{env, io::Write, path::Path, time::Instant};

/// Generate the guest input file for the given block and return the time taken in milliseconds.
/// Reads RPC_URL from environment, spawns a blocking task to run the non-Send generation future,
/// writes the file named using `BlockInfo::filename()`.
pub async fn generate_input_file(
    block_info: BlockInfo,
    inputs_folder: String,
) -> Result<u128> {
    let start = Instant::now();
    let block_number = block_info.block_number;
    let rpc_url = Url::parse(&env::var("RPC_URL").expect("RPC_URL must be set"))?;
    let input_folder = Path::new(&inputs_folder);
    std::fs::create_dir_all(input_folder)?;
    let input_path = input_folder.join(block_info.filename());

    let start_input_gen = Instant::now();
    let result = tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Handle::current();
        rt.block_on(async move {
            let input_generator = InputGenerator::new(rpc_url, Network::Mainnet);
            input_generator.generate(block_number).await
        })
    })
    .await??;
    info!(
        "Input generation for block {} took {} ms",
        block_number,
        start_input_gen.elapsed().as_millis()
    );

    let save_file_start = Instant::now();
    let mut input_file = std::fs::File::create(&input_path)?;
    input_file.write_all(&result.input)?;
    input_file.flush()?;
    info!(
        "Saving input file for block {} took {} ms",
        block_number,
        save_file_start.elapsed().as_millis()
    );

    Ok(start.elapsed().as_millis())
}

pub async fn generate_input(
    block_info: BlockInfo,
) -> Result<(u128, InputGeneratorResult)> {
    let start = Instant::now();
    let block_number = block_info.block_number;
    let rpc_url = Url::parse(&env::var("RPC_URL").expect("RPC_URL must be set"))?;

    let start_input_gen = Instant::now();

    let input_generator = InputGenerator::new(rpc_url, Network::Mainnet);
    let result = input_generator.generate(block_number).await?;

    info!(
        "Input generation for block {} took {} ms",
        block_number,
        start_input_gen.elapsed().as_millis()
    );

    Ok((start.elapsed().as_millis(), result))
}
