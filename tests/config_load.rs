use blipmq::config::load_config;
use blipmq::Config;

#[test]
fn load_config_matches_toml() {
    let cfg: Config = load_config("blipmq.toml").expect("failed to load config");

    assert_eq!(cfg.server.bind_addr, "127.0.0.1:8080");
    assert_eq!(cfg.server.max_connections, 100);
    assert_eq!(cfg.auth.api_keys, vec!["supersecretkey".to_string()]);
    assert_eq!(cfg.queues.overflow_policy, "drop_oldest");
    assert_eq!(cfg.wal.directory, "./wal");
    assert_eq!(cfg.wal.segment_size_bytes, 1_048_576);
    assert_eq!(cfg.wal.flush_interval_ms, 5000);
    assert_eq!(cfg.metrics.bind_addr, "127.0.0.1:9090");
    assert_eq!(cfg.delivery.max_batch, 64);
}
