//! One verified, untapped benchmark sample. Orchestrated by tools/ubus-benchmark.py.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Barrier};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::json;
use ubus::blobmsg::{lookup, BlobMsgTable};
use ubus::client::UbusClient;
use ubus::server::{DispatchOutcome, UbusConnection, UbusObject};
use ubus::{Transport, UbusError};
use ubus_testkit::{UbusdHarness, UnixUbusTransport};

type Client = UbusClient<UnixUbusTransport>;
type Result<T> = std::result::Result<T, String>;
const OBJECT: &str = "benchmark.echo";

struct Config {
    broker: String,
    workload: String,
    iterations: usize,
    warmup: usize,
    clients: usize,
    payload_bytes: usize,
}

impl Config {
    fn parse(args: &[String]) -> Result<Self> {
        if args.len() != 6 {
            return Err("usage: ubus-bench <upstream|rust> <connect|ping|lookup|echo> <iterations> <warmup> <clients> <payload-bytes>".into());
        }
        let number = |index: usize, min, max| -> Result<usize> {
            let value = args[index].parse::<usize>().map_err(|e| e.to_string())?;
            if !(min..=max).contains(&value) {
                return Err(format!("argument {} must be in {min}..={max}", index + 1));
            }
            Ok(value)
        };
        if !matches!(args[0].as_str(), "upstream" | "rust")
            || !matches!(args[1].as_str(), "connect" | "ping" | "lookup" | "echo")
        {
            return Err("unknown broker or workload".into());
        }
        Ok(Self {
            broker: args[0].clone(),
            workload: args[1].clone(),
            iterations: number(2, 1, 1_000_000)?,
            warmup: number(3, 0, 1_000_000)?,
            clients: number(4, 1, 64)?,
            payload_bytes: number(5, 0, 65_536)?,
        })
    }
}

fn connect(path: &str) -> Result<Client> {
    UbusClient::connect(UnixUbusTransport::connect(path).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}

fn echo(args: &[u8]) -> ubus::Result<BlobMsgTable> {
    let payload = lookup(args, "payload")
        .and_then(|a| a.as_str())
        .ok_or(UbusError::Malformed("missing benchmark payload"))?;
    let client = lookup(args, "client")
        .and_then(|a| a.as_i32())
        .ok_or(UbusError::Malformed("missing benchmark client"))?;
    let sequence = lookup(args, "sequence")
        .and_then(|a| a.as_i32())
        .ok_or(UbusError::Malformed("missing benchmark sequence"))?;
    Ok(BlobMsgTable::build(|b| {
        b.put_str(Some("payload"), payload);
        b.put_i32(Some("client"), client);
        b.put_i32(Some("sequence"), sequence);
    }))
}

struct Provider {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<()>>>,
}

impl Provider {
    fn start(path: String) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            let transport = UnixUbusTransport::connect(&path).map_err(|e| e.to_string())?;
            let mut connection = UbusConnection::connect_and_register(
                transport,
                UbusObject::new(OBJECT).method("echo", echo),
            )
            .map_err(|e| e.to_string())?;
            ready_tx.send(()).map_err(|e| e.to_string())?;
            while !stopping.load(Ordering::Relaxed) {
                match connection.poll_one().map_err(|e| e.to_string())? {
                    DispatchOutcome::Idle => connection.transport_mut().wait_recv(),
                    DispatchOutcome::HandlerErr(error) => return Err(error.to_string()),
                    _ => {}
                }
            }
            Ok(())
        });
        let provider = Self {
            stop,
            thread: Some(thread),
        };
        ready_rx
            .recv_timeout(Duration::from_secs(15))
            .map_err(|e| format!("provider registration: {e}"))?;
        Ok(provider)
    }

    fn finish(mut self) -> Result<()> {
        self.stop.store(true, Ordering::Relaxed);
        self.thread
            .take()
            .expect("provider thread")
            .join()
            .map_err(|_| "provider panicked".to_string())?
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Worker {
    client: Option<Client>,
    object: u32,
    identity: i32,
    payload: String,
}

impl Worker {
    fn operation(&mut self, path: &str, workload: &str, sequence: i32) -> Result<()> {
        match workload {
            "connect" => {
                // Includes socket connect, HELLO validation and closing the connection.
                let client = connect(path)?;
                if client.peer_id().is_none() {
                    return Err("missing HELLO identity".into());
                }
            }
            "ping" => self
                .client
                .as_mut()
                .expect("persistent client")
                .ping()
                .map_err(|e| e.to_string())?,
            "lookup" => {
                if self
                    .client
                    .as_mut()
                    .expect("persistent client")
                    .lookup(OBJECT)
                    .map_err(|e| e.to_string())?
                    != self.object
                {
                    return Err("lookup returned the wrong object".into());
                }
            }
            "echo" => {
                let args = BlobMsgTable::build(|b| {
                    b.put_str(Some("payload"), &self.payload);
                    b.put_i32(Some("client"), self.identity);
                    b.put_i32(Some("sequence"), sequence);
                });
                let reply = self
                    .client
                    .as_mut()
                    .expect("persistent client")
                    .invoke(self.object, "echo", &args)
                    .map_err(|e| e.to_string())?;
                if reply != args.contents() {
                    return Err("echo payload/client/sequence mismatch".into());
                }
            }
            _ => unreachable!("validated workload"),
        }
        Ok(())
    }
}

fn run(config: &Config) -> Result<serde_json::Value> {
    // The compatibility harness adds the same credential shim to both brokers.
    // Other instrumentation would invalidate this measurement.
    for (key, value) in std::env::vars_os() {
        if !value.is_empty()
            && (key == "UBUS_ZERO_FAULT"
                || key == "LD_PRELOAD"
                || key.to_string_lossy().starts_with("UBUS_TESTKIT_"))
        {
            return Err("unset fault injection, LD_PRELOAD and UBUS_TESTKIT_* overrides before benchmarking".into());
        }
    }
    let broker = match config.broker.as_str() {
        "upstream" => UbusdHarness::start_upstream(),
        _ => UbusdHarness::start_rust(),
    }
    .map_err(|e| e.to_string())?
    .ok_or("benchmark broker binary missing")?;
    let path = broker.socket_path().to_string_lossy().into_owned();
    let provider = if matches!(config.workload.as_str(), "lookup" | "echo") {
        Some(Provider::start(path.clone())?)
    } else {
        None
    };
    let mut workers = Vec::new();
    for identity in 0..config.clients {
        let mut client = if config.workload == "connect" {
            None
        } else {
            Some(connect(&path)?)
        };
        let object = if provider.is_some() {
            client
                .as_mut()
                .expect("persistent provider client")
                .lookup(OBJECT)
                .map_err(|e| e.to_string())?
        } else {
            0
        };
        let mut worker = Worker {
            client,
            object,
            identity: identity as i32,
            payload: "x".repeat(config.payload_bytes),
        };
        for sequence in 0..config.warmup {
            worker.operation(&path, &config.workload, sequence as i32)?;
        }
        workers.push(worker);
    }
    let gate = Barrier::new(config.clients + 1);
    let (elapsed_ns, latencies) = thread::scope(|scope| -> Result<_> {
        let mut threads = Vec::new();
        for mut worker in workers {
            let (gate, path) = (&gate, &path);
            threads.push(scope.spawn(move || -> Result<Vec<u64>> {
                let mut latencies = Vec::with_capacity(config.iterations);
                gate.wait();
                for sequence in config.warmup..config.warmup + config.iterations {
                    let start = Instant::now();
                    worker.operation(path, &config.workload, sequence as i32)?;
                    latencies.push(start.elapsed().as_nanos() as u64);
                }
                Ok(latencies)
            }));
        }
        let start = Instant::now();
        gate.wait();
        let mut latencies = Vec::with_capacity(config.clients * config.iterations);
        for thread in threads {
            latencies.extend(thread.join().map_err(|_| "client panicked")??);
        }
        Ok((start.elapsed().as_nanos() as u64, latencies))
    })?;
    if let Some(provider) = provider {
        provider.finish()?;
    }
    Ok(json!({
        "schema_version": 1, "broker": config.broker, "workload": config.workload,
        "clients": config.clients, "payload_bytes": config.payload_bytes,
        "iterations": config.iterations, "warmup": config.warmup,
        "operations": latencies.len(), "elapsed_ns": elapsed_ns, "latency_ns": latencies,
    }))
}

fn main() -> std::process::ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match Config::parse(&args).and_then(|config| run(&config)) {
        Ok(result) => {
            println!("{result}");
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("ubus-bench: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
