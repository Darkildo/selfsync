//! Массовые прогоны симуляции: `notesync-sim --runs 10000 [--seed N] [--threads T]`.
//! Падение воспроизводится по seed: `notesync-sim --seed <seed> --runs 1`.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use notesync_sim::{SimConfig, run_seed};

fn arg(args: &[String], name: &str) -> Option<u64> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
}

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let runs = arg(&args, "--runs").unwrap_or(1000);
    let start = arg(&args, "--seed").unwrap_or(1);
    let threads = arg(&args, "--threads")
        .map(|t| t as usize)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));
    let next = AtomicU64::new(0);
    let done = AtomicUsize::new(0);
    let failures: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let t0 = std::time::Instant::now();
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    if i >= runs {
                        break;
                    }
                    let seed = start + i;
                    if let Err(e) = run_seed(SimConfig::random(seed)) {
                        eprintln!("ПРОВАЛ seed {seed}");
                        if let Ok(mut f) = failures.lock() {
                            f.push(e);
                        }
                    }
                    let d = done.fetch_add(1, Ordering::SeqCst) + 1;
                    if d % 500 == 0 {
                        eprintln!("{d}/{runs} прогонов, {:.0?}", t0.elapsed());
                    }
                }
            });
        }
    });
    let failures = failures.into_inner().unwrap_or_default();
    println!(
        "прогонов: {runs}, провалов: {}, время: {:.1?}",
        failures.len(),
        t0.elapsed()
    );
    if let Some(first) = failures.first() {
        println!("\nпервый провал:\n{first}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}
