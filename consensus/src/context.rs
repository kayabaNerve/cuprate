//! # Blockchain Context
//!
//! This module contains a service to get cached context from the blockchain: [`BlockChainContext`].
//! This is used during contextual validation, this does not have all the data for contextual validation
//! (outputs) for that you will need a [`Database`].
//!

use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use futures::FutureExt;
use tokio::sync::RwLock;
use tower::buffer::future::ResponseFuture;
use tower::{buffer::Buffer, Service, ServiceExt};

use crate::{ConsensusError, Database, DatabaseRequest, DatabaseResponse};

pub mod difficulty;
mod hardforks;
mod weight;

pub use difficulty::DifficultyCacheConfig;
pub use hardforks::{HardFork, HardForkConfig};
pub use weight::BlockWeightsCacheConfig;

const BUFFER_CONTEXT_CHANNEL_SIZE: usize = 5;

pub struct ContextConfig {
    hard_fork_cfg: HardForkConfig,
    difficulty_cfg: DifficultyCacheConfig,
    weights_config: BlockWeightsCacheConfig,
}

impl ContextConfig {
    pub fn main_net() -> ContextConfig {
        ContextConfig {
            hard_fork_cfg: HardForkConfig::main_net(),
            difficulty_cfg: DifficultyCacheConfig::main_net(),
            weights_config: BlockWeightsCacheConfig::main_net(),
        }
    }
}

pub async fn initialize_blockchain_context<D>(
    cfg: ContextConfig,
    mut database: D,
) -> Result<
    (
        impl Service<
                BlockChainContextRequest,
                Response = BlockChainContext,
                Error = tower::BoxError,
                Future = impl Future<Output = Result<BlockChainContext, tower::BoxError>>
                             + Send
                             + 'static,
            > + Clone
            + Send
            + Sync
            + 'static,
        impl Service<UpdateBlockchainCacheRequest, Response = (), Error = tower::BoxError>,
    ),
    ConsensusError,
>
where
    D: Database + Clone + Send + Sync + 'static,
    D::Future: Send + 'static,
{
    let ContextConfig {
        difficulty_cfg,
        weights_config,
        hard_fork_cfg,
    } = cfg;

    tracing::debug!("Initialising blockchain context");

    let DatabaseResponse::ChainHeight(chain_height, top_block_hash) = database
        .ready()
        .await?
        .call(DatabaseRequest::ChainHeight)
        .await?
    else {
        panic!("Database sent incorrect response!");
    };

    let DatabaseResponse::GeneratedCoins(already_generated_coins) = database
        .ready()
        .await?
        .call(DatabaseRequest::GeneratedCoins)
        .await?
    else {
        panic!("Database sent incorrect response!");
    };

    let db = database.clone();
    let difficulty_cache_handle = tokio::spawn(async move {
        difficulty::DifficultyCache::init_from_chain_height(chain_height, difficulty_cfg, db).await
    });

    let db = database.clone();
    let weight_cache_handle = tokio::spawn(async move {
        weight::BlockWeightsCache::init_from_chain_height(chain_height, weights_config, db).await
    });

    let db = database.clone();
    let hardfork_state_handle = tokio::spawn(async move {
        hardforks::HardForkState::init_from_chain_height(chain_height, hard_fork_cfg, db).await
    });

    let context_svc = BlockChainContextService {
        difficulty_cache: Arc::new(difficulty_cache_handle.await.unwrap()?.into()),
        weight_cache: Arc::new(weight_cache_handle.await.unwrap()?.into()),
        hardfork_state: Arc::new(hardfork_state_handle.await.unwrap()?.into()),
        chain_height: Arc::new(chain_height.into()),
        already_generated_coins: Arc::new(already_generated_coins.into()),
        top_block_hash: Arc::new(top_block_hash.into()),
        database,
    };

    let context_svc_update = context_svc.clone();

    let buffered_svc = Buffer::new(context_svc.boxed(), BUFFER_CONTEXT_CHANNEL_SIZE);

    Ok((buffered_svc.clone(), context_svc_update))
}

#[derive(Debug, Clone, Copy)]
pub struct BlockChainContext {
    /// The next blocks difficulty.
    next_difficulty: u128,
    /// The current cumulative difficulty.
    cumulative_difficulty: u128,
    /// The current effective median block weight.
    effective_median_weight: usize,
    /// The median long term block weight.
    median_long_term_weight: usize,
    /// Median weight to use for block reward calculations.
    pub median_weight_for_block_reward: usize,
    /// The amount of coins minted already.
    pub already_generated_coins: u64,
    /// Timestamp to use to check time locked outputs.
    time_lock_timestamp: u64,
    /// The height of the chain.
    pub chain_height: u64,
    /// The top blocks hash
    top_hash: [u8; 32],
    /// The current hard fork.
    pub current_hard_fork: HardFork,
}

#[derive(Debug, Clone)]
pub struct BlockChainContextRequest;

#[derive(Clone)]
pub struct BlockChainContextService<D> {
    difficulty_cache: Arc<RwLock<difficulty::DifficultyCache>>,
    weight_cache: Arc<RwLock<weight::BlockWeightsCache>>,
    hardfork_state: Arc<RwLock<hardforks::HardForkState>>,

    chain_height: Arc<RwLock<u64>>,
    top_block_hash: Arc<RwLock<[u8; 32]>>,
    already_generated_coins: Arc<RwLock<u64>>,

    database: D,
}

impl<D> Service<BlockChainContextRequest> for BlockChainContextService<D> {
    type Response = BlockChainContext;
    type Error = ConsensusError;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: BlockChainContextRequest) -> Self::Future {
        let hardfork_state = self.hardfork_state.clone();
        let difficulty_cache = self.difficulty_cache.clone();
        let weight_cache = self.weight_cache.clone();

        let chain_height = self.chain_height.clone();
        let top_hash = self.top_block_hash.clone();
        let already_generated_coins = self.already_generated_coins.clone();

        async move {
            let hardfork_state = hardfork_state.read().await;
            let difficulty_cache = difficulty_cache.read().await;
            let weight_cache = weight_cache.read().await;

            let current_hf = hardfork_state.current_hardfork();

            Ok(BlockChainContext {
                next_difficulty: difficulty_cache.next_difficulty(&current_hf),
                cumulative_difficulty: difficulty_cache.cumulative_difficulty(),
                effective_median_weight: weight_cache.effective_median_block_weight(&current_hf),
                median_long_term_weight: weight_cache.median_long_term_weight(),
                median_weight_for_block_reward: weight_cache.median_for_block_reward(&current_hf),
                already_generated_coins: *already_generated_coins.read().await,
                time_lock_timestamp: 0, //TODO:
                chain_height: *chain_height.read().await,
                top_hash: *top_hash.read().await,
                current_hard_fork: current_hf,
            })
        }
        .boxed()
    }
}

pub struct UpdateBlockchainCacheRequest {
    pub new_top_hash: [u8; 32],
    pub height: u64,
    pub timestamp: u64,
    pub weight: usize,
    pub long_term_weight: usize,
    pub generated_coins: u64,
    pub vote: HardFork,
}

impl<D> tower::Service<UpdateBlockchainCacheRequest> for BlockChainContextService<D>
where
    D: Database + Clone + Send + Sync + 'static,
    D::Future: Send + 'static,
{
    type Response = ();
    type Error = tower::BoxError;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.database.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, new: UpdateBlockchainCacheRequest) -> Self::Future {
        let hardfork_state = self.hardfork_state.clone();
        let difficulty_cache = self.difficulty_cache.clone();
        let weight_cache = self.weight_cache.clone();

        let chain_height = self.chain_height.clone();
        let top_hash = self.top_block_hash.clone();
        let already_generated_coins = self.already_generated_coins.clone();

        let database = self.database.clone();

        async move {
            difficulty_cache
                .write()
                .await
                .new_block(new.height, new.timestamp, database.clone())
                .await?;

            weight_cache
                .write()
                .await
                .new_block(
                    new.height,
                    new.weight,
                    new.long_term_weight,
                    database.clone(),
                )
                .await?;

            hardfork_state
                .write()
                .await
                .new_block(new.vote, new.height, database)
                .await?;

            *chain_height.write().await = new.height + 1;
            *top_hash.write().await = new.new_top_hash;
            let mut already_generated_coins = already_generated_coins.write().await;
            *already_generated_coins = already_generated_coins.saturating_add(new.generated_coins);

            Ok(())
        }
        .boxed()
    }
}
