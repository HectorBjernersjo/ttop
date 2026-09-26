//! ttop — vad äter min maskin, per tmux-session.
//!
//! Grupperar alla processer per tmux-session (sessioner sorterade på total
//! förbrukning), och inom varje session per processtyp. Processer utanför
//! tmux hamnar i en egen hink. Default beskärs outputen till det som
//! faktiskt förklarar förbrukningen; `--all` visar allt.
//!
//! Mått: minne (default) eller CPU (`--cpu`). Ett mått i taget, för hela
//! vyn är sortering och beskärning på just det måttet.
//!
//! Minne: RAM + swap, eftersom båda frigörs när processen dör. RAM är PSS
//! och swap SwapPss ur /proc/<pid>/smaps_rollup när det går att läsa (delade
//! sidor räknas proportionellt, så summorna går ihop), annars VmRSS/VmSwap.
//! zram och zswap visas som egna rader: det komprimerade innehållet ligger i
//! RAM men tillhör ingen process. Samma sidor syns alltså två gånger,
//! okomprimerat som processens swap och komprimerat som zram/zswaps RAM.
//!
//! CPU: utime+stime ur /proc/<pid>/stat, samplat två gånger med en sekund
//! emellan. Visas i procent av en kärna, så 400 % är fyra fulla kärnor.
//!
//! Sessionstillhörighet: förälderkedjan upp till en tmux-pane-pid. Faller
//! tillbaka på TMUX_PANE i processens environ — det fångar föräldralösa
//! daemons (t.ex. MSBuild-noder med PPID 1) som annars ser sessionslösa ut.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::IsTerminal;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

const OUTSIDE: &str = "[utanför tmux]";
const ZRAM: &str = "[zram]";
const ZSWAP: &str = "[zswap]";
const BAR_WIDTH: usize = 20;
// Rader visas vid >= 1 % av sessionens förbrukning; sessioner vid >= 2 % av
// totalen — en session ska förtjäna sin plats mer än en enskild rad.
const ROW_MIN_PERCENT: u64 = 1;
const SESSION_MIN_PERCENT: u64 = 2;
const CPU_SAMPLE: Duration = Duration::from_secs(1);
// Linux-default; libc::sysconf(_SC_CLK_TCK) hade varit rätt men kostar ett beroende.
const CLK_TCK: u64 = 100;

#[derive(Clone, Copy, PartialEq)]
enum Metric {
    Mem,
    Cpu,
}

impl Metric {
    /// Värdet är bytes (RAM + swap) för Mem, hundradels procent av en kärna för Cpu.
    fn fmt(self, v: u64) -> String {
        match self {
            Metric::Mem => human(v),
            Metric::Cpu => format!("{:.1}%", v as f64 / 100.0),
        }
    }
}

struct Proc {
    pid: u32,
    ppid: u32,
    name: String,
    ram: u64,  // bytes
    swap: u64, // bytes
    pss: bool,
    cpu_ticks: u64, // utime + stime
    tmux_pane_env: Option<String>, // "%194"
}

#[derive(Default)]
struct Usage {
    value: u64, // det som sorteras och beskärs på
    ram: u64,
    swap: u64,
}

impl Usage {
    fn add(&mut self, o: &Usage) {
        self.value += o.value;
        self.ram += o.ram;
        self.swap += o.swap;
    }
}

struct Row {
    name: String,
    count: u32,
    usage: Usage,
}

struct Session {
    name: String,
    rows: Vec<Row>,
    total: Usage,
}

fn sum<'a>(it: impl Iterator<Item = &'a Usage>) -> Usage {
    let mut u = Usage::default();
    it.for_each(|x| u.add(x));
    u
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
    let mut all = false;
    let mut metric = Metric::Mem;
    for a in env::args().skip(1) {
        match a.as_str() {
            "-a" | "--all" => all = true,
            "-c" | "--cpu" => metric = Metric::Cpu,
            "-m" | "--mem" => metric = Metric::Mem,
            "-h" | "--help" => {
                println!("ttop [--mem|--cpu] [--all]\n  --mem  minne (RAM + swap) per tmux-session (default)\n  --cpu  CPU per tmux-session, samplat under en sekund\n  --all  visa alla rader och sessioner");
                return;
            }
            _ => {
                eprintln!("okänd flagga: {a}");
                std::process::exit(2);
            }
        }
    }
    let paint = Paint {
        on: std::io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none(),
    };

    let mut procs = read_procs();
    if metric == Metric::Cpu {
        sample_cpu(&mut procs);
    }
    let (pane_pid_to_session, pane_id_to_session) = tmux_panes();
    let ppid: HashMap<u32, u32> = procs.iter().map(|p| (p.pid, p.ppid)).collect();

    // session -> processtyp -> (antal, förbrukning)
    let mut by_session: HashMap<String, HashMap<String, (u32, Usage)>> = HashMap::new();
    let mut any_rss_fallback = false;
    for p in &procs {
        let usage = match metric {
            Metric::Mem => Usage { value: p.ram + p.swap, ram: p.ram, swap: p.swap },
            Metric::Cpu => Usage { value: p.cpu_ticks, ..Default::default() },
        };
        if usage.value == 0 {
            continue; // kärntrådar, sovande processer m.m.
        }
        any_rss_fallback |= !p.pss;
        let session = session_of(p, &ppid, &pane_pid_to_session, &pane_id_to_session);
        let e = by_session
            .entry(session)
            .or_default()
            .entry(p.name.clone())
            .or_default();
        e.0 += 1;
        e.1.add(&usage);
    }
    let swaps = read_swaps();
    let zswap = read_zswap();
    if metric == Metric::Mem {
        let mut kernel_row = |name: &str, count: u32, ram: u64| {
            by_session
                .entry(OUTSIDE.to_string())
                .or_default()
                .insert(name.to_string(), (count, Usage { value: ram, ram, swap: 0 }));
        };
        let zram: Vec<&SwapDev> = swaps.iter().filter(|d| d.zram_ram.is_some()).collect();
        if !zram.is_empty() {
            kernel_row(ZRAM, zram.len() as u32, zram.iter().filter_map(|d| d.zram_ram).sum());
        }
        if let Some(z) = zswap.as_ref().filter(|z| z.ram > 0) {
            kernel_row(ZSWAP, 1, z.ram);
        }
    }

    let mut sessions: Vec<Session> = by_session
        .into_iter()
        .map(|(name, types)| {
            let mut rows: Vec<Row> = types
                .into_iter()
                .map(|(name, (count, usage))| Row { name, count, usage })
                .collect();
            rows.sort_by(|a, b| b.usage.value.cmp(&a.usage.value));
            let total = sum(rows.iter().map(|r| &r.usage));
            Session { name, rows, total }
        })
        .collect();
    sessions.sort_by(|a, b| b.total.value.cmp(&a.total.value));
    let grand = sum(sessions.iter().map(|s| &s.total));

    // Sessioner under gränsen kollapsar till en samlingsrad.
    let mut n_show = sessions.len();
    if !all {
        n_show = sessions
            .iter()
            .enumerate()
            .take_while(|(i, s)| *i == 0 || s.total.value * 100 >= grand.value * SESSION_MIN_PERCENT)
            .count();
        if sessions.len() - n_show == 1 {
            n_show = sessions.len(); // "… 1 session till" döljer inget; visa den
        }
    }

    let visible: Vec<usize> = sessions[..n_show]
        .iter()
        .map(|s| visible_row_count(&s.rows, s.total.value, all))
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
        let u = sum(rest.iter().map(|s| &s.total));
        println!(
            "{}",
            paint.w(
                Paint::DIM,
                &format!(
                    "{:<name_w$} {:>9}{}",
                    format!("… {} sessioner till", rest.len()),
                    metric.fmt(u.value),
                    split(metric, &u),
                ),
            )
        );
    }

    for (s, &k) in sessions[..n_show].iter().zip(&visible).rev() {
        let pct = if grand.value > 0 { s.total.value * 100 / grand.value } else { 0 };
        println!(
            "\n{} {}  {} {}{}",
            paint.w(Paint::BOLD, &format!("{:<name_w$}", s.name)),
            paint.w(Paint::BOLD, &format!("{:>9}", metric.fmt(s.total.value))),
            bar(pct, paint),
            paint.w(Paint::DIM, &format!("{pct:>3}%")),
            paint.w(Paint::DIM, &split(metric, &s.total)),
        );
        // Rader fallande: sessionens största process direkt under rubriken.
        for r in &s.rows[..k] {
            println!(
                "  {:<row_w$} {:>4} st {:>9}{}",
                r.name,
                r.count,
                metric.fmt(r.usage.value),
                paint.w(Paint::DIM, &split(metric, &r.usage)),
            );
        }
        if k < s.rows.len() {
            let rest = &s.rows[k..];
            let u = sum(rest.iter().map(|r| &r.usage));
            println!(
                "  {}",
                paint.w(
                    Paint::DIM,
                    &format!(
                        "{:<row_w$} {:7} {:>9}{}",
                        format!("… {} till", rest.len()),
                        "",
                        metric.fmt(u.value),
                        split(metric, &u),
                    ),
                )
            );
        }
    }

    println!();
    if metric == Metric::Mem && any_rss_fallback {
        println!(
            "{}",
            paint.w(
                Paint::DIM,
                "(vissa processer kunde inte läsas som PSS; RSS/VmSwap använt, kan överdriva delat minne)",
            )
        );
    }
    match metric {
        Metric::Mem => print_mem_footer(&grand, &swaps, zswap.as_ref(), paint),
        Metric::Cpu => print_cpu_header(grand.value, paint),
    }
}

/// "   RAM 840 MB  swap 1.6 GB" efter summan; tomt för CPU.
fn split(metric: Metric, u: &Usage) -> String {
    if metric != Metric::Mem {
        return String::new();
    }
    let swap = if u.swap == 0 { "–".to_string() } else { human(u.swap) };
    format!("   RAM {:>9}  swap {:>9}", human(u.ram), swap)
}

/// Visa rader som står för minst ROW_MIN_PERCENT av sessionens förbrukning.
fn visible_row_count(rows: &[Row], total: u64, all: bool) -> usize {
    if all {
        return rows.len();
    }
    let mut k = rows
        .iter()
        .take_while(|r| r.usage.value * 100 >= total * ROW_MIN_PERCENT)
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
        let mut swap_kb = 0u64;
        for line in status.lines() {
            match line.split_once(':') {
                Some(("Name", v)) => comm = v.trim().to_string(),
                Some(("PPid", v)) => ppid = v.trim().parse().unwrap_or(0),
                Some(("VmRSS", v)) => rss_kb = kb_value(v),
                Some(("VmSwap", v)) => swap_kb = kb_value(v),
                _ => {}
            }
        }
        let ((ram_kb, swap_kb), pss) = match pss_kb(&base) {
            Some(v) => (v, true),
            None => ((rss_kb, swap_kb), false),
        };
        out.push(Proc {
            pid,
            ppid,
            name: proc_name(&base, &comm),
            ram: ram_kb * 1024,
            swap: swap_kb * 1024,
            pss,
            cpu_ticks: cpu_ticks(pid).unwrap_or(0),
            tmux_pane_env: tmux_pane_from_environ(&base),
        });
    }
    out
}

/// Byt cpu_ticks från ackumulerat till förbrukning under CPU_SAMPLE, uttryckt
/// i hundradels procent av en kärna. Processer som dött under tiden får 0.
fn sample_cpu(procs: &mut [Proc]) {
    let start = Instant::now();
    thread::sleep(CPU_SAMPLE);
    let elapsed = start.elapsed().as_secs_f64();
    for p in procs {
        let delta = cpu_ticks(p.pid).unwrap_or(p.cpu_ticks).saturating_sub(p.cpu_ticks);
        p.cpu_ticks = (delta as f64 / CLK_TCK as f64 / elapsed * 10_000.0).round() as u64;
    }
}

/// utime + stime ur /proc/<pid>/stat (fält 14 och 15, räknat efter comm).
fn cpu_ticks(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm kan innehålla mellanslag och parenteser; hoppa till sista ')'.
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut f = rest.split_whitespace().skip(11); // fält 3..=13
    let utime: u64 = f.next()?.parse().ok()?;
    let stime: u64 = f.next()?.parse().ok()?;
    Some(utime + stime)
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

/// "  1234 kB" -> 1234
fn kb_value(v: &str) -> u64 {
    v.split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or(0)
}

/// (Pss, SwapPss) i kB ur smaps_rollup.
fn pss_kb(base: &str) -> Option<(u64, u64)> {
    let rollup = fs::read_to_string(format!("{base}/smaps_rollup")).ok()?;
    let field = |key: &str| rollup.lines().find_map(|l| l.strip_prefix(key)).map(kb_value);
    Some((field("Pss:")?, field("SwapPss:").unwrap_or(0)))
}

/// zswap: komprimerad cache i RAM framför swap-enheterna. Sidorna har
/// fortfarande en plats reserverad på enheten och räknas i dess "använt".
struct Zswap {
    ram: u64,   // komprimerat, det RAM poolen tar
    data: u64,  // okomprimerat
    limit: u64, // max_pool_percent av RAM
}

/// None när zswap är avslaget och tomt.
fn read_zswap() -> Option<Zswap> {
    let mi = fs::read_to_string("/proc/meminfo").ok()?;
    let param = |name: &str| fs::read_to_string(format!("/sys/module/zswap/parameters/{name}")).ok();
    let enabled = param("enabled").is_some_and(|v| v.trim() == "Y");
    let (ram, data) = (meminfo(&mi, "Zswap:"), meminfo(&mi, "Zswapped:"));
    if !enabled && data == 0 {
        return None;
    }
    let pct: u64 = param("max_pool_percent").and_then(|v| v.trim().parse().ok()).unwrap_or(20);
    Some(Zswap { ram, data, limit: meminfo(&mi, "MemTotal:") * pct / 100 })
}

/// Ett fält ur /proc/meminfo i bytes.
fn meminfo(mi: &str, key: &str) -> u64 {
    mi.lines().find_map(|l| l.strip_prefix(key)).map(kb_value).unwrap_or(0) * 1024
}

struct SwapDev {
    name: String, // "zram0", "swapfile"
    size: u64,    // bytes
    used: u64,
    zram_ram: Option<u64>, // RAM som zram-enheten faktiskt tar
}

/// Swap-enheter ur /proc/swaps, i den ordning kernel fyller dem.
fn read_swaps() -> Vec<SwapDev> {
    let Ok(s) = fs::read_to_string("/proc/swaps") else { return Vec::new() };
    let mut devs: Vec<(i64, SwapDev)> = s
        .lines()
        .skip(1)
        .filter_map(|l| {
            // Filename Type Size Used Priority; filnamnet kan innehålla mellanslag.
            let mut f = l.split_whitespace().rev();
            let prio: i64 = f.next()?.parse().ok()?;
            let used: u64 = f.next()?.parse().ok()?;
            let size: u64 = f.next()?.parse().ok()?;
            let path = l.split_whitespace().next()?;
            let name = basename(path).to_string();
            // mm_stat fält 3: mem_used_total, komprimerad data plus overhead.
            let zram_ram = name
                .starts_with("zram")
                .then(|| fs::read_to_string(format!("/sys/block/{name}/mm_stat")).ok())
                .flatten()
                .and_then(|m| m.split_whitespace().nth(2)?.parse().ok());
            Some((prio, SwapDev { name, size: size * 1024, used: used * 1024, zram_ram }))
        })
        .collect();
    devs.sort_by(|a, b| b.0.cmp(&a.0));
    devs.into_iter().map(|(_, d)| d).collect()
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

/// En rad för RAM och en per swap-enhet, "använt / totalt".
fn print_mem_footer(grand: &Usage, swaps: &[SwapDev], zswap: Option<&Zswap>, paint: Paint) {
    let Ok(mi) = fs::read_to_string("/proc/meminfo") else { return };
    let b = |key: &str| meminfo(&mi, key);
    let total = b("MemTotal:");
    let avail = b("MemAvailable:");
    let label_w = swaps.iter().map(|d| d.name.chars().count() + 5).max().unwrap_or(0).max(5);
    let used_of = |used: u64, size: u64| {
        let pct = if size > 0 { used * 100 / size } else { 0 };
        format!("{:>9} / {:<9} {pct:>3}%", human(used), human(size))
    };
    let mut lines = vec![format!(
        "{:<label_w$}  {} · {} tillgängligt · cache {} · processer {}",
        "RAM",
        used_of(total - avail, total),
        human(avail),
        human(b("Cached:")),
        human(
            grand.ram
                - swaps.iter().filter_map(|d| d.zram_ram).sum::<u64>()
                - zswap.map_or(0, |z| z.ram)
        ),
    )];
    if let Some(z) = zswap {
        let ratio = if z.ram > 0 { format!(" ({:.1}× komprimerat)", z.data as f64 / z.ram as f64) } else { String::new() };
        lines.push(format!(
            "{:<label_w$}  {} · {} data{ratio} · ingår i swap-enheternas använt",
            "zswap",
            used_of(z.ram, z.limit),
            human(z.data),
        ));
    }
    for d in swaps {
        let what = match d.zram_ram {
            Some(ram) if ram > 0 => format!("tar {} RAM ({:.1}× komprimerat)", human(ram), d.used as f64 / ram as f64),
            Some(_) => "zram".to_string(),
            None => "disk".to_string(),
        };
        lines.push(format!(
            "{:<label_w$}  {} · {} ledigt · {what}",
            format!("swap {}", d.name),
            used_of(d.used, d.size),
            human(d.size.saturating_sub(d.used)),
        ));
    }
    for l in lines {
        println!("{}", paint.w(Paint::DIM, &l));
    }
}

fn print_cpu_header(grand: u64, paint: Paint) {
    let cores = thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let load = fs::read_to_string("/proc/loadavg")
        .map(|s| s.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    let line = format!(
        "{cores} kärnor · load {load} · processer {} av {}% (1 s)",
        Metric::Cpu.fmt(grand),
        cores * 100,
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
