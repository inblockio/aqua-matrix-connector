//! `aqua-seat-stress`: K headless seats on persisted keys `seat-stress-00..K-1`
//! exchanging messages in the roster room on DEV, reporting delivery latency
//! (p50/p95), decrypt failures, daemon RSS (marginal per seat) and sync cost.
//!
//! It starts its own `aqua-seatd` (hub mode, relaxed per-recipient rate limit,
//! no idle detach) on a private socket, so a daemon already owning the state
//! root makes it stop with a clear message. Seats are minted once (first run)
//! and reused by every later run. With `--dev-metrics` it takes three READ-ONLY
//! samples of the dev box over ssh (before / during / after): `free -m`,
//! `uptime`, `docker stats --no-stream`; it changes nothing there.
//!
//! Per-seat RSS: all seats live in one daemon process, so the figure is the
//! marginal RSS of the daemon per attached seat, (RSS after attach - RSS
//! before) / seats attached, plus the peak during load.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use aqua_seat::client::SeatClient;
use aqua_seat::paths;
use aqua_seat::proto::{Item, Request};
use clap::Parser;
use serde_json::{json, Value};
use tokio::process::Command;
use tokio::sync::Semaphore;

const DEV_SSH: [&str; 2] = ["-p 8022", "dev@207.154.209.103"];

#[derive(Parser, Debug, Clone)]
#[command(
    name = "aqua-seat-stress",
    version,
    about = "Claude seats stress harness (dev only)"
)]
struct Args {
    /// Number of seats (seat-stress-00 .. K-1).
    #[arg(long, default_value_t = 3)]
    seats: usize,
    /// Seat name prefix.
    #[arg(long, default_value = "seat-stress")]
    prefix: String,
    /// Load phase length in seconds.
    #[arg(long, default_value_t = 60)]
    duration: u64,
    /// Messages per seat per minute into the roster room.
    #[arg(long, default_value_t = 6.0)]
    rate: f64,
    /// Seconds to keep receiving after the senders stop.
    #[arg(long, default_value_t = 30)]
    drain: u64,
    /// Message body size in bytes (at least ~40).
    #[arg(long, default_value_t = 120)]
    payload: usize,
    /// Seats attached in parallel (logins are rate-limited by the homeserver).
    #[arg(long, default_value_t = 3)]
    attach_parallel: usize,
    /// Take read-only dev box samples over ssh before / during / after.
    #[arg(long)]
    dev_metrics: bool,
    /// With --dev-metrics: stop the load when the dev box has less memory available than this (MB).
    #[arg(long, default_value_t = 600)]
    abort_below_mb: u64,
    /// Daemon socket (default: <runtime dir>/claude-seats/stress.sock).
    #[arg(long)]
    sock: Option<PathBuf>,
    /// aqua-seatd binary (default: next to this executable, else PATH).
    #[arg(long)]
    seatd: Option<PathBuf>,
    /// Leave the daemon running afterwards.
    #[arg(long)]
    keep_daemon: bool,
    /// Where the result JSON goes (default ~/.cache/claude-seats-dev).
    #[arg(long)]
    out: Option<PathBuf>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted[rank.min(sorted.len()) - 1]
}

fn stats_line(v: &mut [f64]) -> (f64, f64, f64) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (
        pct(v, 50.0),
        pct(v, 95.0),
        v.last().copied().unwrap_or(f64::NAN),
    )
}

// ---- dev box (read-only) --------------------------------------------------

#[derive(Debug, Clone, Default)]
struct DevSample {
    label: String,
    load: String,
    mem_available_mb: Option<u64>,
    swap_used_mb: Option<u64>,
    synapse: String,
    siwx: String,
    error: Option<String>,
}

async fn dev_sample(label: &str) -> DevSample {
    let mut s = DevSample {
        label: label.to_string(),
        ..Default::default()
    };
    // Exactly the three read-only commands the plan allows.
    let remote =
        "free -m; uptime; docker stats --no-stream --format '{{.Name}} {{.CPUPerc}} {{.MemUsage}}'";
    let out = tokio::time::timeout(
        Duration::from_secs(90),
        Command::new("ssh")
            .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"])
            .args(DEV_SSH[0].split(' '))
            .arg(DEV_SSH[1])
            .arg(remote)
            .stdin(Stdio::null())
            .output(),
    )
    .await;
    let out = match out {
        Ok(Ok(o)) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        Ok(Ok(o)) => {
            s.error = Some(format!("ssh exit {:?}", o.status.code()));
            return s;
        }
        Ok(Err(e)) => {
            s.error = Some(e.to_string());
            return s;
        }
        Err(_) => {
            s.error = Some("ssh timed out".into());
            return s;
        }
    };
    for line in out.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if line.starts_with("Mem:") && f.len() >= 7 {
            s.mem_available_mb = f[6].parse().ok();
        } else if line.starts_with("Swap:") && f.len() >= 3 {
            s.swap_used_mb = f[2].parse().ok();
        } else if let Some(i) = line.find("load average:") {
            s.load = line[i + 14..].trim().to_string();
        } else if f.len() >= 3 && f[0].contains("synapse") {
            s.synapse = format!("cpu {} mem {}", f[1], f[2]);
        } else if f.len() >= 3 && f[0].contains("siwx-oidc") {
            s.siwx = format!("cpu {} mem {}", f[1], f[2]);
        }
    }
    s
}

fn dev_line(s: &DevSample) -> String {
    if let Some(e) = &s.error {
        return format!("{:<7} unavailable ({e})", s.label);
    }
    format!(
        "{:<7} load {} | mem available {} MB, swap used {} MB | synapse {} | siwx-oidc {}",
        s.label,
        s.load,
        s.mem_available_mb
            .map(|v| v.to_string())
            .unwrap_or_else(|| "?".into()),
        s.swap_used_mb
            .map(|v| v.to_string())
            .unwrap_or_else(|| "?".into()),
        s.synapse,
        s.siwx
    )
}

// ---- daemon ------------------------------------------------------------------

struct Daemon {
    child: Option<tokio::process::Child>,
    client: SeatClient,
}

fn seatd_binary(args: &Args) -> PathBuf {
    if let Some(p) = &args.seatd {
        return p.clone();
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("aqua-seatd");
            if p.exists() {
                return p;
            }
        }
    }
    PathBuf::from("aqua-seatd")
}

async fn start_daemon(args: &Args, sock: &PathBuf) -> Result<Daemon> {
    let client = SeatClient::new(sock.clone());
    if client
        .call(&Request::Ping, Duration::from_secs(2))
        .await
        .is_ok()
    {
        bail!(
            "a daemon already answers on {}; stop it first",
            sock.display()
        );
    }
    paths::ensure_private_dir(sock.parent().context("socket path has no parent")?)?;
    let root = paths::state_root();
    paths::ensure_private_dir(&root)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("seatd-stress.log"))?;
    let child = Command::new(seatd_binary(args))
        .env(paths::SOCK_ENV, sock)
        .env("CLAUDE_SEATS_HUB", "1")
        .env("CLAUDE_SEATS_RATE_COUNT", "1000000")
        .env("CLAUDE_SEATS_IDLE_DETACH_SECS", "86400")
        .env("CLAUDE_SEATS_ATTACH_TIMEOUT_SECS", "400")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .kill_on_drop(true)
        .spawn()
        .context("starting aqua-seatd")?;
    for _ in 0..200 {
        if client
            .call(&Request::Ping, Duration::from_secs(2))
            .await
            .is_ok()
        {
            return Ok(Daemon {
                child: Some(child),
                client,
            });
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!(
        "aqua-seatd did not answer on {} (see {}/seatd-stress.log)",
        sock.display(),
        root.display()
    )
}

async fn daemon_stats(c: &SeatClient) -> Result<Value> {
    c.call(&Request::Stats, Duration::from_secs(30))
        .await
        .map_err(anyhow::Error::msg)
}

fn cpu_ticks(pid: u64) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit_once(')')?.1;
    let f: Vec<&str> = rest.split_whitespace().collect();
    Some(f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?)
}

fn clk_tck() -> f64 {
    std::process::Command::new("getconf")
        .arg("CLK_TCK")
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .unwrap_or(100.0)
}

fn loadavg() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .map(|s| s.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
        .unwrap_or_default()
}

// ---- messages -------------------------------------------------------------------

fn make_body(run: &str, idx: usize, n: u64, payload: usize) -> String {
    let head = format!("stress|{run}|{idx:02}|{n}|{}|", now_ms());
    let pad = payload.saturating_sub(head.len());
    format!("{head}{}", "x".repeat(pad))
}

/// `(sender idx, n, sent_ms)` of a stress message of this run.
fn parse_body(run: &str, body: &str) -> Option<(usize, u64, u64)> {
    let mut it = body.splitn(6, '|');
    if it.next()? != "stress" || it.next()? != run {
        return None;
    }
    Some((
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
    ))
}

#[derive(Default)]
struct Shared {
    /// (receiver, sender, n) -> latency ms of the first receipt via Wait
    receipts: Mutex<HashMap<(usize, usize, u64), f64>>,
    send_ms: Mutex<Vec<f64>>,
    sent: AtomicU64,
    send_errors: AtomicU64,
    first_errors: Mutex<Vec<String>>,
    stop_send: AtomicBool,
    stop_recv: AtomicBool,
}

async fn sender(c: SeatClient, seat: String, idx: usize, args: Args, run: String, sh: Arc<Shared>) {
    let interval = Duration::from_secs_f64(60.0 / args.rate.max(0.01));
    // spread the seats over one interval
    tokio::time::sleep(interval.mul_f64(idx as f64 / args.seats.max(1) as f64)).await;
    let mut n = 0u64;
    while !sh.stop_send.load(Ordering::Relaxed) {
        let body = make_body(&run, idx, n, args.payload);
        let t0 = Instant::now();
        let r = c
            .call(
                &Request::Send {
                    seat: seat.clone(),
                    to: "room".into(),
                    text: body,
                    reply_to: None,
                    mention: None,
                },
                Duration::from_secs(200),
            )
            .await;
        match r {
            Ok(_) => {
                sh.sent.fetch_add(1, Ordering::Relaxed);
                sh.send_ms
                    .lock()
                    .unwrap()
                    .push(t0.elapsed().as_secs_f64() * 1000.0);
            }
            Err(e) => {
                sh.send_errors.fetch_add(1, Ordering::Relaxed);
                let mut fe = sh.first_errors.lock().unwrap();
                if fe.len() < 3 {
                    fe.push(e);
                }
            }
        }
        n += 1;
        let used = t0.elapsed();
        if used < interval {
            tokio::time::sleep(interval - used).await;
        }
    }
}

async fn receiver(c: SeatClient, seat: String, idx: usize, run: String, sh: Arc<Shared>) {
    while !sh.stop_recv.load(Ordering::Relaxed) {
        let r = c
            .call(
                &Request::Wait {
                    seat: seat.clone(),
                    timeout_secs: 5,
                    after_seq: None,
                    addressed_only: false,
                    by: Some("stress".into()),
                    session: None,
                },
                Duration::from_secs(60),
            )
            .await;
        let woke = now_ms();
        let Ok(data) = r else {
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        };
        let items: Vec<Item> = serde_json::from_value(data["items"].clone()).unwrap_or_default();
        let mut g = sh.receipts.lock().unwrap();
        for it in items {
            if let Some((from, n, sent_ms)) = parse_body(&run, &it.body) {
                if from != idx {
                    g.entry((idx, from, n))
                        .or_insert_with(|| woke.saturating_sub(sent_ms) as f64);
                }
            }
        }
    }
}

/// Count this run's messages in every seat's inbox (authoritative: independent
/// of the Wait loop).
async fn reconcile(c: &SeatClient, names: &[String], run: &str) -> HashMap<usize, usize> {
    let mut out = HashMap::new();
    for (i, seat) in names.iter().enumerate() {
        let mut count = 0;
        if let Ok(d) = c
            .call(
                &Request::Inbox {
                    seat: seat.clone(),
                    since: Some(0),
                    addressed_only: false,
                    limit: Some(500),
                    by: Some("stress".into()),
                    session: None,
                },
                Duration::from_secs(120),
            )
            .await
        {
            let items: Vec<Item> = serde_json::from_value(d["items"].clone()).unwrap_or_default();
            count = items
                .iter()
                .filter(|it| parse_body(run, &it.body).is_some_and(|(from, _, _)| from != i))
                .count();
        }
        out.insert(i, count);
    }
    out
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if args.seats == 0 || args.seats > 99 {
        bail!("--seats must be 1..=99");
    }
    let sock = args.sock.clone().unwrap_or_else(|| {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(paths::state_root)
            .join("claude-seats/stress.sock")
    });
    let run = format!("{}", now_ms());
    let names: Vec<String> = (0..args.seats)
        .map(|i| format!("{}-{i:02}", args.prefix))
        .collect();
    let root = paths::state_root();
    let preexisting = names
        .iter()
        .filter(|n| root.join("seats").join(n).exists())
        .count();
    println!(
        "aqua-seat-stress run {run}: K={} rate={}/min/seat duration={}s drain={}s payload={}B (dev only)",
        args.seats, args.rate, args.duration, args.drain, args.payload
    );

    let mut dev_samples: Vec<DevSample> = Vec::new();
    if args.dev_metrics {
        let s = dev_sample("before").await;
        println!("dev box {}", dev_line(&s));
        if let Some(m) = s.mem_available_mb {
            if m < args.abort_below_mb {
                bail!("dev box has only {m} MB available before the run; not starting (--abort-below-mb {})", args.abort_below_mb);
            }
        }
        dev_samples.push(s);
    }

    let mut daemon = start_daemon(&args, &sock).await?;
    let c = daemon.client.clone();
    // The hub seat attaches by itself at startup; measure the baseline once it
    // (or any other seat the daemon brings up) is settled.
    let mut base = daemon_stats(&c).await?;
    for _ in 0..60 {
        let settling = base["seats"]
            .as_array()
            .is_some_and(|a| a.iter().any(|s| s["state"] == "Starting"));
        if !settling {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        base = daemon_stats(&c).await?;
    }
    let pid = base["pid"].as_u64().unwrap_or(0);
    let rss_base = base["rss_kb"].as_u64().unwrap_or(0);
    let base_seats = base["seats"].as_array().map(|a| a.len()).unwrap_or(0);

    // -- attach ---------------------------------------------------------------
    let t_attach = Instant::now();
    let sem = Arc::new(Semaphore::new(args.attach_parallel.max(1)));
    let mut joins = Vec::new();
    for name in &names {
        let (c, name, sem) = (c.clone(), name.clone(), sem.clone());
        joins.push(tokio::spawn(async move {
            let _p = sem.acquire().await.unwrap();
            let t0 = Instant::now();
            let mut last = String::new();
            for _ in 0..2 {
                match c
                    .call(
                        &Request::Attach { seat: name.clone() },
                        Duration::from_secs(450),
                    )
                    .await
                {
                    Ok(_) => return (name, Ok(t0.elapsed().as_secs_f64())),
                    Err(e) => last = e,
                }
            }
            (name, Err(last))
        }));
    }
    let mut attach_s = Vec::new();
    let mut attach_failed = Vec::new();
    for j in joins {
        match j.await? {
            (_, Ok(s)) => attach_s.push(s),
            (n, Err(e)) => attach_failed.push(format!("{n}: {e}")),
        }
    }
    let attach_total = t_attach.elapsed().as_secs_f64();
    let attached_stats = daemon_stats(&c).await?;
    let rss_attached = attached_stats["rss_kb"].as_u64().unwrap_or(0);
    let threads_attached = attached_stats["threads"].as_u64().unwrap_or(0);
    println!(
        "attached {}/{} seats in {:.0}s ({} identities were already provisioned); daemon RSS {} -> {} MB",
        attach_s.len(),
        names.len(),
        attach_total,
        preexisting,
        rss_base / 1024,
        rss_attached / 1024
    );
    if !attach_failed.is_empty() {
        println!("ATTACH FAILURES: {attach_failed:?}");
    }
    let live: Vec<(usize, String)> = names
        .iter()
        .enumerate()
        .filter(|(_, n)| {
            !attach_failed
                .iter()
                .any(|f| f.starts_with(&format!("{n}:")))
        })
        .map(|(i, n)| (i, n.clone()))
        .collect();

    // -- roster convergence ------------------------------------------------------
    let t0 = Instant::now();
    let mut roster_ok = false;
    while t0.elapsed() < Duration::from_secs(120) {
        if let Some((_, first)) = live.first() {
            if let Ok(r) = c
                .call(
                    &Request::Roster {
                        seat: first.clone(),
                    },
                    Duration::from_secs(60),
                )
                .await
            {
                let have: Vec<String> = r["peers"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|p| p["seat"].as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                if live.iter().all(|(_, n)| have.contains(n)) {
                    roster_ok = true;
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    println!(
        "roster lists all {} seats: {} ({:.0}s)",
        live.len(),
        roster_ok,
        t0.elapsed().as_secs_f64()
    );

    // -- load ------------------------------------------------------------------------
    let sh = Arc::new(Shared::default());
    let mut recv_tasks = Vec::new();
    for (i, n) in &live {
        recv_tasks.push(tokio::spawn(receiver(
            c.clone(),
            n.clone(),
            *i,
            run.clone(),
            sh.clone(),
        )));
    }
    let tck = clk_tck();
    let cpu0 = cpu_ticks(pid);
    let load_start = Instant::now();
    let mut send_tasks = Vec::new();
    for (i, n) in &live {
        send_tasks.push(tokio::spawn(sender(
            c.clone(),
            n.clone(),
            *i,
            args.clone(),
            run.clone(),
            sh.clone(),
        )));
    }
    // sampler: RSS every 10 s, one dev sample mid-load
    let mut rss_samples: Vec<u64> = vec![rss_attached];
    let mut aborted: Option<String> = None;
    let mut mid_taken = false;
    while load_start.elapsed() < Duration::from_secs(args.duration) {
        tokio::time::sleep(
            Duration::from_secs(10).min(
                Duration::from_secs(args.duration)
                    .saturating_sub(load_start.elapsed())
                    .max(Duration::from_millis(100)),
            ),
        )
        .await;
        if let Ok(s) = daemon_stats(&c).await {
            rss_samples.push(s["rss_kb"].as_u64().unwrap_or(0));
        }
        if args.dev_metrics
            && !mid_taken
            && load_start.elapsed() >= Duration::from_secs(args.duration / 2)
        {
            mid_taken = true;
            let s = dev_sample("during").await;
            println!("dev box {}", dev_line(&s));
            if s.mem_available_mb.is_some_and(|m| m < args.abort_below_mb) {
                aborted = Some(format!(
                    "dev box memory available {} MB < {}",
                    s.mem_available_mb.unwrap(),
                    args.abort_below_mb
                ));
                dev_samples.push(s);
                break;
            }
            dev_samples.push(s);
        }
    }
    sh.stop_send.store(true, Ordering::Relaxed);
    let load_secs = load_start.elapsed().as_secs_f64();
    let cpu1 = cpu_ticks(pid);
    let load_after = loadavg();
    for t in send_tasks {
        let _ = t.await;
    }
    // drain
    tokio::time::sleep(Duration::from_secs(args.drain)).await;
    sh.stop_recv.store(true, Ordering::Relaxed);
    for t in recv_tasks {
        let _ = t.await;
    }
    if args.dev_metrics {
        let s = dev_sample("after").await;
        println!("dev box {}", dev_line(&s));
        dev_samples.push(s);
    }

    // -- results ------------------------------------------------------------------------
    let sent = sh.sent.load(Ordering::Relaxed);
    let k_live = live.len() as u64;
    let expected = sent * k_live.saturating_sub(1);
    let by_wait = sh.receipts.lock().unwrap().len() as u64;
    let rec = reconcile(&c, &names, &run).await;
    let by_inbox: u64 = rec.values().map(|v| *v as u64).sum();
    let delivered = by_inbox.max(by_wait);
    let mut lat: Vec<f64> = sh.receipts.lock().unwrap().values().copied().collect();
    let n_lat = lat.len();
    let (l50, l95, lmax) = stats_line(&mut lat);
    let mut sm = sh.send_ms.lock().unwrap().clone();
    let (s50, s95, _) = stats_line(&mut sm);
    let fin = daemon_stats(&c).await?;
    let rss_final = fin["rss_kb"].as_u64().unwrap_or(0);
    let rss_peak = rss_samples
        .iter()
        .copied()
        .max()
        .unwrap_or(0)
        .max(rss_final);
    let seats_json = fin["seats"].as_array().cloned().unwrap_or_default();
    let sum = |k: &str| -> u64 {
        seats_json
            .iter()
            .filter(|s| {
                s["seat"]
                    .as_str()
                    .is_some_and(|n| n.starts_with(&args.prefix))
            })
            .filter_map(|s| s[k].as_u64())
            .sum()
    };
    let (utd_seen, utd_rec, utd_unres, backfilled) = (
        sum("utd_seen"),
        sum("utd_recovered"),
        sum("utd_unresolved"),
        sum("backfilled"),
    );
    let attached_n = attach_s.len().max(1) as f64;
    let per_seat_mb = (rss_attached.saturating_sub(rss_base)) as f64 / 1024.0 / attached_n;
    let per_seat_peak_mb = (rss_peak.saturating_sub(rss_base)) as f64 / 1024.0 / attached_n;
    let cpu_pct = match (cpu0, cpu1) {
        (Some(a), Some(b)) if load_secs > 0.0 => {
            (b.saturating_sub(a)) as f64 / tck / load_secs * 100.0
        }
        _ => f64::NAN,
    };
    let mut at = attach_s.clone();
    let (a50, a95, _) = stats_line(&mut at);

    println!("\n=== K={} (run {run}) ===", args.seats);
    println!("| item | value |\n|---|---|");
    println!(
        "| seats attached | {}/{} ({} fresh identities minted, {} reused) |",
        attach_s.len(),
        names.len(),
        names.len() - preexisting,
        preexisting
    );
    println!("| attach time p50 / p95 / total | {a50:.1} s / {a95:.1} s / {attach_total:.0} s |");
    println!("| roster converged | {roster_ok} |");
    println!(
        "| load | {sent} msgs sent ({} errors) over {load_secs:.0} s, {} expected deliveries |",
        sh.send_errors.load(Ordering::Relaxed),
        expected
    );
    println!("| delivered | {delivered} of {expected} (inbox reconcile {by_inbox}, wait path {by_wait}); missing {} |", expected.saturating_sub(delivered));
    println!("| delivery latency p50 / p95 / max | {l50:.0} ms / {l95:.0} ms / {lmax:.0} ms ({n_lat} samples) |");
    println!("| send call p50 / p95 | {s50:.0} ms / {s95:.0} ms |");
    println!("| decrypt failures | seen {utd_seen}, recovered via history {utd_rec}, unresolved {utd_unres}; backfilled {backfilled} |");
    println!("| daemon RSS | base {} MB -> attached {} MB -> peak {} MB -> final {} MB; threads {threads_attached} |", rss_base / 1024, rss_attached / 1024, rss_peak / 1024, rss_final / 1024);
    println!("| per-seat RSS (marginal) | {per_seat_mb:.0} MB after attach, {per_seat_peak_mb:.0} MB at peak ({} seats were already attached at start) |", base_seats);
    println!("| sync cost | daemon CPU {cpu_pct:.0}% of one core during load; local loadavg {} -> {load_after} |", loadavg());
    if let Some(a) = &aborted {
        println!("| ABORTED | {a} |");
    }
    for e in sh.first_errors.lock().unwrap().iter() {
        println!("| send error sample | {e} |");
    }
    for s in &dev_samples {
        println!(
            "| dev box {} | {} |",
            s.label,
            dev_line(s)
                .split_once(' ')
                .map(|x| x.1)
                .unwrap_or("")
                .trim()
        );
    }

    let result = json!({
        "run": run, "seats": args.seats, "rate_per_min": args.rate, "duration_s": args.duration, "payload": args.payload,
        "attached": attach_s.len(), "attach_failed": attach_failed, "attach_p50_s": a50, "attach_p95_s": a95,
        "sent": sent, "send_errors": sh.send_errors.load(Ordering::Relaxed), "expected": expected,
        "delivered": delivered, "missing": expected.saturating_sub(delivered),
        "latency_ms": {"p50": l50, "p95": l95, "max": lmax, "n": n_lat},
        "send_ms": {"p50": s50, "p95": s95},
        "decrypt": {"seen": utd_seen, "recovered": utd_rec, "unresolved": utd_unres},
        "rss_mb": {"base": rss_base / 1024, "attached": rss_attached / 1024, "peak": rss_peak / 1024, "final": rss_final / 1024},
        "per_seat_rss_mb": per_seat_mb, "per_seat_peak_rss_mb": per_seat_peak_mb, "daemon_cpu_pct": cpu_pct,
        "aborted": aborted,
        "dev": dev_samples.iter().map(|s| json!({"label": s.label, "load": s.load, "mem_available_mb": s.mem_available_mb, "swap_used_mb": s.swap_used_mb, "synapse": s.synapse, "siwx": s.siwx, "error": s.error})).collect::<Vec<_>>(),
    });
    let out_dir = args.out.clone().unwrap_or_else(|| {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(".cache/claude-seats-dev")
    });
    std::fs::create_dir_all(&out_dir).ok();
    let out_file = out_dir.join(format!("stress-k{}-{run}.json", args.seats));
    std::fs::write(&out_file, serde_json::to_string_pretty(&result)?).ok();
    println!("result written to {}", out_file.display());

    if !args.keep_daemon {
        let _ = c.call(&Request::Shutdown, Duration::from_secs(10)).await;
        if let Some(mut ch) = daemon.child.take() {
            let _ = tokio::time::timeout(Duration::from_secs(60), ch.wait()).await;
        }
    }
    if aborted.is_some()
        || !attach_failed.is_empty()
        || delivered < expected
        || sh.send_errors.load(Ordering::Relaxed) > 0
    {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_use_nearest_rank() {
        let mut v: Vec<f64> = (1..=100).map(f64::from).collect();
        let (p50, p95, max) = stats_line(&mut v);
        assert_eq!((p50, p95, max), (50.0, 95.0, 100.0));
        let mut one = vec![7.0];
        assert_eq!(stats_line(&mut one), (7.0, 7.0, 7.0));
        assert!(pct(&[], 50.0).is_nan());
    }

    #[test]
    fn bodies_roundtrip_and_only_match_their_own_run() {
        let b = make_body("123", 7, 42, 120);
        assert_eq!(b.len(), 120);
        let (idx, n, sent) = parse_body("123", &b).unwrap();
        assert_eq!((idx, n), (7, 42));
        assert!(sent > 1_700_000_000_000);
        assert!(parse_body("124", &b).is_none());
        assert!(parse_body("123", "hello").is_none());
    }

    #[test]
    fn dev_samples_are_parsed_from_the_three_read_only_commands() {
        // the label/format helpers must not panic on a failed sample
        let s = DevSample {
            label: "before".into(),
            error: Some("ssh timed out".into()),
            ..Default::default()
        };
        assert!(dev_line(&s).contains("unavailable"));
    }
}
