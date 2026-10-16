//! Converting pesos to time. The customer always gets the best combination
//! of the owner's rates for what they paid, so inserting ₱1 five times is
//! never worse than inserting one ₱5 coin.

use crate::db::Rate;

/// Seconds of internet for `pesos`, maximised over any mix of rates.
/// Pesos that no rate fits are still credited at the smallest rate's per-peso value.
pub fn secs_for(pesos: u32, rates: &[Rate]) -> u64 {
    let Some(base) = rates.iter().min_by_key(|r| r.pesos) else { return 0 };
    let per_peso = base.minutes as u64 * 60 / base.pesos as u64;
    let p = pesos as usize;
    let mut best = vec![0u64; p + 1];
    for i in 1..=p {
        let mut b = best[i - 1] + per_peso;
        for r in rates {
            let rp = r.pesos as usize;
            if rp <= i {
                b = b.max(best[i - rp] + r.minutes as u64 * 60);
            }
        }
        best[i] = b;
    }
    best[p]
}

pub fn validate(rates: &[Rate]) -> Result<(), &'static str> {
    if rates.is_empty() {
        return Err("add at least one rate");
    }
    if rates.len() > 20 {
        return Err("at most 20 rates");
    }
    let mut seen = std::collections::HashSet::new();
    for r in rates {
        if r.pesos == 0 || r.pesos > 1000 || r.minutes == 0 || r.minutes > 60 * 24 * 60 {
            return Err("rates must be ₱1–₱1000 and 1 minute–60 days");
        }
        if !seen.insert(r.pesos) {
            return Err("each peso amount can only appear once");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(pesos: u32, minutes: u32) -> Rate {
        Rate { pesos, minutes }
    }

    #[test]
    fn picks_best_combination() {
        let rates = [r(1, 20), r(5, 120), r(10, 300)];
        assert_eq!(secs_for(1, &rates), 20 * 60);
        assert_eq!(secs_for(5, &rates), 120 * 60);
        assert_eq!(secs_for(7, &rates), (120 + 40) * 60);
        assert_eq!(secs_for(10, &rates), 300 * 60);
        assert_eq!(secs_for(15, &rates), 420 * 60);
    }

    #[test]
    fn leftover_pesos_are_not_lost() {
        // Only a ₱5 rate: ₱3 still earns 3/5 of it.
        assert_eq!(secs_for(3, &[r(5, 120)]), 3 * 24 * 60);
    }

    #[test]
    fn empty_rates_give_nothing() {
        assert_eq!(secs_for(5, &[]), 0);
    }
}
