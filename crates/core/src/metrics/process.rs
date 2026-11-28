//! Process metrics read from procfs at scrape time. Linux only; elsewhere every
//! sample is absent.

use std::fmt;

use prometheus_client::collector::Collector;
use prometheus_client::encoding::{DescriptorEncoder, EncodeMetric};
use prometheus_client::metrics::counter::ConstCounter;
use prometheus_client::metrics::gauge::ConstGauge;

/// One read of the process's own figures. Each is independent: a failed read
/// or parse leaves only that one `None`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ProcSample {
    pub cpu_seconds: Option<f64>,
    pub rss_bytes: Option<f64>,
    pub open_fds: Option<f64>,
    pub start_time: Option<f64>,
}

/// Parses `/proc/self/stat`, `/proc/self/statm` and `/proc/stat`. `open_fds`
/// is left `None`; `read` fills it.
pub fn parse(stat: &str, statm: &str, proc_stat: &str, clk_tck: f64, page_size: f64) -> ProcSample {
    // `comm` (field 2) may hold spaces and `)`, so fields are counted from
    // after the last `)`: index 0 there is field 3.
    let after_comm: Vec<&str> = stat
        .rfind(')')
        .map(|i| stat[i + 1..].split_whitespace().collect())
        .unwrap_or_default();
    let field = |n: usize| after_comm.get(n - 3).and_then(|v| v.parse::<f64>().ok());
    let ticks = (clk_tck > 0.0).then_some(clk_tck);

    let cpu_seconds = ticks.and_then(|t| Some((field(14)? + field(15)?) / t));
    let btime = proc_stat
        .lines()
        .find_map(|l| l.strip_prefix("btime "))
        .and_then(|v| v.trim().parse::<f64>().ok());
    let start_time = ticks.and_then(|t| Some(btime? + field(22)? / t));
    let rss_bytes = (page_size > 0.0)
        .then(|| statm.split_whitespace().nth(1)?.parse::<f64>().ok())
        .flatten()
        .map(|pages| pages * page_size);

    ProcSample {
        cpu_seconds,
        rss_bytes,
        open_fds: None,
        start_time,
    }
}

#[cfg(target_os = "linux")]
pub fn read() -> ProcSample {
    use std::fs;
    // SAFETY: `sysconf` only reads a system constant.
    let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    // SAFETY: as above.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as f64;
    let text = |p: &str| fs::read_to_string(p).unwrap_or_default();
    let mut sample = parse(
        &text("/proc/self/stat"),
        &text("/proc/self/statm"),
        &text("/proc/stat"),
        clk_tck,
        page_size,
    );
    sample.open_fds = fs::read_dir("/proc/self/fd")
        .ok()
        .map(|d| d.filter(|e| e.is_ok()).count() as f64);
    sample
}

#[cfg(not(target_os = "linux"))]
pub fn read() -> ProcSample {
    ProcSample::default()
}

/// Encodes the `process_*` families from a fresh `read` on each scrape.
#[derive(Debug)]
pub struct ProcessCollector;

fn gauge(enc: &mut DescriptorEncoder, name: &str, help: &str, v: Option<f64>) -> fmt::Result {
    if let Some(v) = v {
        let g = ConstGauge::new(v);
        g.encode(enc.encode_descriptor(name, help, None, g.metric_type())?)?;
    }
    Ok(())
}

impl Collector for ProcessCollector {
    fn encode(&self, mut enc: DescriptorEncoder) -> fmt::Result {
        let s = read();
        if let Some(v) = s.cpu_seconds {
            let c = ConstCounter::new(v);
            c.encode(enc.encode_descriptor(
                "process_cpu_seconds",
                "Total user and system CPU time spent in seconds.",
                None,
                c.metric_type(),
            )?)?;
        }
        gauge(
            &mut enc,
            "process_resident_memory_bytes",
            "Resident memory size in bytes.",
            s.rss_bytes,
        )?;
        gauge(
            &mut enc,
            "process_open_fds",
            "Number of open file descriptors.",
            s.open_fds,
        )?;
        gauge(
            &mut enc,
            "process_start_time_seconds",
            "Start time of the process since unix epoch in seconds.",
            s.start_time,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `comm` holds spaces and `)`; fields after it: state is field 3, utime 14,
    /// stime 15, starttime 22.
    const STAT: &str = "4242 (tab ) lica) S 1 4242 4242 0 -1 4194560 1000 0 0 0 250 75 0 0 20 0 8 0 12345 104857600 2048 18446744073709551615";
    const STATM: &str = "25600 2048 512 100 0 4000 0\n";
    const PROC_STAT: &str = "cpu  1 2 3 4\nintr 5\nbtime 1700000000\nprocesses 99\n";

    #[test]
    fn parse_fixture() {
        let s = parse(STAT, STATM, PROC_STAT, 100.0, 4096.0);
        assert_eq!(s.cpu_seconds, Some((250.0 + 75.0) / 100.0));
        assert_eq!(s.start_time, Some(1_700_000_000.0 + 12345.0 / 100.0));
        assert_eq!(s.rss_bytes, Some(2048.0 * 4096.0));
        assert_eq!(s.open_fds, None);
    }

    #[test]
    fn parse_garbage() {
        assert_eq!(
            parse("nonsense", "", "no btime here", 100.0, 4096.0),
            ProcSample::default()
        );
        assert_eq!(
            parse("1 (x) S 1 2", "a b", "btime x", 100.0, 4096.0),
            ProcSample::default()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn read_live_process() {
        let s = read();
        assert!(s.cpu_seconds.is_some_and(|v| v >= 0.0), "{s:?}");
        assert!(s.rss_bytes.is_some_and(|v| v > 0.0), "{s:?}");
        assert!(s.open_fds.is_some_and(|v| v > 0.0), "{s:?}");
        assert!(s.start_time.is_some_and(|v| v > 1_600_000_000.0), "{s:?}");
    }
}
