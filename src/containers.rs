//! Containrar, maskiner och Kubernetes, utpekade via processens cgroup.
//! VM:ar som inte har en egen cgroup (Incus kör qemu i incus.service)
//! känns istället igen på qemus kommandorad, se `vm`.
//!
//! /proc/<pid>/cgroup är läsbar för alla, och varje runtime namnger sina
//! cgroups efter ett känt mönster (tabellen i `classify`). Den yttersta
//! träffen i sökvägen vinner, så allt som körs inuti en container räknas
//! till den: ett Kubernetes-kluster som körs i docker (k3d, kind, minikube)
//! blir en rad, inte en per pod.
//!
//! Minnet läses från enhetens cgroup (v2): exakt och utan root. Processernas
//! smaps kräver att man äger processen, och containerprocesser kör oftast som
//! andra användare, så alternativet vore RSS som dubbelräknar delat minne.
//! Utan cgroup v2 summeras processerna som för allt annat.
//!
//! CPU likaså: cgroupens usage_usec, avläst i början och slutet av
//! mätfönstret, räknar även processer som hann avslutas däremellan
//! (Kubernetes probes, korta exec). En processumma missar dem.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::Usage;

const CGROUP_ROOT: &str = "/sys/fs/cgroup";
const CLI_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Runtime {
    Docker,
    Podman,
    Containerd,
    Crio,
}

impl Runtime {
    fn label(self) -> &'static str {
        match self {
            Runtime::Docker => "docker",
            Runtime::Podman => "podman",
            Runtime::Containerd => "containerd",
            Runtime::Crio => "cri-o",
        }
    }
}

/// Det en process tillhör.
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum Unit {
    /// Namn och etiketter slås upp via runtimens CLI när det går.
    Container { runtime: Runtime, id: String },
    /// LXC-container eller systemd-maskin (nspawn, libvirt), som bär sitt namn.
    Named { kind: &'static str, name: String },
    /// Kubernetes direkt på värden: alla pods plus kontrollplanets tjänst.
    Kube,
    /// qemu utan egen cgroup, med namnet ur `-name`.
    Vm { launcher: &'static str, name: String },
}

pub struct Membership {
    pub unit: Unit,
    /// Enhetens cgroup, relativ CGROUP_ROOT.
    pub dir: String,
    pod: Option<String>,
    /// Kubernetes-distribution, när processen hör till kontrollplanets tjänst.
    distro: Option<&'static str>,
    /// `dir` hör bara till enheten. Annars räknas processerna, inte cgroupen.
    own_cgroup: bool,
}

/// En qemu-process som egen enhet. Vem som startat den syns på cgroupen,
/// men den cgroupen delas med startarens egna processer.
pub fn vm(cgroup: Option<&str>, name: String) -> Membership {
    let comps: Vec<&str> = cgroup.unwrap_or("").split('/').collect();
    let launcher = if comps.contains(&"incus.service") {
        "incus"
    } else if comps.iter().any(|c| *c == "lxd.service" || c.starts_with("snap.lxd.")) {
        "lxd"
    } else {
        "qemu"
    };
    Membership { unit: Unit::Vm { launcher, name }, dir: String::new(), pod: None, distro: None, own_cgroup: false }
}

/// VM-namnet ur qemus argv: `-name vm1`, `-name vm1,process=…` eller
/// libvirts `-name guest=vm1,debug-threads=on`. None om det inte är qemu
/// eller saknar namn.
pub fn qemu_name(argv: &[String]) -> Option<String> {
    let exe = argv.first()?.rsplit('/').next()?;
    if !(exe.starts_with("qemu-system-") || exe == "qemu-kvm" || exe == "kvm") {
        return None;
    }
    let i = argv.iter().position(|a| a == "-name" || a == "--name")?;
    let opts = argv.get(i + 1)?;
    let first = opts.split(',').next().filter(|p| !p.contains('='));
    opts.split(',')
        .find_map(|p| p.strip_prefix("guest="))
        .or(first)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
}

/// libvirts maskinnamn "qemu-<nr>-<gäst>" -> gäst.
fn libvirt_guest(machine: &str) -> Option<&str> {
    let (nr, guest) = machine.strip_prefix("qemu-")?.split_once('-')?;
    (!nr.is_empty() && nr.bytes().all(|b| b.is_ascii_digit())).then_some(guest)
}

/// Den cgroup-sökväg som räknas: v2-raden ("0::/…"), annars v1:s
/// memory-hierarki, annars första raden.
pub fn cgroup_path(pid: u32) -> Option<String> {
    let s = fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let lines: Vec<(&str, &str)> = s
        .lines()
        .filter_map(|l| {
            let mut f = l.splitn(3, ':');
            f.next()?;
            Some((f.next()?, f.next()?))
        })
        .collect();
    lines
        .iter()
        .find(|(ctl, _)| ctl.is_empty())
        .or_else(|| lines.iter().find(|(ctl, _)| ctl.split(',').any(|c| c == "memory")))
        .or(lines.first())
        .map(|(_, path)| path.to_string())
}

/// Yttersta container/maskin/kluster i en cgroup-sökväg.
///
/// | Runtime               | Mönster                                          |
/// |-----------------------|--------------------------------------------------|
/// | Docker                | docker-<id>.scope, docker/<id> (cgroupfs)        |
/// | Podman                | libpod-<id>.scope, libpod-conmon-<id>.scope      |
/// | containerd, nerdctl   | cri-containerd-<id>.scope, nerdctl-<id>.scope    |
/// | CRI-O                 | crio-<id>.scope, crio-conmon-<id>.scope          |
/// | Kubernetes            | kubepods.slice, kubepods (cgroupfs)              |
/// | k3s, rke2, k0s, …     | kontrollplanets systemd-tjänst                   |
/// | LXC, LXD              | lxc.payload.<namn>, lxc.monitor.<namn>, lxc/<namn> |
/// | nspawn, libvirt       | machine-<namn>.scope                             |
pub fn classify(path: &str) -> Option<Membership> {
    let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    for (i, &c) in comps.iter().enumerate() {
        let next = comps.get(i + 1).copied();
        let found = if c == "kubepods.slice" || c == "kubepods" {
            Some((Unit::Kube, i, None))
        } else if let Some(distro) = control_plane(c) {
            Some((Unit::Kube, i, Some(distro)))
        } else if let Some((runtime, id)) = scoped_container(c) {
            Some((Unit::Container { runtime, id }, i, None))
        } else if let (Some(runtime), Some(id)) = (cgroupfs_parent(c), next.filter(|n| is_id(n))) {
            Some((Unit::Container { runtime, id: id.to_string() }, i + 1, None))
        } else if let Some(name) = c.strip_prefix("lxc.payload.").or_else(|| c.strip_prefix("lxc.monitor.")) {
            Some((Unit::Named { kind: "lxc", name: name.to_string() }, i, None))
        } else if let (true, Some(name)) = (c == "lxc", next) {
            Some((Unit::Named { kind: "lxc", name: name.to_string() }, i + 1, None))
        } else if let Some(name) = c.strip_prefix("machine-").and_then(|n| n.strip_suffix(".scope")) {
            Some((Unit::Named { kind: "machine", name: unescape(name) }, i, None))
        } else {
            None
        };
        if let Some((unit, end, distro)) = found {
            return Some(Membership {
                unit,
                dir: comps[..=end].join("/"),
                pod: comps[end + 1..].iter().find_map(|c| pod_uid(c)),
                distro,
                own_cgroup: true,
            });
        }
    }
    None
}

fn scoped_container(c: &str) -> Option<(Runtime, String)> {
    let c = c.strip_suffix(".scope").unwrap_or(c);
    const PREFIXES: &[(&str, Runtime)] = &[
        ("docker-", Runtime::Docker),
        ("libpod-conmon-", Runtime::Podman),
        ("libpod-", Runtime::Podman),
        ("cri-containerd-", Runtime::Containerd),
        ("nerdctl-", Runtime::Containerd),
        ("crio-conmon-", Runtime::Crio),
        ("crio-", Runtime::Crio),
    ];
    PREFIXES
        .iter()
        .find_map(|(p, rt)| c.strip_prefix(p).filter(|id| is_id(id)).map(|id| (*rt, id.to_string())))
}

/// cgroupfs-drivern lägger containern som en katalog med bara id:t.
fn cgroupfs_parent(c: &str) -> Option<Runtime> {
    match c {
        "docker" => Some(Runtime::Docker),
        "libpod_parent" => Some(Runtime::Podman),
        _ => None,
    }
}

fn control_plane(c: &str) -> Option<&'static str> {
    match c.strip_suffix(".service")? {
        "k3s" | "k3s-agent" => Some("k3s"),
        "rke2-server" | "rke2-agent" => Some("rke2"),
        "k0scontroller" | "k0sworker" => Some("k0s"),
        "kubelet" => Some("kubernetes"),
        s if s.starts_with("snap.microk8s.") => Some("microk8s"),
        _ => None,
    }
}

/// "kubepods-besteffort-pod<uid>.slice" eller "pod<uid>" -> uid.
fn pod_uid(c: &str) -> Option<String> {
    let c = c.strip_suffix(".slice").unwrap_or(c);
    let uid = &c[c.rfind("pod")? + 3..];
    (uid.len() >= 32 && uid.chars().all(|ch| ch.is_ascii_hexdigit() || ch == '-' || ch == '_'))
        .then(|| uid.replace('_', "-"))
}

/// Container-id: 64 hex-tecken.
fn is_id(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// systemd escapar tecken i enhetsnamn som \xNN; "qemu\x2d1\x2dvm" -> "qemu-1-vm".
fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("\\x") {
        out.push_str(&rest[..i]);
        match rest.get(i + 2..i + 4).and_then(|h| u8::from_str_radix(h, 16).ok()) {
            Some(b) => {
                out.push(b as char);
                rest = &rest[i + 4..];
            }
            None => {
                out.push_str("\\x");
                rest = &rest[i + 2..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Processerna i en enhet, medan de samlas in.
#[derive(Default)]
struct UnitAcc {
    dirs: HashSet<String>,
    pods: HashSet<String>,
    procs: u32,
    /// Processernas summa, när cgroupen inte går att läsa eller inte gäller.
    fallback: Usage,
    rss_fallback: bool,
    /// Någon av enhetens processer räknades till en tmux-session, t.ex. när
    /// tmux själv kör i en container (distrobox, toolbox). Då skulle
    /// cgroupens siffror räkna de processerna två gånger.
    shared_with_tmux: bool,
    /// Enheten har ingen egen cgroup att läsa (se `vm`).
    no_cgroup: bool,
    distro: Option<&'static str>,
}

#[derive(Default)]
pub struct Collector {
    units: HashMap<Unit, UnitAcc>,
}

pub struct Group {
    pub name: String,
    /// Virtuell maskin, inte container eller kluster.
    pub vm: bool,
    pub procs: u32,
    pub usage: Usage,
}

impl Collector {
    pub fn add(&mut self, m: Membership, usage: &Usage, pss: bool) {
        let acc = self.units.entry(m.unit).or_default();
        if m.own_cgroup {
            acc.dirs.insert(m.dir);
        } else {
            acc.no_cgroup = true;
        }
        acc.pods.extend(m.pod);
        acc.procs += 1;
        acc.fallback.add(usage);
        acc.rss_fallback |= !pss;
        acc.distro = acc.distro.or(m.distro);
    }

    /// En process utan förbrukning: räknas inte, men dess pod gör det.
    pub fn seen(&mut self, m: Membership) {
        let acc = self.units.entry(m.unit).or_default();
        acc.dirs.insert(m.dir);
        acc.pods.extend(m.pod);
        acc.distro = acc.distro.or(m.distro);
    }

    pub fn mark_in_tmux(&mut self, m: Membership) {
        self.units.entry(m.unit).or_default().shared_with_tmux = true;
    }

    /// En rad per container, men containrar i samma kluster eller
    /// compose-projekt slås ihop. `cgroup_cpu` är CPU per cgroup-katalog
    /// under mätfönstret. Andra värdet: någon rad bygger på RSS.
    pub fn groups(self, cgroup_cpu: &HashMap<String, u64>) -> (Vec<Group>, bool) {
        let runtimes: HashSet<Runtime> = self
            .units
            .keys()
            .filter_map(|u| match u {
                Unit::Container { runtime, .. } => Some(*runtime),
                _ => None,
            })
            .collect();
        let info = container_info(&runtimes);
        let cgroup_v2 = Path::new(CGROUP_ROOT).join("cgroup.controllers").exists();

        struct Merged {
            procs: u32,
            usage: Usage,
            pods: HashSet<String>,
            containers: u32,
            vm: bool,
        }
        let mut merged: HashMap<String, Merged> = HashMap::new();
        let mut any_rss = false;
        for (unit, acc) in self.units {
            let exact = cgroup_v2 && !acc.shared_with_tmux && !acc.no_cgroup;
            let mut usage = match exact.then(|| cgroup_mem(&acc.dirs)).flatten() {
                Some((ram, swap)) => Usage { ram, swap, ..Default::default() },
                None => {
                    any_rss |= acc.rss_fallback;
                    Usage { ram: acc.fallback.ram, swap: acc.fallback.swap, ..Default::default() }
                }
            };
            // Alla kataloger måste ha mätts, annars blir summan för låg.
            usage.cpu = exact
                .then(|| acc.dirs.iter().map(|d| cgroup_cpu.get(d)).sum::<Option<u64>>())
                .flatten()
                .unwrap_or(acc.fallback.cpu);
            let guest = match &unit {
                Unit::Named { kind: "machine", name } => libvirt_guest(name),
                _ => None,
            };
            let name = match (&unit, guest) {
                (_, Some(g)) => format!("libvirt {g}"),
                (Unit::Vm { launcher, name }, _) => format!("{launcher} {name}"),
                (Unit::Kube, _) => acc.distro.unwrap_or("kubernetes").to_string(),
                (Unit::Named { kind, name }, _) => format!("{kind} {name}"),
                (Unit::Container { runtime, id }, _) => match info.get(id) {
                    Some(i) => i.group.clone().unwrap_or_else(|| i.name.clone()),
                    None => format!("{} {}", runtime.label(), &id[..12]),
                },
            };
            let m = merged.entry(name).or_insert_with(|| Merged {
                procs: 0,
                usage: Usage::default(),
                pods: HashSet::new(),
                containers: 0,
                vm: guest.is_some() || matches!(unit, Unit::Vm { .. }),
            });
            m.procs += acc.procs;
            m.usage.add(&usage);
            m.pods.extend(acc.pods);
            m.containers += 1;
        }
        let groups = merged
            .into_iter()
            .filter(|(_, m)| m.procs > 0) // bara sovande, eller bara i tmux
            .map(|(name, m)| {
                let name = if !m.pods.is_empty() {
                    let n = m.pods.len();
                    format!("{name} · {n} {}", if n == 1 { "pod" } else { "pods" })
                } else if m.containers > 1 {
                    format!("{name} · {} containrar", m.containers)
                } else {
                    name
                };
                Group { name, vm: m.vm, procs: m.procs, usage: m.usage }
            })
            .collect();
        (groups, any_rss)
    }
}

/// RAM = anon + fil-mappat, som motsvarar PSS: det processerna har i RAM,
/// men inte page cache. Summan över flera kataloger förutsätter att ingen
/// ligger under en annan, vilket classify garanterar (yttersta träffen).
fn cgroup_mem(dirs: &HashSet<String>) -> Option<(u64, u64)> {
    let (mut ram, mut swap) = (0, 0);
    for d in dirs {
        let base = Path::new(CGROUP_ROOT).join(d);
        let stat = fs::read_to_string(base.join("memory.stat")).ok()?;
        let field = |key: &str| {
            stat.lines()
                .find_map(|l| l.strip_prefix(key)?.strip_prefix(' '))
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(0)
        };
        ram += field("anon") + field("file_mapped");
        // Saknas när swap-accounting är avslaget; då finns inget att räkna.
        swap += fs::read_to_string(base.join("memory.swap.current"))
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0);
    }
    Some((ram, swap))
}

/// Total CPU-tid för en cgroup och allt under den, i mikrosekunder (v2).
pub fn cpu_usec(dir: &str) -> Option<u64> {
    let stat = fs::read_to_string(Path::new(CGROUP_ROOT).join(dir).join("cpu.stat")).ok()?;
    stat.lines().find_map(|l| l.strip_prefix("usage_usec ")?.trim().parse().ok())
}

struct Info {
    name: String,
    /// "k3d mitt-kluster", "compose webbapp" — containrar som hör ihop.
    group: Option<String>,
}

/// Etiketter som knyter ihop containrar, i prioritetsordning.
const GROUP_LABELS: &[(&str, &str)] = &[
    ("k3d", "k3d.cluster"),
    ("kind", "io.x-k8s.kind.cluster"),
    ("minikube", "name.minikube.sigs.k8s.io"),
    ("compose", "com.docker.compose.project"),
];

/// Namn och etiketter från `docker ps` / `podman ps`. Tyst tomt om CLI:t
/// saknas, inte får prata med daemonen eller hänger.
fn container_info(runtimes: &HashSet<Runtime>) -> HashMap<String, Info> {
    let mut out = HashMap::new();
    for (rt, cli) in [(Runtime::Docker, "docker"), (Runtime::Podman, "podman")] {
        if !runtimes.contains(&rt) {
            continue;
        }
        // docker har {{.Label "k"}}; podmans Labels är en map.
        let label = |k: &str| match rt {
            Runtime::Podman => format!("{{{{index .Labels \"{k}\"}}}}"),
            _ => format!("{{{{.Label \"{k}\"}}}}"),
        };
        let mut fields = vec!["{{.ID}}".to_string(), "{{.Names}}".to_string()];
        fields.extend(GROUP_LABELS.iter().map(|(_, k)| label(k)));
        let format = fields.join("\t");
        let Some(stdout) = run(cli, &["ps", "--no-trunc", "--format", &format]) else { continue };
        for line in stdout.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            let [id, name, labels @ ..] = f.as_slice() else { continue };
            let group = GROUP_LABELS.iter().zip(labels).find_map(|((kind, _), v)| {
                let v = v.trim();
                (!v.is_empty() && v != "<no value>").then(|| format!("{kind} {v}"))
            });
            out.insert(id.to_string(), Info { name: name.to_string(), group });
        }
    }
    out
}

/// Kör ett kommando med tidsgräns; None vid fel, felkod eller timeout.
fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Läs i en egen tråd så att en full pipe inte låser barnet medan vi väntar.
    let mut stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut s = String::new();
        stdout.read_to_string(&mut s).ok().map(|_| s)
    });
    let deadline = Instant::now() + CLI_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return reader.join().ok().flatten(),
            Ok(Some(_)) => return None,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "6ff8daebd14cc078babfd7ba196202f470887fc49edae04f409e29ee7a840785";

    fn unit(path: &str) -> Option<Unit> {
        classify(path).map(|m| m.unit)
    }

    #[test]
    fn docker_systemd_and_cgroupfs() {
        let docker = Some(Unit::Container { runtime: Runtime::Docker, id: ID.into() });
        assert!(unit(&format!("/system.slice/docker-{ID}.scope")) == docker);
        assert!(unit(&format!("/docker/{ID}")) == docker);
        // Rootless docker ligger under användarens systemd.
        let rootless = format!("/user.slice/user-1000.slice/user@1000.service/docker.service/docker-{ID}.scope");
        assert!(unit(&rootless) == docker);
        // docker.service själv (dockerd) är ingen container.
        assert!(unit("/system.slice/docker.service").is_none());
    }

    #[test]
    fn podman_container_and_conmon_are_one_unit() {
        let podman = Some(Unit::Container { runtime: Runtime::Podman, id: ID.into() });
        assert!(unit(&format!("/machine.slice/libpod-{ID}.scope/container")) == podman);
        assert!(unit(&format!("/machine.slice/libpod-conmon-{ID}.scope")) == podman);
    }

    #[test]
    fn cluster_in_docker_counts_as_the_container_with_pods() {
        let uid = "da5f8ec1-af33-4c49-bb87-b218e46f89b4";
        let m = classify(&format!("/system.slice/docker-{ID}.scope/kubepods/besteffort/pod{uid}/{ID}")).unwrap();
        assert!(m.unit == Unit::Container { runtime: Runtime::Docker, id: ID.into() });
        assert_eq!(m.dir, format!("system.slice/docker-{ID}.scope"));
        assert_eq!(m.pod.as_deref(), Some(uid));
    }

    #[test]
    fn host_kubernetes_pods_and_control_plane() {
        let pod = "kubepods.slice/kubepods-burstable.slice/kubepods-burstable-podb47535e5_06d1_488a_8805_eb7549deb89a.slice";
        let m = classify(&format!("/{pod}/cri-containerd-{ID}.scope")).unwrap();
        assert!(m.unit == Unit::Kube);
        assert_eq!(m.dir, "kubepods.slice");
        assert_eq!(m.pod.as_deref(), Some("b47535e5-06d1-488a-8805-eb7549deb89a"));
        // QoS-klassens slice är ingen pod.
        assert!(classify("/kubepods.slice/kubepods-besteffort.slice").unwrap().pod.is_none());

        let cp = classify("/system.slice/k3s.service").unwrap();
        assert!(cp.unit == Unit::Kube && cp.distro == Some("k3s"));
        assert!(classify("/system.slice/rke2-agent.service").unwrap().distro == Some("rke2"));
    }

    #[test]
    fn lxc_and_machines() {
        let lxc = Some(Unit::Named { kind: "lxc", name: "web".into() });
        assert!(unit("/lxc.payload.web/init.scope") == lxc);
        assert!(unit("/lxc.monitor.web") == lxc);
        assert!(unit("/lxc/web") == lxc);
        let vm = Some(Unit::Named { kind: "machine", name: "qemu-1-ubuntu".into() });
        assert!(unit("/machine.slice/machine-qemu\\x2d1\\x2dubuntu.scope/libvirt") == vm);
    }

    #[test]
    fn qemu_names() {
        let argv = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        let name = |s: &str| qemu_name(&argv(s));
        assert_eq!(name("/usr/bin/qemu-system-x86_64 -S -name gbandit-vm -uuid 312c").as_deref(), Some("gbandit-vm"));
        assert_eq!(name("qemu-system-aarch64 -name vm1,process=qemu-vm1").as_deref(), Some("vm1"));
        assert_eq!(name("/usr/bin/qemu-kvm -name guest=ubuntu,debug-threads=on").as_deref(), Some("ubuntu"));
        assert!(name("qemu-system-x86_64 -m 4G").is_none());
        assert!(name("nvim -name foo").is_none());
    }

    #[test]
    fn vm_launcher_from_cgroup() {
        let launcher = |cg| match vm(cg, "x".into()).unit {
            Unit::Vm { launcher, .. } => launcher,
            _ => unreachable!(),
        };
        assert_eq!(launcher(Some("/system.slice/incus.service")), "incus");
        assert_eq!(launcher(Some("/system.slice/snap.lxd.daemon.service")), "lxd");
        assert_eq!(launcher(Some("/user.slice/user-1000.slice/session-2.scope")), "qemu");
        assert_eq!(libvirt_guest("qemu-1-ubuntu"), Some("ubuntu"));
        assert_eq!(libvirt_guest("qemu-12-my-vm"), Some("my-vm"));
        assert!(libvirt_guest("debian-nspawn").is_none());
    }

    #[test]
    fn ordinary_processes_are_not_containers() {
        assert!(unit("/user.slice/user-1000.slice/user@1000.service/app.slice/app-tmux.scope").is_none());
        assert!(unit("/").is_none());
        assert!(unit("/init.scope").is_none());
    }
}
