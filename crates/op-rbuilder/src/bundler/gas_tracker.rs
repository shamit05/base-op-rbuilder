//! Gas tracking and reservation for AA bundles

/// Tracks gas usage and determines when to reserve gas for AA bundles.
#[derive(Debug, Clone)]
pub struct GasTracker {
    /// Total block gas limit
    block_gas_limit: u64,
    /// Percentage of block gas at which to stop regular tx processing (0-100)
    threshold_percentage: u8,
    /// Percentage of block gas reserved for AA bundles (0-100)
    reserve_percentage: u8,
}

/// Result of checking gas reservation
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GasReservation {
    /// Whether the threshold has been reached and we should stop regular tx processing
    pub threshold_reached: bool,
    /// Gas limit available for regular transactions
    pub available_for_txs: u64,
    /// Gas reserved for AA bundles
    pub reserved_for_bundles: u64,
}

impl GasTracker {
    /// Create a new gas tracker.
    ///
    /// # Arguments
    /// * `block_gas_limit` - Total gas limit for the block
    /// * `threshold_percentage` - Percentage of gas at which to stop regular txs (e.g., 80)
    /// * `reserve_percentage` - Percentage of gas to reserve for bundles (e.g., 20)
    pub fn new(block_gas_limit: u64, threshold_percentage: u8, reserve_percentage: u8) -> Self {
        Self {
            block_gas_limit,
            threshold_percentage: threshold_percentage.min(100),
            reserve_percentage: reserve_percentage.min(100),
        }
    }

    /// Check gas reservation status given current cumulative gas used.
    ///
    /// Returns information about whether the threshold has been reached and
    /// how much gas is available for regular transactions vs bundles.
    pub fn check_reservation(&self, cumulative_gas_used: u64) -> GasReservation {
        let threshold_gas = self.calculate_threshold_gas();
        let reserved_gas = self.calculate_reserved_gas();

        let threshold_reached = cumulative_gas_used >= threshold_gas;
        let available_for_txs = threshold_gas.saturating_sub(cumulative_gas_used);

        GasReservation {
            threshold_reached,
            available_for_txs,
            reserved_for_bundles: reserved_gas,
        }
    }

    /// Calculate the gas threshold at which to stop regular tx processing.
    pub fn calculate_threshold_gas(&self) -> u64 {
        (self.block_gas_limit as u128 * self.threshold_percentage as u128 / 100) as u64
    }

    /// Calculate the gas reserved for AA bundles.
    pub fn calculate_reserved_gas(&self) -> u64 {
        (self.block_gas_limit as u128 * self.reserve_percentage as u128 / 100) as u64
    }

    /// Check if a transaction can fit within the remaining available gas.
    ///
    /// # Arguments
    /// * `cumulative_gas_used` - Current total gas used in the block
    /// * `tx_gas_limit` - Gas limit of the transaction to check
    ///
    /// # Returns
    /// `true` if the transaction can fit, `false` if it would exceed the threshold
    pub fn can_fit_transaction(&self, cumulative_gas_used: u64, tx_gas_limit: u64) -> bool {
        let reservation = self.check_reservation(cumulative_gas_used);
        !reservation.threshold_reached && tx_gas_limit <= reservation.available_for_txs
    }

    /// Get the block gas limit
    pub fn block_gas_limit(&self) -> u64 {
        self.block_gas_limit
    }

    /// Get the threshold percentage
    pub fn threshold_percentage(&self) -> u8 {
        self.threshold_percentage
    }

    /// Get the reserve percentage
    pub fn reserve_percentage(&self) -> u8 {
        self.reserve_percentage
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gas_tracker_creation() {
        let tracker = GasTracker::new(30_000_000, 80, 20);
        assert_eq!(tracker.block_gas_limit(), 30_000_000);
        assert_eq!(tracker.threshold_percentage(), 80);
        assert_eq!(tracker.reserve_percentage(), 20);
    }

    #[test]
    fn test_percentage_clamping() {
        let tracker = GasTracker::new(30_000_000, 150, 200);
        assert_eq!(tracker.threshold_percentage(), 100);
        assert_eq!(tracker.reserve_percentage(), 100);
    }

    #[test]
    fn test_threshold_calculation() {
        let tracker = GasTracker::new(30_000_000, 80, 20);
        // 80% of 30M = 24M
        assert_eq!(tracker.calculate_threshold_gas(), 24_000_000);
        // 20% of 30M = 6M
        assert_eq!(tracker.calculate_reserved_gas(), 6_000_000);
    }

    #[test]
    fn test_reservation_not_reached() {
        let tracker = GasTracker::new(30_000_000, 80, 20);
        let reservation = tracker.check_reservation(10_000_000);

        assert!(!reservation.threshold_reached);
        assert_eq!(reservation.available_for_txs, 14_000_000); // 24M - 10M
        assert_eq!(reservation.reserved_for_bundles, 6_000_000);
    }

    #[test]
    fn test_reservation_reached() {
        let tracker = GasTracker::new(30_000_000, 80, 20);
        let reservation = tracker.check_reservation(24_000_000);

        assert!(reservation.threshold_reached);
        assert_eq!(reservation.available_for_txs, 0);
        assert_eq!(reservation.reserved_for_bundles, 6_000_000);
    }

    #[test]
    fn test_reservation_exceeded() {
        let tracker = GasTracker::new(30_000_000, 80, 20);
        let reservation = tracker.check_reservation(25_000_000);

        assert!(reservation.threshold_reached);
        assert_eq!(reservation.available_for_txs, 0);
        assert_eq!(reservation.reserved_for_bundles, 6_000_000);
    }

    #[test]
    fn test_can_fit_transaction() {
        let tracker = GasTracker::new(30_000_000, 80, 20);

        // With 10M used, we have 14M available
        assert!(tracker.can_fit_transaction(10_000_000, 5_000_000));
        assert!(tracker.can_fit_transaction(10_000_000, 14_000_000));
        assert!(!tracker.can_fit_transaction(10_000_000, 15_000_000));

        // At threshold, nothing fits
        assert!(!tracker.can_fit_transaction(24_000_000, 1));
    }

    #[test]
    fn test_zero_percentages() {
        let tracker = GasTracker::new(30_000_000, 0, 0);
        let reservation = tracker.check_reservation(0);

        assert!(reservation.threshold_reached);
        assert_eq!(reservation.available_for_txs, 0);
        assert_eq!(reservation.reserved_for_bundles, 0);
    }

    #[test]
    fn test_full_percentages() {
        let tracker = GasTracker::new(30_000_000, 100, 100);

        assert_eq!(tracker.calculate_threshold_gas(), 30_000_000);
        assert_eq!(tracker.calculate_reserved_gas(), 30_000_000);

        let reservation = tracker.check_reservation(0);
        assert!(!reservation.threshold_reached);
        assert_eq!(reservation.available_for_txs, 30_000_000);
    }
}




