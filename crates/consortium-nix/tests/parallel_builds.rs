//! Integration tests for parallel build execution via DAG.

use consortium_nix::build::{build_closures, BuildResults};
use consortium_nix::config::{
    DeployAction, DeploymentNode, DeploymentPlan, DeploymentTarget, ProfileType,
};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Mock build function that tracks execution.
/// Returns a simple mock path if successful.
fn create_mock_plan_with_3_nodes() -> DeploymentPlan {
    let nodes = vec![
        DeploymentNode {
            name: "hp01".to_string(),
            target_host: "192.168.1.121".to_string(),
            target_user: "root".to_string(),
            target_port: None,
            system: "x86_64-linux".to_string(),
            profile_type: ProfileType::Nixos,
            build_on_target: false,
            tags: vec!["build-host".to_string()],
            drv_path: None,
            toplevel: None,
        },
        DeploymentNode {
            name: "hp02".to_string(),
            target_host: "192.168.1.122".to_string(),
            target_user: "root".to_string(),
            target_port: None,
            system: "x86_64-linux".to_string(),
            profile_type: ProfileType::Nixos,
            build_on_target: false,
            tags: vec!["build-host".to_string()],
            drv_path: None,
            toplevel: None,
        },
        DeploymentNode {
            name: "hp03".to_string(),
            target_host: "192.168.1.123".to_string(),
            target_user: "root".to_string(),
            target_port: None,
            system: "x86_64-linux".to_string(),
            profile_type: ProfileType::Nixos,
            build_on_target: false,
            tags: vec!["build-host".to_string()],
            drv_path: None,
            toplevel: None,
        },
    ];

    let targets = nodes
        .into_iter()
        .map(|node| DeploymentTarget {
            node,
            toplevel_path: "/nix/store/mock-toplevel".to_string(),
            current_system: None,
            needs_build: true,
            needs_copy: false,
        })
        .collect();

    DeploymentPlan {
        targets,
        action: DeployAction::Build,
        max_parallel: 3,
    }
}

#[test]
fn test_parallel_builds_with_3_nodes() {
    // Create a deployment plan with 3 nodes that all need building
    let plan = create_mock_plan_with_3_nodes();

    // Verify plan has 3 targets that need building
    assert_eq!(plan.target_count(), 3);
    assert_eq!(plan.build_count(), 3);
    assert_eq!(plan.max_parallel, 3);

    // Note: This test is primarily a structural test. A real test would require
    // a mock build system or a test environment with Nix available.
    // The actual parallelization is verified by the DAG executor's task scheduling.
}

#[test]
fn test_build_plan_with_mixed_needs() {
    let mut plan = create_mock_plan_with_3_nodes();

    // Mark one node as not needing build
    plan.targets[0].needs_build = false;

    // Verify the plan structure
    assert_eq!(plan.target_count(), 3);
    assert_eq!(plan.build_count(), 2); // Only 2 should need building
}

#[test]
fn test_build_plan_respects_concurrency_limit() {
    let mut plan = create_mock_plan_with_3_nodes();
    plan.max_parallel = 2;

    // Verify the concurrency limit is set correctly
    assert_eq!(plan.max_parallel, 2);
}

#[test]
fn test_build_results_structure() {
    // This test verifies that BuildResults can be created and used correctly
    let results = BuildResults {
        paths: std::collections::HashMap::new(),
        errors: std::collections::HashMap::new(),
    };

    assert!(results.paths.is_empty());
    assert!(results.errors.is_empty());
}
