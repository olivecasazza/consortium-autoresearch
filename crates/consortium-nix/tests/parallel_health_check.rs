//! Integration tests for parallel health checking via DAG.

use consortium_nix::config::{Builder, FleetConfig};
use consortium_nix::health::{check_builders, HealthStatus};
use std::collections::HashMap;

/// Create a mock FleetConfig with 3 builders for testing.
fn create_mock_fleet_with_3_builders() -> FleetConfig {
    let mut builders = HashMap::new();

    builders.insert(
        "builder1".to_string(),
        Builder {
            host: "builder1.local".to_string(),
            user: "root".to_string(),
            max_jobs: 4,
            speed_factor: 100,
            systems: vec!["x86_64-linux".to_string()],
            features: vec!["big-parallel".to_string()],
            ssh_key: None,
            protocol: "ssh-ng".to_string(),
        },
    );

    builders.insert(
        "builder2".to_string(),
        Builder {
            host: "builder2.local".to_string(),
            user: "root".to_string(),
            max_jobs: 4,
            speed_factor: 100,
            systems: vec!["x86_64-linux".to_string()],
            features: vec!["big-parallel".to_string()],
            ssh_key: None,
            protocol: "ssh-ng".to_string(),
        },
    );

    builders.insert(
        "builder3".to_string(),
        Builder {
            host: "builder3.local".to_string(),
            user: "root".to_string(),
            max_jobs: 2,
            speed_factor: 50,
            systems: vec!["x86_64-linux".to_string()],
            features: vec!["kvm".to_string()],
            ssh_key: None,
            protocol: "ssh-ng".to_string(),
        },
    );

    FleetConfig {
        nodes: HashMap::new(),
        builders,
        flake_uri: ".".to_string(),
        ansible_config: None,
        slurm_config: None,
        ray_config: None,
        skypilot_config: None,
    }
}

#[test]
fn test_parallel_health_check_returns_all_builders() {
    // Create a fleet with 3 builders
    let config = create_mock_fleet_with_3_builders();

    // Check health — this will fail for unreachable hosts (expected in test)
    // but the important thing is that we get results for all builders
    let statuses = check_builders(&config);

    // Verify we got one status per builder
    assert_eq!(statuses.len(), 3, "Should have status for all 3 builders");

    // Verify that each status corresponds to a builder
    let host_names: Vec<String> = statuses.iter().map(|s| s.builder.host.clone()).collect();
    assert!(host_names.contains(&"builder1.local".to_string()));
    assert!(host_names.contains(&"builder2.local".to_string()));
    assert!(host_names.contains(&"builder3.local".to_string()));
}

#[test]
fn test_parallel_health_check_empty_fleet() {
    // Create an empty fleet
    let config = FleetConfig {
        nodes: HashMap::new(),
        builders: HashMap::new(),
        flake_uri: ".".to_string(),
        ansible_config: None,
        slurm_config: None,
        ray_config: None,
        skypilot_config: None,
    };

    let statuses = check_builders(&config);

    // Should return empty vec for empty fleet
    assert_eq!(statuses.len(), 0);
}

#[test]
fn test_health_status_structure() {
    // Create a mock fleet with one builder
    let config = create_mock_fleet_with_3_builders();
    let statuses = check_builders(&config);

    // Pick one status and verify its structure
    let status = &statuses[0];

    // Check that all fields are present
    assert!(!status.builder.host.is_empty());
    assert!(!status.builder.user.is_empty());

    // Check that either latency_ms is Some or error is Some
    // (depending on whether the host was reachable)
    if status.healthy {
        // Healthy status should have latency info
        assert!(status.latency_ms.is_some());
        assert!(status.error.is_none());
    } else {
        // Unhealthy status should have error info
        assert!(status.error.is_some());
    }
}
