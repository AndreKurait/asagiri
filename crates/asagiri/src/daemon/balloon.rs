//! Memory targets reserve the guest's working set and wait for pressure to
//! settle before reclaiming. This policy uses supplied samples and time so
//! it can be checked without starting a VM.

use std::time::{Duration, Instant};

const HEADROOM: u64 = 256 << 20;
const FLOOR: u64 = 256 << 20;
const GROW: u64 = 1 << 30;
const SHRINK: u64 = 128 << 20;
const MIN_CHANGE: u64 = 64 << 20;
const QUIET: Duration = Duration::from_secs(30);
const REFAULT_THRESHOLD: u64 = 256;

/// VZ targets include physical memory that Linux excludes from MemTotal.
/// Reserve that difference alongside the cgroup's measured working set.
/// An older guest can omit the working-set sample or report no total.
pub fn working_set_target(working_kib: u64, total_kib: u64, ceiling: u64) -> u64 {
    let working = working_kib.saturating_mul(1024);
    if working == 0 || total_kib == 0 { return working; }
    working.saturating_add(ceiling.saturating_sub(total_kib.saturating_mul(1024)))
}

pub struct Balloon {
    pub target: u64,
    quiet_until: Instant,
    refaults: Option<u64>,
    oom_kills: Option<u64>,
}

impl Balloon {
    pub fn new(ceiling: u64, now: Instant) -> Self {
        Self { target: ceiling, quiet_until: now + QUIET, refaults: None, oom_kills: None }
    }

    fn change(&mut self, next: u64) -> Option<u64> {
        if next.abs_diff(self.target) < MIN_CHANGE { return None; }
        self.target = next;
        Some(next)
    }

    pub fn relieve(&mut self, ceiling: u64, now: Instant) {
        self.target = ceiling;
        self.quiet_until = now + QUIET;
    }

    pub fn pressure(&mut self, ceiling: u64, now: Instant) -> Option<u64> {
        self.quiet_until = now + QUIET;
        self.change(self.target.saturating_add(GROW).min(ceiling))
    }

    pub fn sample(&mut self, used: u64, working_set: u64, refaults: u64,
        stall: u64, oom_kills: u64, ceiling: u64, now: Instant) -> Option<u64>
    {
        let refaulting = self.refaults.replace(refaults)
            .is_some_and(|previous| refaults.saturating_sub(previous) >= REFAULT_THRESHOLD);
        let killed = self.oom_kills.replace(oom_kills).is_some_and(|previous| oom_kills > previous);
        let mut want = used.max(working_set).saturating_add(HEADROOM).clamp(FLOOR.min(ceiling), ceiling);
        if refaulting || killed || stall >= 100 {
            // PSI is hundredths of a percent. Material refaults also catch
            // file-cache thrashing before the guest reaches full-stall PSI.
            self.quiet_until = now + QUIET;
            want = want.max(self.target.saturating_add(GROW).min(ceiling));
        } else if want > self.target {
            self.quiet_until = now + QUIET;
        }
        if self.target > ceiling {
            self.target = ceiling;
            return Some(ceiling);
        }
        if want < self.target {
            if now < self.quiet_until { return None; }
            want = want.max(self.target.saturating_sub(SHRINK));
        }
        self.change(want)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;

    #[test]
    fn vz_target_accounts_for_pages_outside_linux_memtotal() {
        let ceiling = 8 * GIB;
        let total_kib = 8_211_244;
        let working_kib = 544_764;
        assert_eq!(working_set_target(working_kib, total_kib, ceiling),
            working_kib * 1024 + ceiling - total_kib * 1024);
        assert_eq!(working_set_target(working_kib, 0, ceiling), working_kib * 1024);
        assert_eq!(working_set_target(0, total_kib, ceiling), 0);
        assert_eq!(working_set_target(working_kib, u64::MAX, ceiling), working_kib * 1024);
    }

    #[test]
    fn mapped_file_working_set_survives_low_anonymous_usage() {
        let now = Instant::now();
        let mut balloon = Balloon::new(8 * GIB, now);
        for second in 30..160 {
            balloon.sample(64 * MIB, 2 * GIB, 0, 0, 0, 8 * GIB, now + Duration::from_secs(second));
            assert!(balloon.target >= 2 * GIB + HEADROOM);
        }
        assert_eq!(balloon.target, 2 * GIB + HEADROOM);
    }

    #[test]
    fn pressure_growth_is_not_undone_by_the_next_sample() {
        let now = Instant::now();
        let mut balloon = Balloon::new(GIB, now);
        let next = balloon.pressure(8 * GIB, now + Duration::from_secs(35)).unwrap();
        assert_eq!(next, 2 * GIB);
        assert_eq!(balloon.sample(64 * MIB, 64 * MIB, 0, 0, 0, 8 * GIB, now + Duration::from_secs(36)), None);
        assert_eq!(balloon.sample(64 * MIB, 64 * MIB, 0, 0, 0, 8 * GIB, now + Duration::from_secs(66)), Some(next - SHRINK));
    }

    #[test]
    fn refaults_and_ooms_grow_and_extend_the_quiet_period() {
        let now = Instant::now();
        let mut balloon = Balloon::new(GIB, now);
        balloon.sample(64 * MIB, 64 * MIB, 100, 0, 0, 8 * GIB, now);
        assert_eq!(balloon.sample(64 * MIB, 64 * MIB, 400, 0, 0, 8 * GIB, now + Duration::from_secs(1)), Some(2 * GIB));
        assert_eq!(balloon.sample(64 * MIB, 64 * MIB, 400, 0, 0, 8 * GIB, now + Duration::from_secs(30)), None);
        assert_eq!(balloon.sample(64 * MIB, 64 * MIB, 400, 0, 1, 8 * GIB, now + Duration::from_secs(31)), Some(3 * GIB));
        assert_eq!(balloon.sample(64 * MIB, 64 * MIB, 400, 0, 1, 8 * GIB, now + Duration::from_secs(60)), None);
    }

    #[test]
    fn idle_cache_can_be_reclaimed_and_counter_resets_do_not_fake_pressure() {
        let now = Instant::now();
        let mut balloon = Balloon::new(2 * GIB, now);
        balloon.sample(64 * MIB, GIB, 10_000, 0, 5, 2 * GIB, now);
        for second in 30..100 {
            balloon.sample(64 * MIB, 64 * MIB, 0, 0, 0, 2 * GIB, now + Duration::from_secs(second));
        }
        assert!(balloon.target >= 64 * MIB + HEADROOM);
        assert!(balloon.target < 64 * MIB + HEADROOM + MIN_CHANGE);
    }

    #[test]
    fn pressure_and_saturated_samples_respect_the_configured_ceiling() {
        let now = Instant::now();
        let mut balloon = Balloon::new(GIB, now);
        balloon.pressure(GIB, now);
        balloon.sample(u64::MAX, u64::MAX, u64::MAX, 100, 1, GIB, now);
        assert_eq!(balloon.target, GIB);
        assert_eq!(balloon.sample(0, 0, 0, 0, 0, 32 * MIB, now), Some(32 * MIB));
    }
}
