//! Integration tests for parallel health checking via SshWorker + fanout.

use consortium_nix::config::{Builder, FleetConfig};
use consortium_nix::health::{check_builder, check_builders, healthy_builders};
use std::collections::HashMap;

/// Create a test fleet config with mock builders.
fn create_test_fleet() -> FleetConfig {
    let mut builders = HashMap::new();

    // Add 3 mock builder configs (won't actually connect)
    for i in 1..=3 {
        builders.insert(
            format!("builder{}", i),
            Builder {
                host: format!("192.168.1.{}", 100 + i),
                user: "root".to_string(),
                protocol: "ssh-ng".to_string(),
                max_jobs: 4,
                speed_factor: (4 - i) as u32, // Reverse order for sorting
                systems: vec!["x86_64-linux".to_string()],
                features: vec![],
                ssh_key: None,
            },
        );
    }

    FleetConfig {
        flake_uri: "file:///tmp/test".to_string(),
        builders,
        nodes: HashMap::new(),
        ansible_config: None,
        slurm_config: None,
        ray_config: None,
        skypilot_config: None,
    }
}

#[test]
fn test_check_builders_returns_all_builders() {
    let fleet = create_test_fleet();
    let statuses = check_builders(&fleet);

    // Should return 3 status entries (one per builder)
    assert_eq!(statuses.len(), 3);

    // All should have builder hosts set
    for status in &statuses {
        assert!(status.builder.host.starts_with("192.168.1."));
    }
}

#[test]
fn test_check_builder_fallback() {
    let builder = Builder {
        host: "192.168.1.100".to_string(),
        user: "root".to_string(),
        protocol: "ssh-ng".to_string(),
        max_jobs: 4,
        speed_factor: 1,
        systems: vec!["x86_64-linux".to_string()],
        features: vec![],
        ssh_key: None,
    };

    // Call the fallback check_builder function
    let status = check_builder(&builder);

    // Should return a status (may be unhealthy if host unreachable)
    assert_eq!(status.builder.host, "192.168.1.100");
}

#[test]
fn test_healthy_builders_sorting() {
    let fleet = create_test_fleet();
    let statuses = check_builders(&fleet);

    // Get healthy builders (may be empty if hosts unreachable, that's OK)
    let healthy = healthy_builders(&statuses);

    // If any are healthy, they should be sorted by speed factor (highest first)
    if healthy.len() > 1 {
        for i in 0..healthy.len() - 1 {
            assert!(healthy[i].builder.speed_factor >= healthy[i + 1].builder.speed_factor);
        }
    }
}

#[test]
fn test_empty_fleet() {
    let fleet = FleetConfig {
        flake_uri: "file:///tmp/test".to_string(),
        builders: HashMap::new(),
        nodes: HashMap::new(),
        ansible_config: None,
        slurm_config: None,
        ray_config: None,
        skypilot_config: None,
    };

    let statuses = check_builders(&fleet);
    assert!(statuses.is_empty());
}

#[test]
fn test_health_status_fields() {
    let builder = Builder {
        host: "localhost".to_string(),
        user: "user".to_string(),
        protocol: "ssh-ng".to_string(),
        max_jobs: 4,
        speed_factor: 1,
        systems: vec!["x86_64-linux".to_string()],
        features: vec![],
        ssh_key: None,
    };

    let status = check_builder(&builder);

    // Status should have builder host set
    assert_eq!(status.builder.host, "localhost");

    // Either healthy with latency, or unhealthy with error
    if status.healthy {
        assert!(status.latency_ms.is_some());
        assert!(status.error.is_none());
    } else {
        assert!(status.error.is_some());
    }
}
