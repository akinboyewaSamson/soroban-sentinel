//! Pure rent runway and reserve budget calculations.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::{Durability, EntryType};

/// Approximate Soroban ledger closes per day (5 seconds per ledger).
pub const LEDGERS_PER_DAY: u32 = 17_280;
/// Number of stroops in one XLM.
pub const STROOPS_PER_XLM: u128 = 10_000_000;

/// Entry size and classification used as input to a rent budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetEntry {
    pub key_identifier: String,
    pub entry_type: EntryType,
    pub durability: Durability,
    pub byte_size: usize,
}

impl BudgetEntry {
    pub fn new(
        key_identifier: impl Into<String>,
        entry_type: EntryType,
        durability: Durability,
        byte_size: usize,
    ) -> Self {
        Self {
            key_identifier: key_identifier.into(),
            entry_type,
            durability,
            byte_size,
        }
    }
}

/// Itemized rent estimate for one ledger entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetLineItem {
    pub key_identifier: String,
    pub entry_type: EntryType,
    pub durability: Durability,
    pub byte_size: usize,
    pub rent_stroops: u128,
    pub rent_xlm: String,
}

/// Total reserve estimate for a target horizon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetReport {
    pub horizon_days: u32,
    pub horizon_ledgers: u32,
    pub fee_rate_stroops_per_byte_ledger: u64,
    pub entries: Vec<BudgetLineItem>,
    pub total_stroops: u128,
    pub total_xlm: String,
}

/// Errors raised while validating or calculating a budget.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BudgetError {
    #[error("horizon days must be greater than 0")]
    InvalidHorizon,
    #[error("rent estimate overflows the supported stroop range")]
    Overflow,
}

/// Calculates fee = bytes × horizon ledgers × network rate for each entry.
pub fn calculate_budget(
    entries: &[BudgetEntry],
    horizon_days: u32,
    fee_rate_stroops_per_byte_ledger: u64,
) -> Result<BudgetReport, BudgetError> {
    if horizon_days == 0 {
        return Err(BudgetError::InvalidHorizon);
    }
    let horizon_ledgers = horizon_days
        .checked_mul(LEDGERS_PER_DAY)
        .ok_or(BudgetError::Overflow)?;
    let line_items = entries
        .iter()
        .map(|entry| {
            let rent_stroops = (entry.byte_size as u128)
                .checked_mul(horizon_ledgers as u128)
                .and_then(|amount| amount.checked_mul(fee_rate_stroops_per_byte_ledger as u128))
                .ok_or(BudgetError::Overflow)?;
            Ok(BudgetLineItem {
                key_identifier: entry.key_identifier.clone(),
                entry_type: entry.entry_type,
                durability: entry.durability,
                byte_size: entry.byte_size,
                rent_stroops,
                rent_xlm: stroops_to_xlm(rent_stroops),
            })
        })
        .collect::<Result<Vec<_>, BudgetError>>()?;
    let total_stroops = line_items.iter().try_fold(0u128, |total, item| {
        total
            .checked_add(item.rent_stroops)
            .ok_or(BudgetError::Overflow)
    })?;

    Ok(BudgetReport {
        horizon_days,
        horizon_ledgers,
        fee_rate_stroops_per_byte_ledger,
        entries: line_items,
        total_stroops,
        total_xlm: stroops_to_xlm(total_stroops),
    })
}

fn stroops_to_xlm(stroops: u128) -> String {
    format!(
        "{}.{:07}",
        stroops / STROOPS_PER_XLM,
        stroops % STROOPS_PER_XLM
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calculates_itemized_budgets_for_target_horizons() {
        let entries = [BudgetEntry::new(
            "persistent:COUNTER",
            EntryType::PersistentData,
            Durability::Persistent,
            64,
        )];
        for days in [30, 90, 180, 365] {
            let report = calculate_budget(&entries, days, 2).expect("valid budget");
            let expected = 64u128 * (days as u128 * LEDGERS_PER_DAY as u128) * 2;
            assert_eq!(report.entries[0].rent_stroops, expected);
            assert_eq!(report.total_stroops, expected);
            assert_eq!(report.total_xlm, stroops_to_xlm(expected));
        }
    }

    #[test]
    fn empty_storage_and_invalid_horizons_are_handled() {
        let report = calculate_budget(&[], 30, 1).expect("empty storage");
        assert!(report.entries.is_empty());
        assert_eq!(report.total_stroops, 0);
        assert_eq!(report.total_xlm, "0.0000000");
        assert_eq!(
            calculate_budget(&[], 0, 1).unwrap_err(),
            BudgetError::InvalidHorizon
        );
    }

    #[test]
    fn rejects_horizon_and_cost_overflow() {
        assert_eq!(
            calculate_budget(&[], u32::MAX, 1).unwrap_err(),
            BudgetError::Overflow
        );
        let huge = [BudgetEntry::new(
            "huge",
            EntryType::ContractCode,
            Durability::Code,
            usize::MAX,
        )];
        assert_eq!(
            calculate_budget(&huge, 365, u64::MAX).unwrap_err(),
            BudgetError::Overflow
        );
    }
//! Fee projections for batched TTL renewals over a common ledger horizon.

use serde::Serialize;

use super::SECONDS_PER_DAY;

/// Fixed planning rates, in stroops, for persistent Soroban storage.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct BudgetRates {
    pub rent_rate_per_kib: u64,
    pub persistent_rent_denominator: u32,
    /// All modeled non-rent costs for each transaction, including TTL writes.
    pub transaction_fee: u64,
    pub base_reserve: u64,
}

/// One live entry, with already-paid TTL expressed as ledgers from the scan.
#[derive(Debug, Clone, Copy)]
pub struct BudgetEntry {
    pub byte_size: u32,
    pub prepaid_ledgers: u32,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ExtensionBudget {
    pub transactions: u32,
    pub rent_stroops: u128,
    pub transaction_stroops: u128,
    pub total_stroops: u128,
}

/// Convert a wall-clock target to ledgers, rounding up to cover the full duration.
pub fn duration_ledgers(days: u32, ledger_seconds: f64) -> Option<u32> {
    let ledgers = (f64::from(days) * SECONDS_PER_DAY / ledger_seconds).ceil();
    (1.0..=f64::from(u32::MAX))
        .contains(&ledgers)
        .then_some(ledgers as u32)
}

/// Model renewal to checkpoints spaced by cadence, with the last bump clipped to the horizon.
/// Entries needing renewal at the same checkpoint share one transaction.
pub fn extension_budget(
    entries: &[BudgetEntry],
    horizon: u32,
    cadence: u32,
    rates: BudgetRates,
) -> Option<ExtensionBudget> {
    if cadence == 0 || rates.persistent_rent_denominator == 0 {
        return None;
    }

    let checkpoints = horizon.div_ceil(cadence);
    let mut transactions = 0;
    let mut rent_stroops = 0;
    for entry in entries {
        if entry.prepaid_ledgers >= horizon {
            continue;
        }

        let skipped = entry.prepaid_ledgers / cadence;
        let count = checkpoints - skipped;
        transactions = transactions.max(count);
        let rent = |ledgers: u32| {
            (u128::from(entry.byte_size)
                * u128::from(ledgers)
                * u128::from(rates.rent_rate_per_kib))
            .div_ceil(1024 * u128::from(rates.persistent_rent_denominator))
        };

        if count == 1 {
            rent_stroops += rent(horizon - entry.prepaid_ledgers);
        } else {
            let first = cadence - entry.prepaid_ledgers % cadence;
            let last = (horizon - 1) % cadence + 1;
            rent_stroops += rent(first) + u128::from(count - 2) * rent(cadence) + rent(last);
        }
    }

    let transaction_stroops = u128::from(transactions) * u128::from(rates.transaction_fee);
    Some(ExtensionBudget {
        transactions,
        rent_stroops,
        transaction_stroops,
        total_stroops: rent_stroops + transaction_stroops,
    })
}

/// Smallest non-negative whole-stroop transaction fee at which the long plan
/// costs no more than the short plan. Equal transaction counts have no threshold.
pub fn break_even_transaction_fee(short: ExtensionBudget, long: ExtensionBudget) -> Option<u128> {
    let extra_transactions = short.transactions.checked_sub(long.transactions)?;
    if extra_transactions == 0 {
        return None;
    }
    Some(
        long.rent_stroops
            .saturating_sub(short.rent_stroops)
            .div_ceil(u128::from(extra_transactions)),
    )
}
