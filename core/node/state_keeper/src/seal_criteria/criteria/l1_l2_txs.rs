use zksync_types::{
    aggregated_operations::{L1_BATCH_EXECUTE_BASE_COST, L1_OPERATION_EXECUTE_COST},
    ProtocolVersionId,
};

use crate::seal_criteria::{SealCriterion, SealData, SealResolution, StateKeeperConfig};

#[derive(Debug)]
pub(crate) struct L1L2TxsCriterion;

impl SealCriterion for L1L2TxsCriterion {
    fn should_seal(
        &self,
        config: &StateKeeperConfig,
        _block_open_timestamp_ms: u128,
        _tx_count: usize,
        l1_tx_count: usize,
        _block_data: &SealData,
        _tx_data: &SealData,
        _protocol_version_id: ProtocolVersionId,
    ) -> SealResolution {
        let block_l1_gas_bound =
            (config.max_single_tx_gas as f64 * config.close_block_at_gas_percentage).round() as u32;
        let l1_gas = L1_BATCH_EXECUTE_BASE_COST + (l1_tx_count as u32) * L1_OPERATION_EXECUTE_COST;

        if l1_gas >= block_l1_gas_bound {
            SealResolution::IncludeAndSeal
        } else {
            SealResolution::NoSeal
        }
    }

    fn prom_criterion_name(&self) -> &'static str {
        "gas"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_l1_l2_txs_seal_criterion() {
        let max_single_tx_gas = 15_000_000;
        let close_block_at_gas_percentage = 0.95;

        let gas_bound = (max_single_tx_gas as f64 * close_block_at_gas_percentage).round() as u32;
        let l1_tx_count_bound =
            (gas_bound - L1_BATCH_EXECUTE_BASE_COST - 1) / L1_OPERATION_EXECUTE_COST;

        // Create an empty config and only setup fields relevant for the test.
        let config = StateKeeperConfig {
            max_single_tx_gas,
            close_block_at_gas_percentage,
            ..Default::default()
        };

        let criterion = L1L2TxsCriterion;

        // Empty block should fit into gas criterion.
        let empty_block_resolution = criterion.should_seal(
            &config,
            0,
            0,
            0,
            &SealData::default(),
            &SealData::default(),
            ProtocolVersionId::latest(),
        );
        assert_eq!(empty_block_resolution, SealResolution::NoSeal);

        // `l1_tx_count_bound` should return `NoSeal`.
        let block_resolution = criterion.should_seal(
            &config,
            0,
            0,
            l1_tx_count_bound as usize,
            &SealData::default(),
            &SealData::default(),
            ProtocolVersionId::latest(),
        );
        assert_eq!(block_resolution, SealResolution::NoSeal);

        // `l1_tx_count_bound + 1` should return `IncludeAndSeal`.
        let block_resolution = criterion.should_seal(
            &config,
            0,
            0,
            l1_tx_count_bound as usize + 1,
            &SealData::default(),
            &SealData::default(),
            ProtocolVersionId::latest(),
        );
        assert_eq!(block_resolution, SealResolution::IncludeAndSeal);
    }
}
