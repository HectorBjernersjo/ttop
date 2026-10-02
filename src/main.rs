//! ttop — vad äter min maskin, per tmux-session.
//!
//! Grupperar alla processer per tmux-session (sessioner sorterade på total
//! förbrukning), och inom varje session per processtyp. Containrar,
//! maskiner och Kubernetes utanför tmux samlas i en egen hink, en rad per
//! container eller kluster (se containers.rs), och VM:ar i en till, en rad
//! per VM med namnet ur qemus `-name`. Övriga processer hamnar i
//! en hink för allt utanför tmux. Default beskärs outputen till det som
//! faktiskt förklarar förbrukningen; `--all` visar allt.
//!
//! Varje rad visar både minne och CPU. Sortering och stapel följer ett av
//! måtten, minne (default) eller CPU (`--cpu`), men en rad visas om den är
//! stor i något av dem.
//!
//! Minne: RAM + swap, eftersom båda frigörs när processen dör. RAM är PSS
//! och swap SwapPss ur /proc/<pid>/smaps_rollup när det går att läsa (delade
//! sidor räknas proportionellt, så summorna går ihop), annars VmRSS/VmSwap.
//! zram och zswap visas som egna rader: det komprimerade innehållet ligger i
//! RAM men tillhör ingen process. Samma sidor syns alltså två gånger,
//! okomprimerat som processens swap och komprimerat som zram/zswaps RAM.
//!
//! CPU: utime+stime ur /proc/<pid>/stat, avläst före och efter att minnet
//! läses, minst en sekund emellan. Den dyra minnesläsningen ryms alltså i
//! mätfönstret och kostar ingen extra tid. Visas i procent av en kärna, så
//! 400 % är fyra fulla kärnor. ttops eget arbete räknas inte.
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

mod containers;

/// println! som avslutar tyst när läsaren stängt pipen (`ttop | head`),
/// istället för att panika.
macro_rules! out {
    ($($arg:tt)*) => {{
        use std::io::Write;
        if let Err(e) = writeln!(std::io::stdout(), $($arg)*) {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                std::process::exit(0);
            }
            panic!("could not write to stdout: {e}");
        }
    }};
}

const OUTSIDE: &str = "[outside tmux]";
const CONTAINERS: &str = "[containers]";
const VMS: &str = "[VMs]";
const ZRAM: &str = "[zram]";
const ZSWAP: &str = "[zswap]";
const BAR_WIDTH: usize = 20;
// Längre namn kortas, så att ett långt klusternamn inte breddar hela tabellen.
const NAME_MAX: usize = 40;
// Rader visas vid >= 1 % av sessionens förbrukning; sessioner vid >= 2 % av
// totalen — en session ska förtjäna sin plats mer än en enskild rad.
const ROW_MIN_PERCENT: u64 = 1;
const SESSION_MIN_PERCENT: u64 = 2;
// CPU under 2 % av en kärna är brus och tar inte en egen rad.
const CPU_FLOOR: u64 = 200;
const CPU_SAMPLE: Duration = Duration::from_secs(1);
// Linux-default; libc::sysconf(_SC_CLK_TCK) hade varit rätt men kostar ett beroende.
const CLK_TCK: u64 = 100;

/// Det som sorteras och ritas som stapel.
#[derive(Clone, Copy, PartialEq)]
enum Metric {
    Mem,
    Cpu,
}

impl Metric {
    const ALL: [Metric; 2] = [Metric::Mem, Metric::Cpu];

    /// Minsta värde som kan ge en egen rad.
    fn floor(self) -> u64 {
        match self {
            Metric::Mem => 1,
            Metric::Cpu => CPU_FLOOR,
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
    cpu: u64, // hundradels procent av en kärna, under mätfönstret
    tmux_pane_env: Option<String>, // "%194"
    cgroup: Option<String>,
    vm: Option<String>, // qemus -name
}

#[derive(Default)]
struct Usage {
    ram: u64,  // bytes
    swap: u64, // bytes
    cpu: u64,  // hundradels procent av en kärna
}

impl Usage {
    fn add(&mut self, o: &Usage) {
        self.ram += o.ram;
        self.swap += o.swap;
        self.cpu += o.cpu;
    }

    fn mem(&self) -> u64 {
        self.ram + self.swap
    }

    fn get(&self, m: Metric) -> u64 {
        match m {
            Metric::Mem => self.mem(),
            Metric::Cpu => self.cpu,
        }
    }

    /// Sorteringsnyckel: måttet, och vid lika det andra.
    fn rank(&self, m: Metric) -> (u64, u64) {
        match m {
            Metric::Mem => (self.mem(), self.cpu),
            Metric::Cpu => (self.cpu, self.mem()),
        }
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
    const BLUE: &'static str = "34";
    const MAGENTA: &'static str = "35";
    const CYAN: &'static str = "36";

    fn w(self, code: &str, s: &str) -> String {
        if self.on && !s.is_empty() {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
}

fn main() {
    let mut all = false;
    let mut sort = Metric::Mem;
    for a in env::args().skip(1) {
        match a.as_str() {
            "-a" | "--all" => all = true,
            "-c" | "--cpu" => sort = Metric::Cpu,
            "-m" | "--mem" => sort = Metric::Mem,
            "-h" | "--help" => {
                out!("ttop [--mem|--cpu] [--all]\n  memory (RAM + swap) and CPU per tmux session; CPU is sampled for at least one second\n  containers, VMs and Kubernetes outside tmux get one row per container, VM or cluster\n  --mem  sort by memory (default)\n  --cpu  sort by CPU\n  --all  show every row and session");
                return;
            }
            _ => {
                eprintln!("unknown flag: {a}");
                std::process::exit(2);
            }
        }
    }
    let paint = Paint {
        on: std::io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none(),
    };

    let start = CpuStart::take();
    let mut procs = read_procs();
    let cpu = start.finish(&mut procs);
    let (pane_pid_to_session, pane_id_to_session) = tmux_panes();
    let ppid: HashMap<u32, u32> = procs.iter().map(|p| (p.pid, p.ppid)).collect();

    // session -> processtyp -> (antal, förbrukning)
    let mut by_session: HashMap<String, HashMap<String, (u32, Usage)>> = HashMap::new();
    let mut containers = containers::Collector::default();
    let mut any_rss_fallback = false;
    for p in &procs {
        let usage = Usage { ram: p.ram, swap: p.swap, cpu: p.cpu };
        // En egen cgroup (libvirt) går före qemus namn: den räknas exakt.
        let member = p.cgroup.as_deref().and_then(containers::classify).or_else(|| {
            let name = p.vm.clone()?;
            Some(containers::vm(p.cgroup.as_deref(), name))
        });
        if usage.mem() == 0 && usage.cpu == 0 {
            // Kärntrådar, sovande processer m.m. En sovande pod ska ändå
            // räknas i sitt klusters antal pods.
            if let Some(m) = member {
                containers.seen(m);
            }
            continue;
        }
        // tmux vinner över containern: kör tmux själv i en container
        // (distrobox, toolbox) ska sessionerna ändå synas som sessioner.
        let session = match (session_of(p, &ppid, &pane_pid_to_session, &pane_id_to_session), member) {
            (Some(s), m) => {
                if let Some(m) = m {
                    containers.mark_in_tmux(m);
                }
                s
            }
            (None, Some(m)) => {
                containers.add(m, &usage, p.pss);
                continue;
            }
            (None, None) => OUTSIDE.to_string(),
        };
        any_rss_fallback |= !p.pss;
        // Flera VM:ar i samma session ska gå att skilja åt.
        let row = match &p.vm {
            Some(vm) => format!("qemu {vm}"),
            None => p.name.clone(),
        };
        let e = by_session.entry(session).or_default().entry(row).or_default();
        e.0 += 1;
        e.1.add(&usage);
    }
    let (groups, groups_rss) = containers.groups(&cpu.cgroups);
    any_rss_fallback |= groups_rss;
    for g in groups {
        let bucket = if g.vm { VMS } else { CONTAINERS };
        by_session.entry(bucket.to_string()).or_default().insert(g.name, (g.procs, g.usage));
    }
    let swaps = read_swaps();
    let zswap = read_zswap();
    let mut kernel_row = |name: &str, ram: u64| {
        by_session
            .entry(OUTSIDE.to_string())
            .or_default()
            .insert(name.to_string(), (0, Usage { ram, ..Default::default() }));
    };
    let zram: Vec<&SwapDev> = swaps.iter().filter(|d| d.zram_ram.is_some()).collect();
    if !zram.is_empty() {
        kernel_row(ZRAM, zram.iter().filter_map(|d| d.zram_ram).sum());
    }
    if let Some(z) = zswap.as_ref().filter(|z| z.ram > 0) {
        kernel_row(ZSWAP, z.ram);
    }

    let mut sessions: Vec<Session> = by_session
        .into_iter()
        .map(|(name, types)| {
            let mut rows: Vec<Row> = types
                .into_iter()
                .map(|(name, (count, usage))| Row { name, count, usage })
                .collect();
            rows.sort_by(|a, b| b.usage.rank(sort).cmp(&a.usage.rank(sort)));
            let total = sum(rows.iter().map(|r| &r.usage));
            Session { name, rows, total }
        })
        .collect();
    sessions.sort_by(|a, b| b.total.rank(sort).cmp(&a.total.rank(sort)));
    let grand = sum(sessions.iter().map(|s| &s.total));

    // Sessioner under gränsen i båda måtten kollapsar till en samlingsrad.
    let n_show = show_first(&mut sessions, |i, s| {
        all || i == 0 || Metric::ALL.iter().any(|&m| notable(m, &s.total, &grand, SESSION_MIN_PERCENT))
    });
    // En rad visas om den är stor i ett mått där sessionen själv är stor.
    // Annars skulle en session som bara syns för sin CPU visa varje liten
    // process, eftersom de alla är stora jämfört med sessionens lilla minne.
    let visible: Vec<usize> = sessions[..n_show]
        .iter_mut()
        .map(|s| {
            let ms: Vec<Metric> =
                Metric::ALL.into_iter().filter(|&m| notable(m, &s.total, &grand, SESSION_MIN_PERCENT)).collect();
            let total = &s.total;
            show_first(&mut s.rows, |i, r| {
                all || i == 0 || ms.iter().any(|&m| notable(m, &r.usage, total, ROW_MIN_PERCENT))
            })
        })
        .collect();

    let sys = system_rows(&grand, &swaps, zswap.as_ref(), &cpu);
    // Processnamn indenteras två steg under sessionen; alla tabeller delar kolumner.
    let name_w = sessions[..n_show]
        .iter()
        .zip(&visible)
        .flat_map(|(s, &k)| {
            std::iter::once(s.name.chars().count())
                .chain(s.rows[..k].iter().map(|r| r.name.chars().count() + 2))
        })
        .chain(sys.iter().map(|r| r.name.chars().count()))
        .max()
        .unwrap_or(0)
        .clamp(22, NAME_MAX);
    let t = Table { sort, name_w, grand: grand.get(sort), paint };
    let procs_in = |rows: &[Row]| rows.iter().map(|r| r.count).sum::<u32>();

    // Stigande ordning: det största hamnar längst ner, närmast prompten.
    t.header();
    if n_show < sessions.len() {
        let rest = &sessions[n_show..];
        t.row(
            &format!("… {} more sessions", rest.len()),
            rest.iter().map(|s| procs_in(&s.rows)).sum(),
            &sum(rest.iter().map(|s| &s.total)),
            Style::Dim,
        );
    }
    for (s, &k) in sessions[..n_show].iter().zip(&visible).rev() {
        out!();
        t.row(&s.name, procs_in(&s.rows), &s.total, Style::Head);
        // Rader fallande: sessionens största process direkt under rubriken.
        for r in &s.rows[..k] {
            t.row(&format!("  {}", r.name), r.count, &r.usage, Style::Normal);
        }
        if k < s.rows.len() {
            let rest = &s.rows[k..];
            // Processrader är processtyper; container- och VM-rader är redan en per enhet.
            let what = if s.name == CONTAINERS || s.name == VMS { "more" } else { "more types" };
            t.row(
                &format!("  … {} {what}", rest.len()),
                procs_in(rest),
                &sum(rest.iter().map(|r| &r.usage)),
                Style::Dim,
            );
        }
    }
    t.rule();
    t.row("total", sessions.iter().map(|s| procs_in(&s.rows)).sum(), &grand, Style::Total);
    if any_rss_fallback {
        out!(
            "{}",
            paint.w(
                Paint::DIM,
                "(some processes could not be read as PSS; RSS/VmSwap used instead, may overcount shared memory)",
            )
        );
    }

    out!();
    t.system(&sys);
}

const VAL_W: usize = 9;

#[derive(Clone, Copy, PartialEq)]
enum Style {
    Head,
    Total, // som Head, men utan andel: den är alltid 100 %
    Normal,
    Dim,
}

/// Kolumnerna: namn, antal processer, summa, RAM, swap, CPU och en stapel
/// för sorteringsmåttets andel av totalen.
struct Table {
    sort: Metric,
    name_w: usize,
    grand: u64, // sorteringsmåttets total, stapelns skala
    paint: Paint,
}

const N_VALS: usize = 4;

impl Table {
    fn text(&self, name: &str, count: &str, vals: &[String]) -> String {
        let name = shorten(name, self.name_w);
        let mut s = format!("{name:<w$} {count:>5}", w = self.name_w);
        for v in vals {
            s += &format!(" {v:>VAL_W$}");
        }
        s
    }

    fn header(&self) {
        let legend = match self.sort {
            Metric::Mem => format!("{} RAM  {} swap", self.paint.w(Paint::CYAN, "█"), self.paint.w(Paint::MAGENTA, "▓")),
            Metric::Cpu => format!("{} CPU", self.paint.w(Paint::BLUE, "█")),
        };
        let cols = ["total", "RAM", "swap", "CPU"].map(String::from);
        let head = format!("{}  {legend}", self.paint.w(Paint::DIM, &self.text("", "procs", &cols)));
        out!("{}", head.trim_end());
    }

    fn rule(&self) {
        let w = self.name_w + 6 + N_VALS * (VAL_W + 1) + 2 + BAR_WIDTH;
        out!("{}", self.paint.w(Paint::DIM, &"─".repeat(w)));
    }

    /// count 0 lämnar antal tomt, t.ex. för zram som inte är en process.
    fn row(&self, name: &str, count: u32, u: &Usage, style: Style) {
        let count = if count == 0 { String::new() } else { count.to_string() };
        let or_dash = |v: u64, f: fn(u64) -> String| if v == 0 { "–".to_string() } else { f(v) };
        let vals = [or_dash(u.mem(), human), or_dash(u.ram, human), or_dash(u.swap, human), or_dash(u.cpu, cpu)];
        let text = self.text(name, &count, &vals);
        let text = match style {
            Style::Head | Style::Total => self.paint.w(Paint::BOLD, &text),
            Style::Normal => text,
            Style::Dim => self.paint.w(Paint::DIM, &text),
        };
        out!("{}", format!("{text}  {}", self.share_bar(u, style)).trim_end());
    }

    /// Sorteringsmåttets andel av totalen; för minne uppdelad i RAM och swap.
    /// Samma skala på alla rader, så staplarna går att jämföra mellan
    /// sessioner. Räknas i åttondelar av ett tecken, så även en procent syns;
    /// RAM-delen avrundas till hela tecken.
    fn share_bar(&self, u: &Usage, style: Style) -> String {
        const PARTIAL: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
        let eighths = |v: u64| {
            if self.grand == 0 {
                return 0;
            }
            let w = BAR_WIDTH as u128 * 8;
            ((v as u128 * w + self.grand as u128 / 2) / self.grand as u128) as usize
        };
        let total = eighths(u.get(self.sort)).min(BAR_WIDTH * 8);
        let head = match self.sort {
            Metric::Mem => (eighths(u.ram) + 4) / 8,
            Metric::Cpu => total / 8,
        }
        .min(total / 8);
        let rest = total - head * 8; // swap, eller CPU:s sista bråkdel
        let (head_c, rest_c) = match (style, self.sort) {
            (Style::Dim, _) => (Paint::DIM, Paint::DIM),
            (_, Metric::Mem) => (Paint::CYAN, Paint::MAGENTA),
            (_, Metric::Cpu) => (Paint::BLUE, Paint::BLUE),
        };
        let full = if self.sort == Metric::Mem { "▓" } else { "█" };
        let tail = format!("{}{}", full.repeat(rest / 8), PARTIAL[rest % 8]);
        format!("{}{}", self.paint.w(head_c, &"█".repeat(head)), self.paint.w(rest_c, &tail))
    }

    /// RAM, swap-enheterna och CPU, i samma kolumner som tabellen ovanför,
    /// så att fyllnadsstaplarna hamnar under andelsstaplarna.
    fn system(&self, rows: &[SysRow]) {
        let cols = ["used", "total", "free", "%"].map(String::from);
        out!("{}", self.paint.w(Paint::DIM, &self.text("", "", &cols)));
        for r in rows {
            let pct = if r.size > 0 { r.used * 100 / r.size } else { 0 };
            let free = r.size.saturating_sub(r.used);
            let vals = [(r.fmt)(r.used), (r.fmt)(r.size), (r.fmt)(free), format!("{pct}%")];
            let text = self.text(&r.name, "", &vals);
            out!(
                "{}  {}  {}",
                self.paint.w(Paint::BOLD, &text),
                fill_bar(pct, self.paint),
                self.paint.w(Paint::DIM, &r.note),
            );
        }
    }
}

/// Kortar till `w` tecken med "…". Det som står efter " · " (antal pods,
/// containrar) behålls, så att det är själva namnet som kortas.
fn shorten(name: &str, w: usize) -> String {
    if name.chars().count() <= w {
        return name.to_string();
    }
    let (head, tail) = match name.rfind(" · ") {
        Some(i) => (&name[..i], &name[i..]),
        None => (name, ""),
    };
    let keep = w.saturating_sub(tail.chars().count() + 1);
    format!("{}…{tail}", head.chars().take(keep).collect::<String>())
}

/// Hur full en resurs är; färgen varnar när den närmar sig full.
fn fill_bar(pct: u64, paint: Paint) -> String {
    let filled = ((pct as usize * BAR_WIDTH + 50) / 100).clamp(usize::from(pct > 0), BAR_WIDTH);
    let color = match pct {
        90.. => Paint::RED,
        70.. => Paint::YELLOW,
        _ => Paint::GREEN,
    };
    format!(
        "{}{}",
        paint.w(color, &"█".repeat(filled)),
        paint.w(Paint::DIM, &"░".repeat(BAR_WIDTH - filled)),
    )
}

/// Om `part` är stor nog för en egen rad jämfört med `whole` i måttet `m`.
fn notable(m: Metric, part: &Usage, whole: &Usage, pct: u64) -> bool {
    let (p, w) = (part.get(m), whole.get(m));
    p >= m.floor() && p * 100 >= w * pct
}

/// Flyttar det som ska visas först, med ordningen bevarad inom båda
/// delarna, och returnerar hur många det är. Döljs bara en visas den
/// ändå: "… 1 till" tar samma plats som raden själv.
fn show_first<T>(items: &mut Vec<T>, show: impl Fn(usize, &T) -> bool) -> usize {
    let mut flags: Vec<bool> = items.iter().enumerate().map(|(i, x)| show(i, x)).collect();
    if flags.iter().filter(|&&f| !f).count() == 1 {
        flags.fill(true);
    }
    let n = flags.iter().filter(|&&f| f).count();
    let mut tagged: Vec<(bool, T)> = flags.into_iter().zip(items.drain(..)).collect();
    tagged.sort_by_key(|(f, _)| !f); // stabil
    items.extend(tagged.into_iter().map(|(_, x)| x));
    n
}

fn pids() -> Vec<u32> {
    let Ok(dir) = fs::read_dir("/proc") else { return Vec::new() };
    dir.flatten().filter_map(|e| e.file_name().to_str()?.parse().ok()).collect()
}

/// Det dyra: smaps_rollup för varje process. Körs inuti CPU-mätfönstret.
fn read_procs() -> Vec<Proc> {
    let mut out = Vec::new();
    for pid in pids() {
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
        let argv = argv(&base);
        out.push(Proc {
            pid,
            ppid,
            name: proc_name(&argv, &comm),
            ram: ram_kb * 1024,
            swap: swap_kb * 1024,
            pss,
            cpu: 0,
            tmux_pane_env: tmux_pane_from_environ(&base),
            cgroup: containers::cgroup_path(pid),
            vm: containers::qemu_name(&argv),
        });
    }
    out
}

/// Första CPU-avläsningen: maskinen, varje process och varje containers
/// cgroup. Den andra görs av `finish`, efter att minnet lästs.
struct CpuStart {
    at: Instant,
    machine: Option<(u64, u64)>,
    procs: HashMap<u32, u64>,
    cgroups: HashMap<String, u64>,
}

/// CPU under mätfönstret, i hundradels procent av en kärna.
struct Cpu {
    secs: f64,
    cores: u64,
    /// Hela maskinen enligt /proc/stat, utan ttop själv. Räknar till
    /// skillnad från processerna även de som hann avslutas.
    machine: Option<u64>,
    cgroups: HashMap<String, u64>,
}

impl CpuStart {
    fn take() -> CpuStart {
        let at = Instant::now();
        let machine = machine_ticks().map(|(busy, all, _)| (busy, all));
        let mut procs = HashMap::new();
        let mut cgroups = HashMap::new();
        for pid in pids() {
            if let Some(t) = cpu_ticks(pid) {
                procs.insert(pid, t);
            }
            let Some(m) = containers::cgroup_path(pid).as_deref().and_then(containers::classify) else { continue };
            if !cgroups.contains_key(&m.dir)
                && let Some(usec) = containers::cpu_usec(&m.dir)
            {
                cgroups.insert(m.dir, usec);
            }
        }
        CpuStart { at, machine, procs, cgroups }
    }

    /// Väntar tills fönstret är minst CPU_SAMPLE och sätter varje process
    /// cpu. En process som startat under fönstret räknas från noll; en som
    /// dött får 0.
    fn finish(self, procs: &mut [Proc]) -> Cpu {
        if let Some(rest) = CPU_SAMPLE.checked_sub(self.at.elapsed()) {
            thread::sleep(rest);
        }
        let secs = self.at.elapsed().as_secs_f64();
        let rate = |seconds: f64| (seconds / secs * 10_000.0).round() as u64;
        let me = std::process::id();
        let mut own = 0;
        for p in procs.iter_mut() {
            let Some(after) = cpu_ticks(p.pid) else { continue };
            let delta = after.saturating_sub(self.procs.get(&p.pid).copied().unwrap_or(0));
            let v = rate(delta as f64 / CLK_TCK as f64);
            if p.pid == me {
                own = v; // smaps-läsningen; den mäter vi inte
            } else {
                p.cpu = v;
            }
        }
        let cgroups = self
            .cgroups
            .into_iter()
            .filter_map(|(dir, before)| {
                let after = containers::cpu_usec(&dir)?;
                let v = rate(after.saturating_sub(before) as f64 / 1e6);
                Some((dir, v))
            })
            .collect();
        let now = machine_ticks();
        let cores = now.map_or(1, |(_, _, n)| n);
        // Andel av alla kärnors tid: tåligt mot att avläsningarna inte
        // görs exakt samtidigt som klockan läses.
        let machine = self.machine.zip(now).and_then(|((b0, a0), (b1, a1, _))| {
            let all = a1.checked_sub(a0).filter(|&d| d > 0)?;
            let busy = b1.saturating_sub(b0);
            Some(((busy as f64 / all as f64) * cores as f64 * 10_000.0).round() as u64)
        });
        Cpu { secs, cores, machine: machine.map(|m| m.saturating_sub(own)), cgroups }
    }
}

/// (upptagen, totalt, antal kärnor) ur /proc/stat, i ticks över alla kärnor.
fn machine_ticks() -> Option<(u64, u64, u64)> {
    let stat = fs::read_to_string("/proc/stat").ok()?;
    let mut lines = stat.lines();
    // cpu user nice system idle iowait irq softirq steal guest guest_nice;
    // guest ingår redan i user.
    let f: Vec<u64> = lines.next()?.split_whitespace().skip(1).take(8).filter_map(|v| v.parse().ok()).collect();
    let all: u64 = f.iter().sum();
    let idle = f.get(3)? + f.get(4)?;
    let cores = lines.filter(|l| l.starts_with("cpu")).count() as u64;
    Some((all - idle, all, cores.max(1)))
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
/// /proc/<pid>/cmdline som argv; tom för kärntrådar eller om den inte går att läsa.
fn argv(base: &str) -> Vec<String> {
    let Ok(raw) = fs::read(format!("{base}/cmdline")) else { return Vec::new() };
    raw.split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .filter_map(|s| std::str::from_utf8(s).ok().map(str::to_string))
        .collect()
}

fn proc_name(argv: &[String], comm: &str) -> String {
    // Processer som skriver om sin titel ("npm exec …") har mellanslag i
    // ett enda argv-fält; platta ut till tokens oavsett.
    let toks: Vec<&str> = argv.iter().flat_map(|a| a.split_whitespace()).collect();
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
) -> Option<String> {
    // Förälderkedjan: pid, förälder, farförälder ... tills en pane-pid.
    let mut cur = p.pid;
    for _ in 0..128 {
        if let Some(sess) = by_pane_pid.get(&cur) {
            return Some(sess.clone());
        }
        match ppid.get(&cur) {
            Some(&parent) if parent > 1 => cur = parent,
            _ => break,
        }
    }
    // Föräldralös: TMUX_PANE i environ pekar ut panen som startade den.
    p.tmux_pane_env.as_ref().and_then(|id| by_pane_id.get(id)).cloned()
}

struct SysRow {
    name: String,
    used: u64,
    size: u64,
    fmt: fn(u64) -> String,
    note: String,
}

/// En rad för RAM, en för zswap om den är på, en per swap-enhet och en för CPU.
fn system_rows(grand: &Usage, swaps: &[SwapDev], zswap: Option<&Zswap>, sample: &Cpu) -> Vec<SysRow> {
    let Ok(mi) = fs::read_to_string("/proc/meminfo") else { return Vec::new() };
    let b = |key: &str| meminfo(&mi, key);
    let total = b("MemTotal:");
    let avail = b("MemAvailable:");
    let kernel_ram = swaps.iter().filter_map(|d| d.zram_ram).sum::<u64>() + zswap.map_or(0, |z| z.ram);
    let compressed = |data: u64, ram: u64| format!("{:.1}× compressed", data as f64 / ram as f64);
    let mut rows = vec![SysRow {
        name: "RAM".to_string(),
        used: total - avail,
        size: total,
        fmt: human,
        note: format!("cache {} · processes {}", human(b("Cached:")), human(grand.ram - kernel_ram)),
    }];
    if let Some(z) = zswap {
        let ratio = if z.ram > 0 { format!(" ({})", compressed(z.data, z.ram)) } else { String::new() };
        rows.push(SysRow {
            name: "zswap".to_string(),
            used: z.ram,
            size: z.limit,
            fmt: human,
            note: format!("{} data{ratio} · counted in the swap devices' used", human(z.data)),
        });
    }
    for d in swaps {
        let note = match d.zram_ram {
            Some(ram) if ram > 0 => format!("zram, uses {} RAM ({})", human(ram), compressed(d.used, ram)),
            Some(_) => "zram".to_string(),
            None => "disk".to_string(),
        };
        rows.push(SysRow { name: format!("swap {}", d.name), used: d.used, size: d.size, fmt: human, note });
    }
    if let Some(used) = sample.machine {
        let load = fs::read_to_string("/proc/loadavg")
            .map(|s| s.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        rows.push(SysRow {
            name: "CPU".to_string(),
            used,
            size: sample.cores * 10_000,
            fmt: cpu,
            note: format!(
                "processes {} · {} cores · load {load} · sampled {:.1} s",
                cpu(grand.cpu),
                sample.cores,
                sample.secs
            ),
        });
    }
    rows
}

/// Hundradels procent av en kärna -> "12.3%".
fn cpu(v: u64) -> String {
    format!("{:.1}%", v as f64 / 100.0)
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

#[cfg(test)]
mod tests {
    use super::{Metric, Usage, notable, shorten, show_first};

    #[test]
    fn shown_items_first_in_order_and_a_lone_hidden_one_is_shown() {
        let mut v = vec![5, 1, 4, 2, 3];
        assert_eq!(show_first(&mut v, |_, &x| x >= 3), 3);
        assert_eq!(v, [5, 4, 3, 1, 2]);
        let mut v = vec![5, 1, 4];
        assert_eq!(show_first(&mut v, |_, &x| x >= 3), 3);
        assert_eq!(v, [5, 1, 4]);
    }

    #[test]
    fn cpu_noise_does_not_earn_a_row() {
        let whole = Usage { cpu: 300, ..Default::default() };
        let tiny = Usage { cpu: 100, ..Default::default() };
        assert!(!notable(Metric::Cpu, &tiny, &whole, 1));
        assert!(notable(Metric::Cpu, &Usage { cpu: 250, ..Default::default() }, &whole, 1));
        assert!(!notable(Metric::Mem, &tiny, &whole, 1));
    }

    #[test]
    fn shorten_keeps_the_count_suffix() {
        assert_eq!(shorten("k3d abc · 3 pods", 40), "k3d abc · 3 pods");
        assert_eq!(shorten("k3d gbandit-gba-95-thin-pool-monitor · 28 pods", 30), "k3d gbandit-gba-95-… · 28 pods");
        assert_eq!(shorten("abcdefgh", 5), "abcd…");
    }
}
