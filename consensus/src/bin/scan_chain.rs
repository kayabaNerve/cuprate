#![cfg(feature = "binaries")]

use std::{ops::Range, path::PathBuf, sync::Arc};

use clap::Parser;
use futures::{
    channel::{mpsc, oneshot},
    SinkExt, StreamExt,
};
use monero_serai::block::Block;
use tokio::sync::RwLock;
use tower::{Service, ServiceExt};
use tracing::level_filters::LevelFilter;

use cuprate_common::Network;

use monero_consensus::{
    context::{
        BlockChainContextRequest, BlockChainContextResponse, ContextConfig,
        UpdateBlockchainCacheData,
    },
    initialize_blockchain_context, initialize_verifier,
    rpc::{cache::ScanningCache, init_rpc_load_balancer, RpcConfig},
    Database, DatabaseRequest, DatabaseResponse, VerifiedBlockInformation, VerifyBlockRequest,
    VerifyBlockResponse,
};

mod tx_pool;

const MAX_BLOCKS_IN_RANGE: u64 = 1000;
const MAX_BLOCKS_HEADERS_IN_RANGE: u64 = 1000;

/// Calls for a batch of blocks, returning the response and the time it took.
async fn call_batch<D: Database>(
    range: Range<u64>,
    database: D,
) -> Result<DatabaseResponse, tower::BoxError> {
    database
        .oneshot(DatabaseRequest::BlockBatchInRange(range))
        .await
}

async fn update_cache_and_context<Ctx>(
    cache: &RwLock<ScanningCache>,
    context_updater: &mut Ctx,
    verified_block_info: VerifiedBlockInformation,
) -> Result<(), tower::BoxError>
where
    Ctx: tower::Service<
        BlockChainContextRequest,
        Response = BlockChainContextResponse,
        Error = tower::BoxError,
    >,
{
    // add the new block to the cache
    cache.write().await.add_new_block_data(
        verified_block_info.generated_coins,
        &verified_block_info.block.miner_tx,
        &verified_block_info.txs,
    );
    // update the chain context svc with the new block
    context_updater
        .ready()
        .await?
        .call(BlockChainContextRequest::Update(
            UpdateBlockchainCacheData {
                new_top_hash: verified_block_info.block_hash,
                height: verified_block_info.height,
                timestamp: verified_block_info.block.header.timestamp,
                weight: verified_block_info.weight,
                long_term_weight: verified_block_info.long_term_weight,
                vote: verified_block_info.hf_vote,
                generated_coins: verified_block_info.generated_coins,
                cumulative_difficulty: verified_block_info.cumulative_difficulty,
            },
        ))
        .await?;

    Ok(())
}

async fn call_blocks<D>(
    mut new_tx_chan: tx_pool::NewTxChanSen,
    mut block_chan: mpsc::Sender<Vec<Block>>,
    start_height: u64,
    chain_height: u64,
    database: D,
) -> Result<(), tower::BoxError>
where
    D: Database + Clone + Send + Sync + 'static,
    D::Future: Send + 'static,
{
    let mut next_fut = tokio::spawn(call_batch(
        start_height..(start_height + (MAX_BLOCKS_IN_RANGE * 3)).min(chain_height),
        database.clone(),
    ));

    for next_batch_start in (start_height..chain_height)
        .step_by((MAX_BLOCKS_IN_RANGE * 3) as usize)
        .skip(1)
    {
        // Call the next batch while we handle this batch.
        let current_fut = std::mem::replace(
            &mut next_fut,
            tokio::spawn(call_batch(
                next_batch_start..(next_batch_start + (MAX_BLOCKS_IN_RANGE * 3)).min(chain_height),
                database.clone(),
            )),
        );

        let DatabaseResponse::BlockBatchInRange(blocks) = current_fut.await?? else {
            panic!("Database sent incorrect response!");
        };

        tracing::info!(
            "Retrived batch: {:?}, chain height: {}",
            (next_batch_start - (MAX_BLOCKS_IN_RANGE * 3))..(next_batch_start),
            chain_height
        );

        let (blocks, txs): (Vec<_>, Vec<_>) = blocks.into_iter().unzip();

        let (tx, rx) = oneshot::channel();
        new_tx_chan
            .send((txs.into_iter().flatten().collect(), tx))
            .await?;
        rx.await??;

        block_chan.send(blocks).await?;
    }

    Ok(())
}

async fn scan_chain<D>(
    cache: Arc<RwLock<ScanningCache>>,
    save_file: PathBuf,
    _rpc_config: Arc<std::sync::RwLock<RpcConfig>>,
    database: D,
) -> Result<(), tower::BoxError>
where
    D: Database + Clone + Send + Sync + 'static,
    D::Future: Send + 'static,
{
    tracing::info!("Beginning chain scan");

    // TODO: when we implement all rules use the RPCs chain height, for now we don't check v2 txs.
    let chain_height = 3_000_000;

    tracing::info!("scanning to chain height: {}", chain_height);

    let config = ContextConfig::main_net();

    let mut ctx_svc = initialize_blockchain_context(config, database.clone()).await?;

    let (tx, rx) = oneshot::channel();

    let (tx_pool_svc, new_tx_chan) = tx_pool::TxPool::spawn(rx, ctx_svc.clone()).await?;

    let (mut block_verifier, transaction_verifier) =
        initialize_verifier(database.clone(), tx_pool_svc, ctx_svc.clone()).await?;

    tx.send(transaction_verifier).map_err(|_| "").unwrap();

    let start_height = cache.read().await.height;

    let (block_tx, mut incoming_blocks) = mpsc::channel(3);

    tokio::spawn(async move {
        call_blocks(new_tx_chan, block_tx, start_height, chain_height, database).await
    });

    let (mut prepared_blocks_tx, mut prepared_blocks_rx) = mpsc::channel(3);

    let mut cloned_block_verifier = block_verifier.clone();
    tokio::spawn(async move {
        while let Some(mut next_blocks) = incoming_blocks.next().await {
            while !next_blocks.is_empty() {
                tracing::info!(
                    "preparing next batch, number of blocks: {}",
                    next_blocks.len().min(150)
                );

                let res = cloned_block_verifier
                    .ready()
                    .await?
                    .call(VerifyBlockRequest::BatchSetup(
                        next_blocks.drain(0..next_blocks.len().min(150)).collect(),
                    ))
                    .await;

                prepared_blocks_tx.send(res).await.unwrap();
            }
        }

        Result::<_, tower::BoxError>::Ok(())
    });

    while let Some(prepared_blocks) = prepared_blocks_rx.next().await {
        let VerifyBlockResponse::BatchSetup(prepared_blocks) = prepared_blocks? else {
            panic!("block verifier sent incorrect response!");
        };
        let mut height = 0;
        for block in prepared_blocks {
            let VerifyBlockResponse::MainChain(verified_block_info) = block_verifier
                .ready()
                .await?
                .call(VerifyBlockRequest::MainChainPreparedBlock(block))
                .await?
            else {
                panic!("Block verifier sent incorrect response!");
            };

            height = verified_block_info.height;

            if verified_block_info.height % 5000 == 0 {
                tracing::info!("saving cache to: {}", save_file.display());
                cache.write().await.save(&save_file).unwrap();
            }

            update_cache_and_context(&cache, &mut ctx_svc, verified_block_info).await?;
        }

        tracing::info!(
            "verified blocks: {:?}, chain height: {}",
            0..height,
            chain_height
        );
    }

    Ok(())
}

#[derive(Parser)]
struct Args {
    /// The log level, valid values:
    /// "off", "error", "warn", "info", "debug", "trace", or a number 0-5.
    #[arg(short, long, default_value = "info")]
    log_level: LevelFilter,
    /// The network we should scan, valid values:
    /// "mainnet", "testnet", "stagenet".
    #[arg(short, long, default_value = "mainnet")]
    network: String,
    /// A list of RPC nodes we should use.
    /// Example: http://xmr-node.cakewallet.com:18081
    #[arg(long)]
    rpc_nodes: Vec<String>,
    /// Stops the scanner from including the default list of nodes, this is not
    /// recommended unless you have sufficient self defined nodes with `rpc_nodes`
    #[arg(long)]
    dont_use_default_nodes: bool,
    /// The directory/ folder to save the scanning cache in.
    /// This will default to your user cache directory.
    #[arg(long)]
    cache_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    if args.dont_use_default_nodes & args.rpc_nodes.is_empty() {
        panic!("Can't run scanner with no RPC nodes, see `--help` ")
    }

    tracing_subscriber::fmt()
        .with_max_level(args.log_level)
        .init();

    let network = match args.network.as_str() {
        "mainnet" => Network::Mainnet,
        _ => panic!("Invalid network, scanner currently only supports mainnet"),
    };

    let mut file_for_cache = match args.cache_dir {
        Some(dir) => dir,
        None => dirs::cache_dir().unwrap(),
    };
    file_for_cache.push("cuprate_rpc_scanning_cache.bin");

    let mut urls = if args.dont_use_default_nodes {
        vec![]
    } else {
        vec![
            "http://xmr-node.cakewallet.com:18081".to_string(),
            "https://node.sethforprivacy.com".to_string(),
            "http://nodex.monerujo.io:18081".to_string(),
            "http://nodes.hashvault.pro:18081".to_string(),
            "http://node.c3pool.com:18081".to_string(),
            "http://node.trocador.app:18089".to_string(),
            "http://xmr.lukas.services:18089".to_string(),
            "http://xmr-node-eu.cakewallet.com:18081".to_string(),
            "http://38.105.209.54:18089".to_string(),
            "http://68.118.241.70:18089".to_string(),
            "http://145.239.97.211:18089".to_string(),
            //
            "http://xmr-node.cakewallet.com:18081".to_string(),
            "https://node.sethforprivacy.com".to_string(),
            "http://nodex.monerujo.io:18081".to_string(),
            "http://nodes.hashvault.pro:18081".to_string(),
            "http://node.c3pool.com:18081".to_string(),
            "http://node.trocador.app:18089".to_string(),
            "http://xmr.lukas.services:18089".to_string(),
            "http://xmr-node-eu.cakewallet.com:18081".to_string(),
            "http://38.105.209.54:18089".to_string(),
            "http://68.118.241.70:18089".to_string(),
            "http://145.239.97.211:18089".to_string(),
        ]
    };

    urls.extend(args.rpc_nodes.into_iter());

    let rpc_config = RpcConfig::new(MAX_BLOCKS_IN_RANGE, MAX_BLOCKS_HEADERS_IN_RANGE);
    let rpc_config = Arc::new(std::sync::RwLock::new(rpc_config));

    tracing::info!("Attempting to open cache at: {}", file_for_cache.display());
    let cache = match ScanningCache::load(&file_for_cache) {
        Ok(cache) => {
            tracing::info!("Reloaded from cache, chain height: {}", cache.height);
            Arc::new(RwLock::new(cache))
        }
        Err(_) => {
            tracing::warn!("Couldn't load from cache starting from scratch");
            let mut cache = ScanningCache::default();
            let genesis = monero_consensus::genesis::generate_genesis_block(&network);

            let total_outs = genesis
                .miner_tx
                .prefix
                .outputs
                .iter()
                .map(|out| out.amount.unwrap_or(0))
                .sum::<u64>();

            cache.add_new_block_data(total_outs, &genesis.miner_tx, &[]);
            Arc::new(RwLock::new(cache))
        }
    };

    let rpc = init_rpc_load_balancer(urls, cache.clone(), rpc_config.clone());

    scan_chain(cache, file_for_cache, rpc_config, rpc)
        .await
        .unwrap();
}
