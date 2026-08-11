//! Snapshots of process and container memory usage, for diagnosing
//! out-of-memory kills and resource misconfigurations -- in particular
//! around `crate::tables`, which builds larger-than-RAM files and mmaps
//! them read-only.
//!
//! [`snapshot`] is meant to be logged at the start and end of each
//! pipeline step. Its fields differ by platform, since there is no single
//! number that's both meaningful on a macOS development machine (no
//! cgroups) and in the OCI/Kubernetes production container -- every
//! field is therefore `Option`, `None` where its source isn't available,
//! never a hard error: memory introspection must not fail a pipeline
//! step.
//!
//! # Why these particular numbers
//!
//! What actually triggers a Kubernetes pod's OOM-kill is the kernel's
//! cgroup accounting: `memory.current` exceeding the container's
//! `memory.max` (from its resource limit). That's Linux/cgroup-only, so
//! [`MemStats::cgroup_current_bytes`] and [`MemStats::cgroup_max_bytes`]
//! are the closest thing to "the metric Kubernetes uses to decide to
//! kill a pod" -- but they don't exist on macOS, hence the
//! [`MemStats::rss_peak_bytes`] fallback, which is available everywhere.
//!
//! A read-only mmap'd page, once touched, is resident and *is* charged
//! to `memory.current` -- but only as reclaimable clean page cache, as
//! long as the mapped file is genuinely disk-backed: the kernel can drop
//! such pages under pressure instead of invoking the OOM killer.
//! [`MemStats::rss_file_bytes`] vs. [`MemStats::rss_shmem_bytes`] is the
//! tell: the large tables showing up under `rss_file_bytes` is the
//! expected, safe case; showing up under `rss_shmem_bytes` would mean
//! the working directory has landed on tmpfs (e.g. a misconfigured
//! `emptyDir.medium: Memory`), silently breaking the "larger than RAM"
//! design assumption -- those pages would then be effectively anonymous
//! memory, unreclaimable and fully counted against any memory limit.
//!
//! On macOS, [`MemStats::rss_peak_bytes`] deliberately reads
//! `getrusage`'s `ru_maxrss` (the same "maximum resident set size"
//! `/usr/bin/time -l` reports), not Apple's `phys_footprint`/"peak
//! memory footprint". Verified empirically: mmap'ing and fully touching
//! a 1 GiB disk-backed file moved "maximum resident set size" from
//! 1.4 MB to 1075 MB, while "peak memory footprint" moved from 1.0 MB to
//! only 1.5 MB. `phys_footprint` deliberately excludes clean,
//! file-backed ("external") pages -- exactly the mmap'd table files --
//! for the same reclaimability reasoning as above; it's the macOS
//! analogue of Linux's *working-set* number (what kubelet uses for
//! node-pressure eviction), not of `memory.current` (what actually
//! triggers a container's own OOM-kill). So `ru_maxrss` is the metric
//! that actually reflects the mmap'd tables on macOS; `phys_footprint`
//! would hide them.

/// A snapshot of memory usage, taken by [`snapshot`]. Every field is
/// `None` when its source isn't available on the current platform, or
/// isn't currently exposed -- e.g. the `cgroup_*` fields when not
/// actually running under a memory-controlled cgroup.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemStats {
    /// This process's current resident set size, in bytes. Linux only
    /// (`VmRSS` in `/proc/self/status`).
    pub rss_bytes: Option<u64>,

    /// This process's peak resident set size since it started, in
    /// bytes. Linux (`VmHWM`) and macOS (`getrusage`'s `ru_maxrss`) --
    /// see the module docs for why macOS reads this rather than
    /// `phys_footprint`.
    pub rss_peak_bytes: Option<u64>,

    /// Portion of `rss_bytes` backed by anonymous memory (heap, stack).
    /// Linux only (`RssAnon`).
    pub rss_anon_bytes: Option<u64>,

    /// Portion of `rss_bytes` backed by a genuine, disk-backed file --
    /// where resident pages of the read-only mmap'd tables in
    /// `crate::tables` are expected to show up. Linux only (`RssFile`).
    pub rss_file_bytes: Option<u64>,

    /// Portion of `rss_bytes` backed by shared/anonymous memory (tmpfs,
    /// SysV/POSIX shared memory). If the tables' backing files show up
    /// here instead of in `rss_file_bytes`, the working directory has
    /// landed on tmpfs: see the module docs. Linux only (`RssShmem`).
    pub rss_shmem_bytes: Option<u64>,

    /// Current usage of the cgroup this process belongs to, in bytes --
    /// the number the kernel compares against `cgroup_max_bytes` to
    /// decide whether to OOM-kill something in the container. cgroup
    /// v2, Linux only, and only when actually running under a
    /// memory-controlled cgroup.
    pub cgroup_current_bytes: Option<u64>,

    /// This cgroup's memory limit, in bytes. `None` both when
    /// unavailable and when the cgroup has no limit configured
    /// (`memory.max` reading literally "max").
    pub cgroup_max_bytes: Option<u64>,

    /// Highest `cgroup_current_bytes` ever observed for this cgroup, in
    /// bytes. Requires Linux 5.19+ (`memory.peak`); `None` on older
    /// kernels.
    pub cgroup_peak_bytes: Option<u64>,
}

impl std::fmt::Display for MemStats {
    /// Renders as space-separated `key=value` pairs, omitting fields
    /// that are `None` -- meant to be interpolated straight into a
    /// `log::info!` message, e.g. `"step start: {name} {snapshot}"`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let fields: [(&str, Option<u64>); 8] = [
            ("rss_bytes", self.rss_bytes),
            ("rss_peak_bytes", self.rss_peak_bytes),
            ("rss_anon_bytes", self.rss_anon_bytes),
            ("rss_file_bytes", self.rss_file_bytes),
            ("rss_shmem_bytes", self.rss_shmem_bytes),
            ("cgroup_current_bytes", self.cgroup_current_bytes),
            ("cgroup_max_bytes", self.cgroup_max_bytes),
            ("cgroup_peak_bytes", self.cgroup_peak_bytes),
        ];
        let mut first = true;
        for (name, value) in fields {
            if let Some(value) = value {
                if !first {
                    write!(f, " ")?;
                }
                write!(f, "{name}={value}")?;
                first = false;
            }
        }
        Ok(())
    }
}

/// Takes a snapshot of current memory usage. Never fails: unavailable
/// sources simply leave the corresponding [`MemStats`] fields `None`.
pub fn snapshot() -> MemStats {
    #[cfg(target_os = "linux")]
    {
        linux::snapshot()
    }
    #[cfg(target_os = "macos")]
    {
        macos::snapshot()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        MemStats::default()
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::MemStats;
    use std::fs;

    const CGROUP_DIR: &str = "/sys/fs/cgroup";

    pub(super) fn snapshot() -> MemStats {
        let mut stats = fs::read_to_string("/proc/self/status")
            .map(|content| parse_proc_status(&content))
            .unwrap_or_default();
        stats.cgroup_current_bytes = read_cgroup_bytes("memory.current");
        stats.cgroup_max_bytes = read_cgroup_bytes("memory.max");
        stats.cgroup_peak_bytes = read_cgroup_bytes("memory.peak");
        stats
    }

    /// Parses the fields we care about out of the content of
    /// `/proc/<pid>/status`. Each relevant line looks like
    /// `VmRSS:\t   12345 kB`, a `kB`-suffixed value in kibibytes.
    fn parse_proc_status(content: &str) -> MemStats {
        let mut stats = MemStats::default();
        for line in content.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let Some(kb) = parse_kb_value(value) else {
                continue;
            };
            let bytes = kb * 1024;
            match key {
                "VmRSS" => stats.rss_bytes = Some(bytes),
                "VmHWM" => stats.rss_peak_bytes = Some(bytes),
                "RssAnon" => stats.rss_anon_bytes = Some(bytes),
                "RssFile" => stats.rss_file_bytes = Some(bytes),
                "RssShmem" => stats.rss_shmem_bytes = Some(bytes),
                _ => {}
            }
        }
        stats
    }

    /// Parses a `/proc/self/status` value field such as `   12345 kB`.
    fn parse_kb_value(value: &str) -> Option<u64> {
        value.trim().strip_suffix("kB")?.trim().parse().ok()
    }

    /// Reads a single-integer cgroup v2 file such as `memory.current`,
    /// returning `None` if the file is absent (no cgroup, or a kernel
    /// too old for it), unreadable, or -- for `memory.max` -- literally
    /// "max" (no limit configured).
    fn read_cgroup_bytes(file_name: &str) -> Option<u64> {
        fs::read_to_string(format!("{CGROUP_DIR}/{file_name}"))
            .ok()
            .and_then(|content| content.trim().parse().ok())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn test_parse_proc_status() {
            let content = "\
Name:\tosm-diffs
State:\tR (running)
VmPeak:\t 2000000 kB
VmSize:\t 1900000 kB
VmHWM:\t   150000 kB
VmRSS:\t   120000 kB
RssAnon:\t   80000 kB
RssFile:\t   39000 kB
RssShmem:\t    1000 kB
Threads:\t8
";
            let stats = parse_proc_status(content);
            assert_eq!(stats.rss_bytes, Some(120_000 * 1024));
            assert_eq!(stats.rss_peak_bytes, Some(150_000 * 1024));
            assert_eq!(stats.rss_anon_bytes, Some(80_000 * 1024));
            assert_eq!(stats.rss_file_bytes, Some(39_000 * 1024));
            assert_eq!(stats.rss_shmem_bytes, Some(1_000 * 1024));
            // Fields we don't care about (VmPeak, VmSize, Threads, ...)
            // must not be mistaken for ones we do.
            assert_eq!(stats.cgroup_current_bytes, None);
        }

        #[test]
        fn test_parse_proc_status_missing_fields() {
            let stats = parse_proc_status("Name:\tosm-diffs\nState:\tR (running)\n");
            assert_eq!(stats, MemStats::default());
        }

        #[test]
        fn test_parse_proc_status_malformed_value_is_skipped() {
            // A value without the expected "kB" suffix (or that isn't a
            // number at all) must be ignored rather than panicking.
            let stats = parse_proc_status("VmRSS:\tnot-a-number\nVmHWM:\t120000 MB\n");
            assert_eq!(stats.rss_bytes, None);
            assert_eq!(stats.rss_peak_bytes, None);
        }

        #[test]
        fn test_parse_kb_value() {
            assert_eq!(parse_kb_value("   12345 kB"), Some(12345));
            assert_eq!(parse_kb_value("0 kB"), Some(0));
            assert_eq!(parse_kb_value("12345"), None);
            assert_eq!(parse_kb_value("kB"), None);
            assert_eq!(parse_kb_value(""), None);
        }

        #[test]
        fn test_read_cgroup_bytes_missing_file() {
            assert_eq!(read_cgroup_bytes("no-such-file-2ce9f286b8"), None);
        }

        #[test]
        fn test_snapshot_does_not_panic() {
            // Smoke test against the real /proc and /sys/fs/cgroup on
            // whatever machine runs the test suite: must never panic,
            // regardless of which fields end up populated.
            let _ = snapshot();
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::MemStats;

    pub(super) fn snapshot() -> MemStats {
        MemStats {
            rss_peak_bytes: peak_rss_bytes(),
            ..MemStats::default()
        }
    }

    /// Returns this process's peak resident set size since it started,
    /// in bytes -- the same "maximum resident set size" figure
    /// `/usr/bin/time -l` reports. Deliberately not `phys_footprint`;
    /// see the module docs.
    fn peak_rss_bytes() -> Option<u64> {
        // SAFETY: `usage` is a plain-old-data struct that `getrusage`
        // fully overwrites on success (return value 0); we only read it
        // in that case.
        unsafe {
            let mut usage: libc::rusage = std::mem::zeroed();
            if libc::getrusage(libc::RUSAGE_SELF, &mut usage) == 0 {
                // On Darwin, ru_maxrss is already in bytes (unlike
                // Linux, where the same field is in kilobytes).
                u64::try_from(usage.ru_maxrss).ok()
            } else {
                None
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn test_peak_rss_bytes_is_nonzero() {
            // Every process has touched at least some resident memory
            // by the time it's running test code.
            assert!(peak_rss_bytes().unwrap_or(0) > 0);
        }

        #[test]
        fn test_snapshot_populates_rss_peak_bytes() {
            let stats = snapshot();
            assert!(stats.rss_peak_bytes.is_some());
            assert_eq!(stats.rss_bytes, None);
            assert_eq!(stats.cgroup_current_bytes, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_display_omits_none_fields() {
        let stats = MemStats {
            rss_peak_bytes: Some(42),
            cgroup_max_bytes: Some(1_000_000),
            ..MemStats::default()
        };
        assert_eq!(
            stats.to_string(),
            "rss_peak_bytes=42 cgroup_max_bytes=1000000"
        );
    }

    #[test]
    fn test_display_empty_when_nothing_available() {
        assert_eq!(MemStats::default().to_string(), "");
    }
}
