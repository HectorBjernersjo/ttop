//! tmem — vad äter mitt RAM, per tmux-session.
//!
//! Grupperar alla processer per tmux-session (sessioner sorterade på total
//! minnesanvändning), och inom varje session per processtyp. Processer
//! utanför tmux hamnar i en egen hink. Default beskärs outputen till det
//! som faktiskt förklarar minnesanvändningen; `--all` visar allt.
//!
//! Minnesmått: PSS från /proc/<pid>/smaps_rollup när det går att läsa
//! (delade sidor räknas proportionellt, så summorna går ihop), annars RSS.
//!
//! Sessionstillhörighet: förälderkedjan upp till en tmux-pane-pid. Faller
//! tillbaka på TMUX_PANE i processens environ — det fångar föräldralösa
//! daemons (t.ex. MSBuild-noder med PPID 1) som annars ser sessionslösa ut.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::IsTerminal;
use std::process::Command;

const OUTSIDE: &str = "[utanför tmux]";
const BAR_WIDTH: usize = 20;
// Rader visas vid >= 1 % av sessionens minne; sessioner vid >= 2 % av
// totalen — en session ska förtjäna sin plats mer än en enskild rad.
const ROW_MIN_PERCENT: u64 = 1;
const SESSION_MIN_PERCENT: u64 = 2;

struct Proc {
    pid: u32,
    ppid: u32,
    name: String,
    mem: u64, // bytes
    pss: bool,
    tmux_pane_env: Option<String>, // "%194"
}

struct Row {
    name: String,
    count: u32,
    bytes: u64,
}

struct Session {
    name: String,
    rows: Vec<Row>,
    total: u64,
}

#[derive(Clone, Copy)]
struct Paint {
    on: bool,
}

impl Paint {
    const BOLD: &'static str = "1";
    const DIM: &'static str = "2";
    const RED: &'static str = "31";
    const GREEN: &'static str = "32";
    const YELLOW: &'static str = "33";

    fn w(self, code: &str, s: &str) -> String {
        if self.on {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
}

fn main() {
    let all = env::args().skip(1).any(|a| a == "-a" || a == "--all");
    let paint = Paint {
        on: std::io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none(),
    };

    let procs = read_procs();
    let (pane_pid_to_session, pane_id_to_session) = tmux_panes();
    let ppid: HashMap<u32, u32> = procs.iter().map(|p| (p.pid, p.ppid)).collect();

    // session -> processtyp -> (antal, bytes)
    let mut by_session: HashMap<String, HashMap<String, (u32, u64)>> = HashMap::new();
    let mut any_rss_fallback = false;
    for p in &procs {
        if p.mem == 0 {
            continue; // kärntrådar m.m.
        }
        any_rss_fallback |= !p.pss;
        let session = session_of(p, &ppid, &pane_pid_to_session, &pane_id_to_session);
        let e = by_session
            .entry(session)
            .or_default()
            .entry(p.name.clone())
            .or_insert((0, 0));
        e.0 += 1;
        e.1 += p.mem;
    }

    let mut sessions: Vec<Session> = by_session
        .into_iter()
        .map(|(name, types)| {
            let mut rows: Vec<Row> = types
                .into_iter()
                .map(|(name, (count, bytes))| Row { name, count, bytes })
                .collect();
            rows.sort_by(|a, b| b.bytes.cmp(&a.bytes));
            let total = rows.iter().map(|r| r.bytes).sum();
            Session { name, rows, total }
        })
        .collect();
    sessions.sort_by(|a, b| b.total.cmp(&a.total));
    let grand: u64 = sessions.iter().map(|s| s.total).sum();

    // Sessioner under gränsen kollapsar till en samlingsrad.
    let mut n_show = sessions.len();
    if !all {
        n_show = sessions
            .iter()
            .enumerate()
            .take_while(|(i, s)| *i == 0 || s.total * 100 >= grand * SESSION_MIN_PERCENT)
            .count();
        if sessions.len() - n_show == 1 {
            n_show = sessions.len(); // "… 1 session till" döljer inget; visa den
        }
    }

    let visible: Vec<usize> = sessions[..n_show]
        .iter()
        .map(|s| visible_row_count(&s.rows, s.total, all))
        .collect();

    let name_w = sessions[..n_show]
        .iter()
        .map(|s| s.name.chars().count())
        .max()
        .unwrap_or(0)
        .max(14);
    let row_w = sessions[..n_show]
        .iter()
        .zip(&visible)
        .flat_map(|(s, &k)| s.rows[..k].iter())
        .map(|r| r.name.chars().count())
        .max()
        .unwrap_or(0)
        .max(12);

    // Stigande ordning: det största hamnar längst ner, närmast prompten.
    if n_show < sessions.len() {
        let rest = &sessions[n_show..];
        let bytes: u64 = rest.iter().map(|s| s.total).sum();
        println!(
            "{}",
            paint.w(
                Paint::DIM,
                &format!("{:<name_w$} {:>9}", format!("… {} sessioner till", rest.len()), human(bytes)),
            )
        );
    }

    for (s, &k) in sessions[..n_show].iter().zip(&visible).rev() {
        let pct = if grand > 0 { s.total * 100 / grand } else { 0 };
        println!(
            "\n{} {}  {} {}",
            paint.w(Paint::BOLD, &format!("{:<name_w$}", s.name)),
            paint.w(Paint::BOLD, &format!("{:>9}", human(s.total))),
            bar(pct, paint),
            paint.w(Paint::DIM, &format!("{pct:>3}%")),
        );
        // Rader fallande: sessionens största process direkt under rubriken.
        for r in &s.rows[..k] {
            println!(
                "  {:<row_w$} {:>4} st {:>9}",
                r.name,
                r.count,
                human(r.bytes)
            );
        }
        if k < s.rows.len() {
            let rest = &s.rows[k..];
            let bytes: u64 = rest.iter().map(|r| r.bytes).sum();
            println!(
                "  {}",
                paint.w(
                    Paint::DIM,
                    &format!("{:<row_w$} {:7} {:>9}", format!("… {} till", rest.len()), "", human(bytes)),
                )
            );
        }
    }

    println!();
    if any_rss_fallback {
        println!(
            "{}",
            paint.w(
                Paint::DIM,
                "(vissa processer kunde inte läsas som PSS; RSS använd, kan överdriva delat minne)",
            )
        );
    }
    print_header(grand, paint);
}

/// Visa rader som står för minst ROW_MIN_PERCENT av sessionens minne.
fn visible_row_count(rows: &[Row], total: u64, all: bool) -> usize {
    if all {
        return rows.len();
    }
    let mut k = rows
        .iter()
        .take_while(|r| r.bytes * 100 >= total * ROW_MIN_PERCENT)
        .count()
        .max(1);
    if rows.len() - k == 1 {
        k += 1; // "… 1 till" tar samma plats som raden själv
    }
    k
}

fn bar(pct: u64, paint: Paint) -> String {
    let filled = ((pct as usize * BAR_WIDTH + 50) / 100)
        .clamp(usize::from(pct > 0), BAR_WIDTH);
    let color = match pct {
        30.. => Paint::RED,
        10.. => Paint::YELLOW,
        _ => Paint::GREEN,
    };
    format!(
        "{}{}",
        paint.w(color, &"█".repeat(filled)),
        paint.w(Paint::DIM, &"░".repeat(BAR_WIDTH - filled)),
    )
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
            name: proc_name(&base, &comm),
            mem,
            pss,
            tmux_pane_env: tmux_pane_from_environ(&base),
        });
    }
    out
}

/// Processtypens namn: basename ur cmdline (comm huggs av vid 15 tecken).
/// För interpreters (node, dotnet, …) används skriptets/dll:ens namn
/// istället — så MSBuild-noder heter "MSBuild", inte "dotnet".
fn proc_name(base: &str, comm: &str) -> String {
    let Ok(raw) = fs::read(format!("{base}/cmdline")) else { return comm.to_string() };
    // Processer som skriver om sin titel ("npm exec …") har mellanslag i
    // ett enda argv-fält; platta ut till tokens oavsett.
    let toks: Vec<&str> = raw
        .split(|&b| b == 0)
        .filter_map(|s| std::str::from_utf8(s).ok())
        .flat_map(str::split_whitespace)
        .collect();
    let Some(first) = toks.first() else { return comm.to_string() };
    // Login-shells har argv0 "-zsh"; setproctitle-namn kan sluta med ":".
    let name = basename(first).trim_start_matches('-').trim_end_matches(':');
    const INTERPRETERS: &[&str] = &["node", "dotnet", "python", "python3", "ruby", "java", "bun", "deno", "mono"];
    if INTERPRETERS.contains(&name) {
        for t in &toks[1..] {
            if t.starts_with('-') {
                continue;
            }
            let b = basename(t);
            for ext in [".dll", ".js", ".mjs", ".cjs", ".py", ".jar", ".rb"] {
                if let Some(stem) = b.strip_suffix(ext) {
                    return stem.to_string();
                }
            }
        }
    }
    if name.is_empty() {
        comm.to_string()
    } else {
        name.to_string()
    }
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
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

fn print_header(grand: u64, paint: Paint) {
    let Ok(mi) = fs::read_to_string("/proc/meminfo") else { return };
    let kb = |key: &str| -> u64 {
        mi.lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    let line = format!(
        "RAM {} · {} tillgängligt · cache {} · swap {} · processer {} (PSS)",
        human(kb("MemTotal:") * 1024),
        human(kb("MemAvailable:") * 1024),
        human(kb("Cached:") * 1024),
        human((kb("SwapTotal:") - kb("SwapFree:")) * 1024),
        human(grand),
    );
    println!("{}", paint.w(Paint::DIM, &line));
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
