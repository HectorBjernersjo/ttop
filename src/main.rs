//! prem — vad äter mitt RAM, per tmux-session.
//!
//! Grupperar alla processer per tmux-session (sessioner sorterade på total
//! minnesanvändning), och inom varje session per processtyp (comm).
//! Processer utanför tmux hamnar i en egen hink.
//!
//! Minnesmått: PSS från /proc/<pid>/smaps_rollup när det går att läsa
//! (delade sidor räknas proportionellt, så summorna går ihop), annars RSS.
//!
//! Sessionstillhörighet: förälderkedjan upp till en tmux-pane-pid. Faller
//! tillbaka på TMUX_PANE i processens environ — det fångar föräldralösa
//! daemons (t.ex. MSBuild-noder med PPID 1) som annars ser sessionslösa ut.

use std::collections::HashMap;
use std::fs;
use std::process::Command;

const OUTSIDE: &str = "[utanför tmux]";

struct Proc {
    pid: u32,
    ppid: u32,
    comm: String,
    mem: u64, // bytes
    pss: bool,
    tmux_pane_env: Option<String>, // "%194"
}

fn main() {
    let procs = read_procs();
    let (pane_pid_to_session, pane_id_to_session) = tmux_panes();

    let ppid: HashMap<u32, u32> = procs.iter().map(|p| (p.pid, p.ppid)).collect();

    // session -> comm -> (antal, bytes)
    let mut sessions: HashMap<String, HashMap<String, (u32, u64)>> = HashMap::new();
    let mut any_rss_fallback = false;

    for p in &procs {
        if p.mem == 0 {
            continue; // kärntrådar m.m.
        }
        any_rss_fallback |= !p.pss;
        let session = session_of(p, &ppid, &pane_pid_to_session, &pane_id_to_session);
        let entry = sessions
            .entry(session)
            .or_default()
            .entry(p.comm.clone())
            .or_insert((0, 0));
        entry.0 += 1;
        entry.1 += p.mem;
    }

    print_meminfo_header();

    let mut ordered: Vec<(String, Vec<(String, u32, u64)>, u64)> = sessions
        .into_iter()
        .map(|(name, comms)| {
            let mut rows: Vec<(String, u32, u64)> =
                comms.into_iter().map(|(c, (n, b))| (c, n, b)).collect();
            rows.sort_by(|a, b| b.2.cmp(&a.2));
            let total = rows.iter().map(|r| r.2).sum();
            (name, rows, total)
        })
        .collect();
    ordered.sort_by(|a, b| b.2.cmp(&a.2));

    for (name, rows, total) in &ordered {
        println!("\n{name:<40} {:>10}", human(*total));
        for (comm, n, bytes) in rows {
            println!("  {comm:<30} {n:>4} st {:>10}", human(*bytes));
        }
    }

    let grand: u64 = ordered.iter().map(|(_, _, t)| t).sum();
    println!("\n{:<40} {:>10}", "summa processer", human(grand));
    if any_rss_fallback {
        println!("(vissa processer kunde inte läsas som PSS; RSS använd, kan överdriva delat minne)");
    }
}

fn read_procs() -> Vec<Proc> {
    let mut out = Vec::new();
    let Ok(dir) = fs::read_dir("/proc") else { return out };
    for entry in dir.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let base = format!("/proc/{pid}");
        let Ok(status) = fs::read_to_string(format!("{base}/status")) else { continue };
        let mut comm = String::new();
        let mut ppid = 0u32;
        let mut rss_kb = 0u64;
        for line in status.lines() {
            match line.split_once(':') {
                Some(("Name", v)) => comm = v.trim().to_string(),
                Some(("PPid", v)) => ppid = v.trim().parse().unwrap_or(0),
                Some(("VmRSS", v)) => {
                    rss_kb = v.trim().trim_end_matches(" kB").trim().parse().unwrap_or(0)
                }
                _ => {}
            }
        }
        let (mem, pss) = match pss_kb(&base) {
            Some(kb) => (kb * 1024, true),
            None => (rss_kb * 1024, false),
        };
        out.push(Proc {
            pid,
            ppid,
            comm,
            mem,
            pss,
            tmux_pane_env: tmux_pane_from_environ(&base),
        });
    }
    out
}

fn pss_kb(base: &str) -> Option<u64> {
    let rollup = fs::read_to_string(format!("{base}/smaps_rollup")).ok()?;
    rollup
        .lines()
        .find(|l| l.starts_with("Pss:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
}

fn tmux_pane_from_environ(base: &str) -> Option<String> {
    let environ = fs::read(format!("{base}/environ")).ok()?;
    environ
        .split(|&b| b == 0)
        .find_map(|kv| std::str::from_utf8(kv).ok()?.strip_prefix("TMUX_PANE="))
        .map(str::to_string)
}

/// Panes via `tmux list-panes -a`: (pane_pid -> session, pane_id -> session).
fn tmux_panes() -> (HashMap<u32, String>, HashMap<String, String>) {
    let mut by_pid = HashMap::new();
    let mut by_id = HashMap::new();
    let Ok(out) = Command::new("tmux")
        .args(["list-panes", "-a", "-F", "#{pane_pid} #{pane_id} #{session_name}"])
        .output()
    else {
        return (by_pid, by_id);
    };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut it = line.splitn(3, ' ');
        if let (Some(pid), Some(id), Some(sess)) = (it.next(), it.next(), it.next()) {
            if let Ok(pid) = pid.parse::<u32>() {
                by_pid.insert(pid, sess.to_string());
            }
            by_id.insert(id.to_string(), sess.to_string());
        }
    }
    (by_pid, by_id)
}

fn session_of(
    p: &Proc,
    ppid: &HashMap<u32, u32>,
    by_pane_pid: &HashMap<u32, String>,
    by_pane_id: &HashMap<String, String>,
) -> String {
    // Förälderkedjan: pid, förälder, farförälder ... tills en pane-pid.
    let mut cur = p.pid;
    for _ in 0..128 {
        if let Some(sess) = by_pane_pid.get(&cur) {
            return sess.clone();
        }
        match ppid.get(&cur) {
            Some(&parent) if parent > 1 => cur = parent,
            _ => break,
        }
    }
    // Föräldralös: TMUX_PANE i environ pekar ut panen som startade den.
    if let Some(sess) = p.tmux_pane_env.as_ref().and_then(|id| by_pane_id.get(id)) {
        return sess.clone();
    }
    OUTSIDE.to_string()
}

fn print_meminfo_header() {
    let Ok(mi) = fs::read_to_string("/proc/meminfo") else { return };
    let kb = |key: &str| -> u64 {
        mi.lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    let total = kb("MemTotal:") * 1024;
    let avail = kb("MemAvailable:") * 1024;
    let cache = kb("Cached:") * 1024;
    let shmem = kb("Shmem:") * 1024;
    let swap_used = (kb("SwapTotal:") - kb("SwapFree:")) * 1024;
    println!(
        "RAM {} totalt, {} tillgängligt  |  cache {} (varav shmem {})  |  swap använt {}",
        human(total),
        human(avail),
        human(cache),
        human(shmem),
        human(swap_used)
    );
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit >= 3 {
        format!("{v:.1} {}", UNITS[unit])
    } else {
        format!("{v:.0} {}", UNITS[unit])
    }
}
