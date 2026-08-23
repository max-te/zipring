//! Measures what a request costs the *server*, rather than what a round trip costs
//! the benchmark.
//!
//! Wall-clock timing of a loopback round trip carries roughly 34 us of client and
//! kernel overhead that has nothing to do with zipring, and drifts by 8-20% between
//! runs on an otherwise idle machine -- more than the effects we are usually chasing.
//! The server's own CPU time, read from `/proc/<pid>/stat`, holds to a few percent.
//!
//! Two habits keep the comparison honest:
//!
//!   * every variant is measured in the same minute, round-robin, so machine drift
//!     lands on all of them alike rather than on whichever ran last;
//!   * the first scenario is a 404, which exercises the request path and writes a
//!     header and nothing else. It is the floor; read every other row net of it.
//!
//! Run with `cargo bench --bench server_cpu`, and run nothing else meanwhile: the
//! measurement is only as quiet as the machine. `BENCH_REQUESTS` and `BENCH_ROUNDS`
//! override the defaults.

// Counts become f64 to be averaged; the precision lost is far below the tick resolution.
#![allow(clippy::cast_precision_loss)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_REQUESTS: usize = 20_000;
const DEFAULT_ROUNDS: usize = 5;
/// Well below this 16-core box's parallelism, so the client never has to fight the
/// server for a core.
const SERVER_THREADS: usize = 4;
const ZIP_PATH: &str = "tests/resources/Universal_Declaration_of_Human_Rights.htmlz";

/// A server configuration to compare. The label is what appears in the report.
///
/// `binary` names an alternative executable, which is how one commit is compared
/// against another: build the other revision elsewhere (`cargo build --release
/// --target-dir /tmp/before`) and point a variant at it. `None` uses this build.
struct Variant {
    label: &'static str,
    uring_flags: &'static str,
    binary: Option<&'static str>,
}

const VARIANTS: &[Variant] = &[
    Variant {
        label: "baseline",
        uring_flags: "",
        binary: None,
    },
    Variant {
        label: "single+coop",
        uring_flags: "single_issuer,coop_taskrun",
        binary: None,
    },
    Variant {
        label: "defer",
        uring_flags: "single_issuer,coop_taskrun,defer_taskrun",
        binary: None,
    },
];

/// A request to repeat. The first is the floor against which the others are read.
struct Scenario {
    label: &'static str,
    path: &'static str,
    encoding: &'static str,
}

const SCENARIOS: &[Scenario] = &[
    Scenario {
        label: "404 (floor)",
        path: "/nonexistent",
        encoding: "identity",
    },
    Scenario {
        label: "small file",
        path: "/metadata.opf",
        encoding: "identity",
    },
    Scenario {
        label: "passthrough gzip",
        path: "/index.html",
        encoding: "gzip",
    },
    Scenario {
        label: "inflated",
        path: "/index.html",
        encoding: "identity",
    },
    Scenario {
        label: "listing",
        path: "/images",
        encoding: "identity",
    },
];

struct Server {
    process: Child,
    addr: SocketAddr,
}

impl Server {
    // The child is reaped by this type's Drop impl, which clippy does not see.
    #[allow(clippy::zombie_processes)]
    fn start(variant: &Variant) -> Server {
        let port = free_port();
        let process = Command::new(variant.binary.unwrap_or(env!("CARGO_BIN_EXE_zipring")))
            .arg(ZIP_PATH)
            .env("RUST_LOG", "error")
            .env("PORT", port.to_string())
            .env("ZIPRING_THREADS", SERVER_THREADS.to_string())
            .env("ZIPRING_URING_FLAGS", variant.uring_flags)
            .spawn()
            .expect("server should start");

        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        for _ in 0..200 {
            if TcpStream::connect(addr).is_ok() {
                return Server { process, addr };
            }
            thread::sleep(Duration::from_millis(25));
        }
        panic!("server for variant {} never came up", variant.label);
    }

    /// CPU seconds the server has burned so far, summed over all its threads.
    fn cpu_seconds(&self) -> f64 {
        let pid = self.process.id();
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("server should live");
        // The comm field may contain spaces; everything after the last ')' is fixed-width.
        let fields: Vec<&str> = stat[stat.rfind(')').expect("stat has a comm field")..]
            .split_whitespace()
            .collect();
        let utime: f64 = fields[12].parse().expect("utime should be a number");
        let stime: f64 = fields[13].parse().expect("stime should be a number");
        let ticks_per_second = 100.0;
        (utime + stime) / ticks_per_second
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("should be able to bind an ephemeral port")
        .local_addr()
        .expect("bound listener should have an address")
        .port()
}

/// One keep-alive connection, issuing requests one at a time.
struct Client {
    stream: TcpStream,
    reader: BufReader<TcpStream>,
}

impl Client {
    fn connect(addr: SocketAddr) -> Client {
        let stream = TcpStream::connect(addr).expect("server should accept");
        stream.set_nodelay(true).expect("nodelay should be settable");
        let reader = BufReader::new(stream.try_clone().expect("stream should clone"));
        Client { stream, reader }
    }

    /// Send one request and drain its response, returning the body length.
    fn request(&mut self, scenario: &Scenario) -> usize {
        // A single write: the server rejects a request split across reads.
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: {}\r\n\r\n",
            scenario.path, scenario.encoding
        );
        self.stream
            .write_all(request.as_bytes())
            .expect("request should be writable");

        let mut content_length = None;
        let mut chunked = false;
        loop {
            let mut line = String::new();
            let read = self.reader.read_line(&mut line).expect("header is readable");
            assert!(read != 0, "server closed the connection mid-response");
            let line = line.trim_end_matches("\r\n");
            if line.is_empty() {
                break;
            }
            if let Some(value) = line.strip_prefix("Content-Length: ") {
                content_length = Some(value.parse::<usize>().expect("length should parse"));
            }
            if line == "Transfer-Encoding: chunked" {
                chunked = true;
            }
        }

        if let Some(len) = content_length {
            self.drain(len as u64);
            len
        } else if chunked {
            let mut total = 0;
            loop {
                let mut line = String::new();
                self.reader.read_line(&mut line).expect("chunk size readable");
                let size = usize::from_str_radix(line.trim_end_matches("\r\n"), 16)
                    .expect("chunk size should be hex");
                if size == 0 {
                    self.reader
                        .read_exact(&mut [0; 2])
                        .expect("final CRLF readable");
                    break;
                }
                total += size;
                self.drain(size as u64 + 2);
            }
            total
        } else {
            0
        }
    }

    fn drain(&mut self, bytes: u64) {
        std::io::copy(&mut self.reader.by_ref().take(bytes), &mut std::io::sink())
            .expect("body should be readable");
    }
}

/// Per-request wall time and server CPU time, in microseconds.
#[derive(Clone, Copy)]
struct Sample {
    wall: f64,
    cpu: f64,
}

fn measure(server: &Server, scenario: &Scenario, requests: usize) -> Sample {
    let mut client = Client::connect(server.addr);
    for _ in 0..100 {
        client.request(scenario);
    }

    let cpu_before = server.cpu_seconds();
    let started = Instant::now();
    for _ in 0..requests {
        client.request(scenario);
    }
    let wall = started.elapsed().as_secs_f64();
    let cpu = server.cpu_seconds() - cpu_before;

    let per_request = 1e6 / requests as f64;
    Sample {
        wall: wall * per_request,
        cpu: cpu * per_request,
    }
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

/// Spread as a percentage of the median, which is how much of any difference between
/// variants is noise rather than signal.
fn spread(values: &[f64], median: f64) -> f64 {
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    (max - min) / median * 100.0
}

/// CPU time counts on-CPU ticks, so it doubles when the governor leaves the cores at
/// idle frequency. A run is only comparable against itself, and barely that unless the
/// machine is held at a fixed clock.
fn report_governor() {
    let governor = std::fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
        .map_or_else(|_| "unknown".to_string(), |g| g.trim().to_string());
    println!("cpu governor: {governor}");
    if governor != "performance" {
        println!(
            "  note: absolute figures scale with clock frequency under this governor.\n  \
             `cpupower frequency-set -g performance` before a run worth quoting."
        );
    }
}

/// Bring the machine to a steady clock before the first measurement, so that round one
/// is not systematically slower than the rest.
fn warm_up(servers: &[Server]) {
    for server in servers {
        let mut client = Client::connect(server.addr);
        for _ in 0..20_000 {
            client.request(&SCENARIOS[SCENARIOS.len() - 1]);
        }
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn main() {
    let requests = env_usize("BENCH_REQUESTS", DEFAULT_REQUESTS);
    let rounds = env_usize("BENCH_ROUNDS", DEFAULT_ROUNDS);

    // All variants stay up for the whole run, so each round measures them minutes
    // apart from nothing and seconds apart from each other.
    let servers: Vec<Server> = VARIANTS.iter().map(Server::start).collect();
    let mut samples = vec![vec![Vec::with_capacity(rounds); SCENARIOS.len()]; VARIANTS.len()];

    eprint!("\rwarming up");
    warm_up(&servers);

    for round in 0..rounds {
        eprint!("\rround {}/{rounds}", round + 1);
        for (s, scenario) in SCENARIOS.iter().enumerate() {
            // Scenario outermost puts the variants of one comparison seconds apart
            // rather than a whole round apart, and rotating their order stops the
            // variant that always goes first from wearing the clock's ramp.
            for offset in 0..servers.len() {
                let v = (offset + round) % servers.len();
                samples[v][s].push(measure(&servers[v], scenario, requests));
            }
        }
    }
    eprintln!("\r{:30}\r", "");

    println!(
        "{requests} requests x {rounds} rounds, {SERVER_THREADS} server threads, \
         one keep-alive connection"
    );
    report_governor();
    println!(
        "\n{:<18} {:<14} {:>12} {:>12} {:>8} {:>12} {:>14}",
        "scenario", "variant", "wall us/req", "cpu us/req", "spread", "cpu - floor", "vs baseline"
    );

    for (s, scenario) in SCENARIOS.iter().enumerate() {
        for (v, variant) in VARIANTS.iter().enumerate() {
            let mut walls: Vec<f64> = samples[v][s].iter().map(|s| s.wall).collect();
            let mut cpus: Vec<f64> = samples[v][s].iter().map(|s| s.cpu).collect();
            let wall = median(&mut walls);
            let cpu = median(&mut cpus);
            let mut floors: Vec<f64> = samples[v][0].iter().map(|s| s.cpu).collect();
            let net = cpu - median(&mut floors);

            // Ratios are taken within a round, where both variants saw the same clock,
            // and only then reduced -- which is what survives frequency scaling.
            let mut ratios: Vec<f64> = samples[v][s]
                .iter()
                .zip(&samples[0][s])
                .map(|(sample, base)| sample.cpu / base.cpu)
                .collect();
            let ratio = median(&mut ratios);

            println!(
                "{:<18} {:<14} {:>12.1} {:>12.1} {:>7.1}% {:>12} {:>13.2}x",
                if v == 0 { scenario.label } else { "" },
                variant.label,
                wall,
                cpu,
                spread(&cpus, cpu),
                if s == 0 {
                    "--".to_string()
                } else {
                    format!("{net:.1}")
                },
                ratio,
            );
        }
        println!();
    }
}
