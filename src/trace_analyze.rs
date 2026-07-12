//! Offline analyzer for trace ring dumps produced by `crate::trace`.
//!
//! Reads one dump (per-stage gap percentiles, worst stalls, stall periodicity
//! with an AWDL 524.288 ms fold, send→recv delay variation from SKM2
//! timestamps) or a Windows + macOS dump pair (per-freeze sequence-range join
//! answering "did the sender keep sending while the receiver saw silence").

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::trace::{EVENT_BYTES, Stage, TraceEvent};

/// AWDL availability-window period: 512 TU, one TU = 1024 us.
pub const AWDL_PERIOD_US: u64 = 524_288;

pub struct DumpFile {
    pub path: PathBuf,
    pub role: u8,
    pub epoch_wall_ms: u64,
    pub dumped_wall_ms: u64,
    pub reason: String,
    pub events: Vec<TraceEvent>,
}

pub fn load_dump(path: &PathBuf) -> Result<DumpFile> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    if bytes.len() < 78 {
        bail!("{}: too short for a trace dump", path.display());
    }
    let magic = u32::from_le_bytes(bytes[0..4].try_into()?);
    if magic != crate::trace::DUMP_MAGIC {
        bail!("{}: bad magic 0x{magic:08x}", path.display());
    }
    let version = u16::from_le_bytes(bytes[4..6].try_into()?);
    if version != crate::trace::DUMP_VERSION {
        bail!("{}: unsupported dump version {version}", path.display());
    }
    let role = bytes[6];
    let epoch_wall_ms = u64::from_le_bytes(bytes[8..16].try_into()?);
    let dumped_wall_ms = u64::from_le_bytes(bytes[16..24].try_into()?);
    let capacity = u64::from_le_bytes(bytes[24..32].try_into()?) as usize;
    let _head = u64::from_le_bytes(bytes[32..40].try_into()?);
    let reason = String::from_utf8_lossy(&bytes[40..72])
        .trim_end_matches('\0')
        .to_owned();
    let body = &bytes[72..];
    if body.len() < capacity * EVENT_BYTES {
        bail!("{}: truncated dump body", path.display());
    }

    let mut events = Vec::with_capacity(capacity);
    for i in 0..capacity {
        let at = i * EVENT_BYTES;
        let t_us = u64::from_le_bytes(body[at..at + 8].try_into()?);
        if t_us == 0 {
            continue;
        }
        let seq = u64::from_le_bytes(body[at + 8..at + 16].try_into()?);
        let ab = u64::from_le_bytes(body[at + 16..at + 24].try_into()?);
        let cs = u64::from_le_bytes(body[at + 24..at + 32].try_into()?);
        let stage = ((cs >> 32) & 0xff) as u8;
        if Stage::name(stage) == "unknown" {
            continue; // torn or garbage slot
        }
        events.push(TraceEvent {
            t_us,
            seq,
            a: ab as u32 as i32,
            b: (ab >> 32) as u32 as i32,
            c: cs as u32,
            stage,
        });
    }
    events.sort_by_key(|event| event.t_us);
    Ok(DumpFile {
        path: path.clone(),
        role,
        epoch_wall_ms,
        dumped_wall_ms,
        reason,
        events,
    })
}

/// Active spans bounded by ActiveOn/ActiveOff markers; whole capture if none.
fn active_spans(events: &[TraceEvent]) -> Vec<(u64, u64)> {
    let mut spans = Vec::new();
    let mut open: Option<u64> = None;
    for event in events {
        if event.stage == Stage::ActiveOn as u8 {
            open.get_or_insert(event.t_us);
        } else if event.stage == Stage::ActiveOff as u8
            && let Some(start) = open.take()
        {
            spans.push((start, event.t_us));
        }
    }
    let last_t = events.last().map(|event| event.t_us).unwrap_or(0);
    if let Some(start) = open {
        spans.push((start, last_t));
    }
    if spans.is_empty() && !events.is_empty() {
        spans.push((events[0].t_us, last_t));
    }
    spans
}

fn in_spans(spans: &[(u64, u64)], t: u64) -> bool {
    spans.iter().any(|&(start, end)| t >= start && t <= end)
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * q.clamp(0.0, 1.0)).round() as usize;
    sorted[index]
}

struct StageGaps {
    stage: u8,
    count: usize,
    gaps_ms: Vec<f64>,
    /// (gap_start_us, gap_ms) for gaps above the stall threshold.
    stalls: Vec<(u64, f64)>,
}

fn stage_gap_analysis(
    events: &[TraceEvent],
    spans: &[(u64, u64)],
    stall_ms: f64,
) -> Vec<StageGaps> {
    let mut by_stage: std::collections::BTreeMap<u8, Vec<u64>> = std::collections::BTreeMap::new();
    for event in events {
        if event.stage >= Stage::ActiveOn as u8 && event.stage != Stage::BenchTick as u8 {
            continue;
        }
        if !in_spans(spans, event.t_us) {
            continue;
        }
        by_stage.entry(event.stage).or_default().push(event.t_us);
    }

    let mut out = Vec::new();
    for (stage, times) in by_stage {
        let mut gaps_ms = Vec::with_capacity(times.len().saturating_sub(1));
        let mut stalls = Vec::new();
        for pair in times.windows(2) {
            // Do not count gaps that cross span boundaries.
            let midpoint = pair[0] + (pair[1] - pair[0]) / 2;
            if !in_spans(spans, midpoint) {
                continue;
            }
            let gap = (pair[1] - pair[0]) as f64 / 1000.0;
            if gap >= stall_ms {
                stalls.push((pair[0], gap));
            }
            gaps_ms.push(gap);
        }
        out.push(StageGaps {
            stage,
            count: times.len(),
            gaps_ms,
            stalls,
        });
    }
    out
}

fn print_stage_table(analysis: &[StageGaps]) {
    println!(
        "{:<16} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9}",
        "stage", "events", "mean_ms", "p50_ms", "p95_ms", "p99_ms", "p99.9", "max_ms"
    );
    for entry in analysis {
        let mut sorted = entry.gaps_ms.clone();
        sorted.sort_by(|a, b| a.total_cmp(b));
        let mean = if sorted.is_empty() {
            0.0
        } else {
            sorted.iter().sum::<f64>() / sorted.len() as f64
        };
        println!(
            "{:<16} {:>9} {:>9.3} {:>9.3} {:>9.3} {:>9.3} {:>9.3} {:>9.3}",
            Stage::name(entry.stage),
            entry.count,
            mean,
            percentile(&sorted, 0.50),
            percentile(&sorted, 0.95),
            percentile(&sorted, 0.99),
            percentile(&sorted, 0.999),
            sorted.last().copied().unwrap_or(0.0),
        );
    }
}

fn print_worst_stalls(analysis: &[StageGaps], top: usize) {
    let mut all: Vec<(u8, u64, f64)> = Vec::new();
    for entry in analysis {
        for &(t, gap) in &entry.stalls {
            all.push((entry.stage, t, gap));
        }
    }
    if all.is_empty() {
        println!("no stalls above threshold");
        return;
    }
    all.sort_by(|a, b| b.2.total_cmp(&a.2));
    println!("worst stalls (stage, t_start_s, gap_ms):");
    for (stage, t, gap) in all.iter().take(top) {
        println!(
            "  {:<16} t={:>10.3}s gap={:>8.1}ms",
            Stage::name(*stage),
            *t as f64 / 1e6,
            gap
        );
    }
}

/// Periodicity of stalls for one stage: consecutive stall-start deltas and how
/// tightly they sit on multiples of the AWDL 524.288 ms availability window.
fn print_periodicity(analysis: &[StageGaps], stage: u8) {
    let Some(entry) = analysis.iter().find(|entry| entry.stage == stage) else {
        return;
    };
    if entry.stalls.len() < 3 {
        return;
    }
    let starts: Vec<u64> = entry.stalls.iter().map(|&(t, _)| t).collect();
    let deltas: Vec<f64> = starts
        .windows(2)
        .map(|pair| (pair[1] - pair[0]) as f64 / 1000.0)
        .collect();

    let mut near = 0_usize;
    let mut considered = 0_usize;
    for &delta_ms in &deltas {
        if delta_ms > 6000.0 {
            continue;
        }
        considered += 1;
        let period_ms = AWDL_PERIOD_US as f64 / 1000.0;
        let ratio = delta_ms / period_ms;
        if (ratio - ratio.round()).abs() < 0.15 && ratio.round() >= 1.0 {
            near += 1;
        }
    }
    let mut sorted = deltas.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    println!(
        "stall periodicity for {}: stalls={} median_delta={:.0}ms p10={:.0}ms p90={:.0}ms",
        Stage::name(stage),
        entry.stalls.len(),
        percentile(&sorted, 0.5),
        percentile(&sorted, 0.1),
        percentile(&sorted, 0.9),
    );
    if considered > 0 {
        let share = 100.0 * near as f64 / considered as f64;
        println!(
            "  awdl fold (multiples of 524.288ms +/-15%): {near}/{considered} = {share:.0}%{}",
            if share >= 70.0 {
                "  << PERIODIC, AWDL-like"
            } else if share >= 40.0 {
                "  << partially periodic"
            } else {
                ""
            }
        );
    }
}

/// Send→recv delay variation from SKM2 sender timestamps carried in `c` of
/// mac_recv stamps. Clock offset cancels in differences against a rolling
/// minimum; sender/receiver drift over a 2 s window is negligible (<1 ms).
fn print_delay_variation(events: &[TraceEvent], spans: &[(u64, u64)], stall_ms: f64) {
    let recv: Vec<&TraceEvent> = events
        .iter()
        .filter(|event| {
            event.stage == Stage::MacRecv as u8 && event.c != 0 && in_spans(spans, event.t_us)
        })
        .collect();
    if recv.len() < 100 {
        return;
    }

    // d_i = t_recv - t_send (mod 2^32 us); normalize by rolling 2 s minimum.
    let mut dv_ms: Vec<f64> = Vec::with_capacity(recv.len());
    let mut spikes: Vec<(u64, f64)> = Vec::new();
    let raw: Vec<(u64, u32)> = recv
        .iter()
        .map(|event| (event.t_us, (event.t_us as u32).wrapping_sub(event.c)))
        .collect();
    let mut window_start = 0_usize;
    for i in 0..raw.len() {
        while raw[window_start].0 + 2_000_000 < raw[i].0 {
            window_start += 1;
        }
        let min_d = raw[window_start..=i]
            .iter()
            .map(|&(_, d)| d)
            .min()
            .unwrap_or(0);
        let dv = raw[i].1.wrapping_sub(min_d) as f64 / 1000.0;
        if dv < 10_000.0 {
            // ignore wrap artifacts
            dv_ms.push(dv);
            if dv >= stall_ms {
                spikes.push((raw[i].0, dv));
            }
        }
    }
    let mut sorted = dv_ms.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    println!(
        "send->recv delay variation (skm2): n={} p50={:.1}ms p95={:.1}ms p99={:.1}ms max={:.1}ms",
        sorted.len(),
        percentile(&sorted, 0.50),
        percentile(&sorted, 0.95),
        percentile(&sorted, 0.99),
        sorted.last().copied().unwrap_or(0.0),
    );
    if !spikes.is_empty() {
        println!(
            "  in-flight delay spikes >= {:.0}ms: {} (these packets were SENT on time and DELIVERED late)",
            stall_ms,
            spikes.len()
        );
    }
}

/// For each receiver freeze window, check what the sender did in the same
/// sequence range: continuous win_udp_post => packets left Windows on time.
fn print_cross_join(win: &DumpFile, mac: &DumpFile, stall_ms: f64) {
    let mac_spans = active_spans(&mac.events);
    let mac_analysis = stage_gap_analysis(&mac.events, &mac_spans, stall_ms);
    let Some(recv) = mac_analysis
        .iter()
        .find(|entry| entry.stage == Stage::MacRecv as u8)
    else {
        println!("cross-join: mac dump has no mac_recv events");
        return;
    };
    if recv.stalls.is_empty() {
        println!("cross-join: no mac_recv stalls above threshold; nothing to join");
        return;
    }

    // Sender stamps by sequence number.
    let mut send_by_seq: Vec<(u64, u64)> = win
        .events
        .iter()
        .filter(|event| event.stage == Stage::WinUdpPost as u8)
        .map(|event| (event.seq, event.t_us))
        .collect();
    send_by_seq.sort_unstable();
    if send_by_seq.is_empty() {
        println!("cross-join: windows dump has no win_udp_post events");
        return;
    }

    let recv_events: Vec<&TraceEvent> = mac
        .events
        .iter()
        .filter(|event| event.stage == Stage::MacRecv as u8)
        .collect();

    println!("cross-join of mac_recv freezes against windows send stamps:");
    for &(gap_start, gap_ms) in recv.stalls.iter().take(20) {
        let gap_end = gap_start + (gap_ms * 1000.0) as u64;
        let seq_before = recv_events
            .iter()
            .rev()
            .find(|event| event.t_us <= gap_start)
            .map(|event| event.seq);
        let seq_after = recv_events
            .iter()
            .find(|event| event.t_us >= gap_end)
            .map(|event| event.seq);
        let (Some(seq_lo), Some(seq_hi)) = (seq_before, seq_after) else {
            continue;
        };
        let sends: Vec<u64> = send_by_seq
            .iter()
            .filter(|&&(seq, _)| seq > seq_lo && seq <= seq_hi)
            .map(|&(_, t)| t)
            .collect();
        let missing = (seq_hi.saturating_sub(seq_lo)).saturating_sub(1);
        let max_send_gap_ms = sends
            .windows(2)
            .map(|pair| (pair[1] - pair[0]) as f64 / 1000.0)
            .fold(0.0_f64, f64::max);
        let verdict = if sends.len() as u64 >= missing.max(1) && max_send_gap_ms < gap_ms * 0.5 {
            "sender kept sending -> network/receive side"
        } else if sends.is_empty() {
            "sender sent nothing -> windows/input side"
        } else {
            "sender paused too -> windows/input side"
        };
        println!(
            "  recv gap {:>7.1}ms at t={:>9.3}s seq({}..{}] sends_in_range={} max_send_gap={:>7.1}ms => {}",
            gap_ms,
            gap_start as f64 / 1e6,
            seq_lo,
            seq_hi,
            sends.len(),
            max_send_gap_ms,
            verdict
        );
    }
}

/// Per-seq pipeline latency inside the mac dump: recv -> take -> post_post.
fn print_mac_pipeline(events: &[TraceEvent], spans: &[(u64, u64)]) {
    use std::collections::HashMap;
    let mut recv_at: HashMap<u64, u64> = HashMap::new();
    let mut take_delay = Vec::new();
    let mut post_delay = Vec::new();
    let mut last_take: Option<(u64, u64)> = None; // (seq, t)

    for event in events {
        if !in_spans(spans, event.t_us) {
            continue;
        }
        match event.stage {
            s if s == Stage::MacRecv as u8 => {
                recv_at.insert(event.seq, event.t_us);
                if recv_at.len() > 100_000 {
                    recv_at.clear();
                }
            }
            s if s == Stage::MacTake as u8 => {
                if let Some(&t_recv) = recv_at.get(&event.seq) {
                    take_delay.push((event.t_us.saturating_sub(t_recv)) as f64 / 1000.0);
                }
                last_take = Some((event.seq, event.t_us));
            }
            s if s == Stage::MacPostPost as u8 => {
                if let Some((_, t_take)) = last_take.take() {
                    post_delay.push((event.t_us.saturating_sub(t_take)) as f64 / 1000.0);
                }
            }
            _ => {}
        }
    }

    for (label, mut values) in [
        ("recv->take (wakeup+aggregation)", take_delay),
        ("take->post_post (create+CGEventPost)", post_delay),
    ] {
        if values.is_empty() {
            continue;
        }
        values.sort_by(|a, b| a.total_cmp(b));
        println!(
            "{label}: n={} p50={:.3}ms p95={:.3}ms p99={:.3}ms max={:.3}ms",
            values.len(),
            percentile(&values, 0.50),
            percentile(&values, 0.95),
            percentile(&values, 0.99),
            values.last().copied().unwrap_or(0.0),
        );
    }
}

pub fn run(files: Vec<PathBuf>, stall_ms: f64, top: usize) -> Result<()> {
    if files.is_empty() {
        bail!("pass at least one .sktrace dump file");
    }
    let mut dumps = Vec::new();
    for file in &files {
        dumps.push(load_dump(file)?);
    }

    for dump in &dumps {
        println!();
        println!("=== {} ===", dump.path.display());
        println!(
            "role={} reason={} events={} epoch_wall_ms={} dumped_wall_ms={}",
            match dump.role {
                0 => "win-host",
                1 => "mac-client",
                2 => "bench",
                _ => "unknown",
            },
            dump.reason,
            dump.events.len(),
            dump.epoch_wall_ms,
            dump.dumped_wall_ms
        );
        let spans = active_spans(&dump.events);
        let analysis = stage_gap_analysis(&dump.events, &spans, stall_ms);
        print_stage_table(&analysis);
        println!();
        print_worst_stalls(&analysis, top);
        println!();
        for stage in [
            Stage::WinRawIn as u8,
            Stage::WinUdpPost as u8,
            Stage::MacRecv as u8,
            Stage::MacPostPost as u8,
            Stage::MacObserved as u8,
            Stage::BenchTick as u8,
        ] {
            print_periodicity(&analysis, stage);
        }
        println!();
        print_delay_variation(&dump.events, &spans, stall_ms);
        print_mac_pipeline(&dump.events, &spans);
    }

    let win = dumps.iter().find(|dump| dump.role == 0);
    let mac = dumps.iter().find(|dump| dump.role == 1);
    if let (Some(win), Some(mac)) = (win, mac) {
        println!();
        print_cross_join(win, mac, stall_ms);
    }

    Ok(())
}

/// In-process report used by mac-cg-bench: same tables, no file round-trip.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn report_events(events: &[TraceEvent], stall_ms: f64, top: usize) {
    let spans = active_spans(events);
    let analysis = stage_gap_analysis(events, &spans, stall_ms);
    print_stage_table(&analysis);
    println!();
    print_worst_stalls(&analysis, top);
    println!();
    for stage in [
        Stage::BenchTick as u8,
        Stage::MacPostPre as u8,
        Stage::MacPostPost as u8,
        Stage::MacObserved as u8,
    ] {
        print_periodicity(&analysis, stage);
    }
    print_mac_pipeline(events, &spans);
}
