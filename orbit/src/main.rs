use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use std::ops::RangeInclusive;

use orbit::{Orbit, OrbitConfig, OrbitHandle, Uuid};
use tracing::info;
use tracing_subscriber::EnvFilter;

type Stats = Arc<(AtomicU64, AtomicU64)>; // (allowed, denied)

fn create_orbit_ring(
    base_port: u16,
    node_count: u64,
    orbit_limit: u64,
    rollover_ratio: f64,
    epoch_duration_ms: u64,
) -> Vec<(Arc<Orbit>, OrbitHandle)> {
    let mut nodes = Vec::with_capacity(node_count as usize);

    for i in 0..node_count {
        let port = base_port + i as u16;
        let prev_port = base_port + ((i + node_count - 1) % node_count) as u16;

        let (orbit, handle) = Orbit::new(OrbitConfig {
            server_addr: format!("127.0.0.1:{}", port).parse().unwrap(),
            prev_node_addr: format!("127.0.0.1:{}", prev_port).parse().unwrap(),
            orbit_limit,
            node_count,
            rollover_ratio,
            epoch_duration_ms,
        }).unwrap();

        nodes.push((Arc::new(orbit), handle));
    }

    nodes
}

fn spawn_request_generator(
    orbit: Arc<Orbit>,
    stats: Stats,
    rps_range: RangeInclusive<u32>,
    test_uuid: Uuid,
) {
    tokio::spawn(async move {
        use rand::{Rng, SeedableRng};
        let mut rng = rand::rngs::StdRng::from_os_rng();
        let mut interval = tokio::time::interval(Duration::from_millis(100));
        let mut batch_count = 0u32;
        let mut target_rps = rng.random_range(rps_range.clone());
        loop {
            interval.tick().await;
            batch_count += 1;
            if batch_count >= 10 {
                batch_count = 0;
                target_rps = rng.random_range(rps_range.clone());
            }
            let batch_size = target_rps / 10;
            for _ in 0..batch_size {
                let orbit = Arc::clone(&orbit);
                let stats = Arc::clone(&stats);
                tokio::spawn(async move {
                    let result = orbit.check_and_increment(&test_uuid, 1).await;
                    if result {
                        stats.0.fetch_add(1, Ordering::Relaxed);
                    } else {
                        stats.1.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
        }
    });
}

fn spawn_stats_reporter(all_stats: Vec<Stats>, orbit_limit: u64, node_count: u64, epoch_duration_ms: u64) {
    // Orbit keeps (node_count - 1) epochs in the table
    // Each epoch lasts epoch_duration_ms
    // So the actual window is (node_count - 1) * epoch_duration_ms
    // But strict limit is defined as: node_count * orbit_limit over node_count * epoch_duration_ms
    let strict_limit = node_count * orbit_limit;
    let window_seconds = (node_count * epoch_duration_ms / 1000) as usize;

    tokio::spawn(async move {
        let stats_count = all_stats.len();
        let mut last: Vec<(u64, u64)> = vec![(0, 0); stats_count];
        // Ring buffer for sliding window (1 second granularity)
        let mut window: std::collections::VecDeque<u64> = std::collections::VecDeque::with_capacity(window_seconds + 1);

        // Sync to epoch boundary (same as Orbit does)
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let ms_until_boundary = epoch_duration_ms - (now_ms % epoch_duration_ms);
        tokio::time::sleep(Duration::from_millis(ms_until_boundary)).await;

        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;

            let mut total_allowed = 0u64;
            let mut total_requests = 0u64;

            println!("----------------------------------------");
            for (i, stats) in all_stats.iter().enumerate() {
                let allowed = stats.0.load(Ordering::Relaxed);
                let denied = stats.1.load(Ordering::Relaxed);

                let delta_allowed = allowed - last[i].0;
                let delta_denied = denied - last[i].1;
                let delta_total = delta_allowed + delta_denied;

                let pct = if delta_total > 0 {
                    delta_allowed as f64 / delta_total as f64 * 100.0
                } else {
                    0.0
                };

                println!(
                    "Orbit{:>3}: {:>5}/{:<5} ({:>5.1}%) | rps: {:>5}",
                    i + 1, delta_allowed, delta_total, pct, delta_allowed
                );

                total_allowed += delta_allowed;
                total_requests += delta_total;
                last[i] = (allowed, denied);
            }

            // Update sliding window (ring buffer)
            window.push_back(total_allowed);
            while window.len() > window_seconds {
                window.pop_front();
            }
            let window_total: u64 = window.iter().sum();
            let window_len = window.len();

            let total_pct = if total_requests > 0 {
                total_allowed as f64 / total_requests as f64 * 100.0
            } else {
                0.0
            };
            let limit_usage_pct = window_total as f64 / strict_limit as f64 * 100.0;
            println!(
                "Total:   {:>5}/{:<5} ({:>5.1}%) | rps: {:>5} | limit: {:>5.1}% ({}/{} @{}/{}s)",
                total_allowed, total_requests, total_pct, total_allowed,
                limit_usage_pct, window_total, strict_limit, window_len, window_seconds
            );
        }
    });
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("error"))
        .init();

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let orbit_limit = 100000;
    let epoch_duration_ms = 1000;
    let rollover_ratio = 0.3;
    let node_count = 10;

    info!("Orbit started");

    // Create 10-node ring
    let nodes = create_orbit_ring(
        1110, // base port: 1110~1119
        node_count,
        orbit_limit,
        rollover_ratio,
        epoch_duration_ms,
    );

    let test_uuid: Uuid = [0u8; 16];

    // Create stats for each node
    let all_stats: Vec<Stats> = (0..node_count)
        .map(|_| Arc::new((AtomicU64::new(0), AtomicU64::new(0))))
        .collect();

    info!("Starting load test with {} nodes", node_count);

    // Distribution 1: 3000~7000 rps (3 nodes)
    for i in 0..3 {
        spawn_request_generator(
            Arc::clone(&nodes[i].0),
            Arc::clone(&all_stats[i]),
            3000..=7000,
            test_uuid,
        );
    }

    // Distribution 2: 7000~13000 rps (3 nodes)
    for i in 3..6 {
        spawn_request_generator(
            Arc::clone(&nodes[i].0),
            Arc::clone(&all_stats[i]),
            7000..=13000,
            test_uuid,
        );
    }

    // Distribution 3: 20000~40000 rps (4 nodes)
    for i in 6..10 {
        spawn_request_generator(
            Arc::clone(&nodes[i].0),
            Arc::clone(&all_stats[i]),
            20000..=40000,
            test_uuid,
        );
    }

    // Spawn stats reporter
    spawn_stats_reporter(all_stats, orbit_limit, node_count, epoch_duration_ms);

    // Wait for Ctrl+C signal
    tokio::signal::ctrl_c().await.expect("Failed to listen for Ctrl+C");
    info!("Shutting down...");
}
