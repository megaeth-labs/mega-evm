//! The EIP-7708 transfer log revm journals for a value movement, as a test expects it.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::vec;

use alloy_primitives::{Address, Bytes, Log, LogData, U256};
use revm::primitives::eip7708::{ETH_TRANSFER_LOG_ADDRESS, ETH_TRANSFER_LOG_TOPIC};

/// The EIP-7708 transfer log of `value` wei moving from `from` to `to`: a `LOG3` from the
/// system address `0xff…fe`, carrying ERC-20's `Transfer` event with the two accounts as its
/// topics and the amount as its one word of data.
pub fn transfer_log(from: Address, to: Address, value: U256) -> Log {
    Log {
        address: ETH_TRANSFER_LOG_ADDRESS,
        data: LogData::new_unchecked(
            vec![ETH_TRANSFER_LOG_TOPIC, from.into_word(), to.into_word()],
            Bytes::copy_from_slice(&value.to_be_bytes::<32>()),
        ),
    }
}

/// Whether `log` is an EIP-7708 transfer log, rather than one a contract emitted.
pub fn is_transfer_log(log: &Log) -> bool {
    log.address == ETH_TRANSFER_LOG_ADDRESS && log.topics().first() == Some(&ETH_TRANSFER_LOG_TOPIC)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TRANSFER_LOG_SIZE;
    use alloy_primitives::address;

    /// The expected log is a `LOG3` of one word, which is what the data size counts it at.
    #[test]
    fn test_a_transfer_log_is_a_log3_of_one_word() {
        let log = transfer_log(
            address!("0000000000000000000000000000000000000001"),
            address!("0000000000000000000000000000000000000002"),
            U256::from(7),
        );
        assert!(is_transfer_log(&log));
        assert_eq!(log.topics().len(), 3);
        assert_eq!(log.data.data.len(), 32);
        assert_eq!(
            32 + 32 * log.topics().len() as u64 + log.data.data.len() as u64,
            TRANSFER_LOG_SIZE
        );
        assert_eq!(U256::from_be_slice(&log.data.data), U256::from(7));
    }
}
