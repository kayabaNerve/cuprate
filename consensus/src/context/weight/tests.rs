use super::{BlockWeightsCache, BlockWeightsCacheConfig};
use crate::test_utils::mock_db::*;

pub const TEST_WEIGHT_CONFIG: BlockWeightsCacheConfig = BlockWeightsCacheConfig::new(100, 5000);

#[tokio::test]
async fn blocks_out_of_window_not_counted() -> Result<(), tower::BoxError> {
    let mut db_builder = DummyDatabaseBuilder::default();
    for weight in 1..=5000 {
        let block = DummyBlockExtendedHeader::default().with_weight_into(weight, weight);
        db_builder.add_block(block);
    }

    let mut weight_cache =
        BlockWeightsCache::init_from_chain_height(5000, TEST_WEIGHT_CONFIG, db_builder.finish())
            .await?;
    assert_eq!(weight_cache.median_long_term_weight(), 2500);
    assert_eq!(weight_cache.median_short_term_weight(), 4950);

    weight_cache.new_block(5000, 0, 0);
    weight_cache.new_block(5001, 0, 0);
    weight_cache.new_block(5002, 0, 0);

    // if blocks outside the window were not removed adding the blocks above would have pulled the median down.
    assert_eq!(weight_cache.median_long_term_weight(), 2500);
    assert_eq!(weight_cache.median_short_term_weight(), 4950);

    Ok(())
}

#[tokio::test]
async fn weight_cache_calculates_correct_median() -> Result<(), tower::BoxError> {
    let mut db_builder = DummyDatabaseBuilder::default();
    // add an initial block as otherwise this will panic.
    let block = DummyBlockExtendedHeader::default().with_weight_into(0, 0);
    db_builder.add_block(block);

    let mut weight_cache =
        BlockWeightsCache::init_from_chain_height(1, TEST_WEIGHT_CONFIG, db_builder.finish())
            .await?;

    for height in 1..=100 {
        weight_cache.new_block(height as u64, height, height);

        assert_eq!(weight_cache.median_short_term_weight(), height / 2);
        assert_eq!(weight_cache.median_long_term_weight(), height / 2);
    }

    for height in 101..=5000 {
        weight_cache.new_block(height as u64, height, height);

        assert_eq!(weight_cache.median_long_term_weight(), height / 2);
    }
    Ok(())
}

// TODO: protests
