//! DAG construction interfaces.
//!
//! Provides two builders:
//! - `DagBuilder` — programmatic, explicit tasks and dependencies
//! - `StageBuilder` — template pattern for "apply N stages to M resources"

use crate::dag::cache::CacheStrategy;
use crate::dag::context::DagContext;
use crate::dag::error::{DagError, Result};
use crate::dag::executor::{DagExecutor, DagMonitor};
use crate::dag::graph::DagGraph;
use crate::dag::pool::{UnlimitedPool, WorkerPool};
use crate::dag::types::{DagTask, ErrorPolicy, TaskId};

/// Builds one [`DagTask`] for a given resource name.
///
/// Stage factories are stored boxed so a `StageDef` stays a plain owned record
/// rather than a generic type that would infect `StageBuilder`.
pub type StageTaskFactory = Box<dyn Fn(&str) -> Box<dyn DagTask>>;

/// Programmatic DAG builder.
///
/// ```rust,no_run
/// use consortium::dag::*;
///
/// let mut dag = DagBuilder::new();
/// dag.add_task("build:hp01", ShellTask { command: "nix build ...".into(), description: "build hp01".into(), resource: None });
/// dag.add_task("copy:hp01", ShellTask { command: "nix copy ...".into(), description: "copy hp01".into(), resource: None });
/// dag.add_dep("copy:hp01", "build:hp01");
/// let executor = dag.build().unwrap();
/// ```
pub struct DagBuilder {
    graph: DagGraph,
    context: DagContext,
    error_policy: ErrorPolicy,
    pool: Option<Box<dyn WorkerPool>>,
    global_cache: Option<Box<dyn CacheStrategy>>,
    monitor: Option<Box<dyn DagMonitor>>,
}

impl DagBuilder {
    pub fn new() -> Self {
        Self {
            graph: DagGraph::new(),
            context: DagContext::new(),
            error_policy: ErrorPolicy::default(),
            pool: None,
            global_cache: None,
            monitor: None,
        }
    }

    /// Add a task to the DAG.
    pub fn add_task(&mut self, id: impl Into<TaskId>, task: impl DagTask) -> &mut Self {
        let _ = self.graph.add_task(id.into(), Box::new(task));
        self
    }

    /// Add a dependency: `dependent` waits for `dependency`.
    pub fn add_dep(
        &mut self,
        dependent: impl Into<TaskId>,
        dependency: impl Into<TaskId>,
    ) -> &mut Self {
        let _ = self.graph.add_dep(&dependent.into(), &dependency.into());
        self
    }

    /// Set a concurrency group limit.
    pub fn concurrency_group(&mut self, name: &str, max_concurrent: usize) -> &mut Self {
        self.graph.set_concurrency_group(name, max_concurrent);
        self
    }

    /// Assign a task to a concurrency group.
    pub fn assign_group(&mut self, task_id: impl Into<TaskId>, group: &str) -> &mut Self {
        self.graph.assign_group(&task_id.into(), group);
        self
    }

    /// Set the error policy.
    pub fn error_policy(&mut self, policy: ErrorPolicy) -> &mut Self {
        self.error_policy = policy;
        self
    }

    /// Set the worker pool.
    pub fn pool(&mut self, pool: impl WorkerPool + 'static) -> &mut Self {
        self.pool = Some(Box::new(pool));
        self
    }

    /// Set the shared context.
    pub fn context(&mut self, ctx: DagContext) -> &mut Self {
        self.context = ctx;
        self
    }

    /// Set a global cache strategy.
    pub fn cache(&mut self, cache: impl CacheStrategy + 'static) -> &mut Self {
        self.global_cache = Some(Box::new(cache));
        self
    }

    /// Set a monitor for execution events.
    pub fn monitor(&mut self, monitor: impl DagMonitor + 'static) -> &mut Self {
        self.monitor = Some(Box::new(monitor));
        self
    }

    /// Build the DAG executor. Validates the graph.
    pub fn build(self) -> Result<DagExecutor> {
        self.graph.validate()?;
        Ok(DagExecutor {
            graph: self.graph,
            context: self.context,
            error_policy: self.error_policy,
            pool: self.pool.unwrap_or_else(|| Box::new(UnlimitedPool)),
            global_cache: self.global_cache,
            monitor: self.monitor,
        })
    }
}

impl Default for DagBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ─── StageBuilder ────────────────────────────────────────────────────────────

/// A stage definition for the template pattern.
struct StageDef {
    name: String,
    concurrency_limit: Option<usize>,
    factory: StageTaskFactory,
}

/// Template/stage pattern builder.
///
/// Generates a DAG where each stage is applied to each resource,
/// with per-resource dependencies between consecutive stages.
///
/// ```rust,no_run
/// use consortium::dag::*;
///
/// let report = StageBuilder::new()
///     .resources(vec!["hp01".into(), "hp02".into()])
///     .stage("build", Some(4), |host| {
///         Box::new(ShellTask {
///             command: format!("nix build ...{}", host),
///             description: format!("build {}", host),
///             resource: None,
///         })
///     })
///     .stage("deploy", Some(2), |host| {
///         Box::new(ShellTask {
///             command: format!("ssh {} ...", host),
///             description: format!("deploy {}", host),
///             resource: None,
///         })
///     })
///     .build()
///     .unwrap()
///     .run()
///     .unwrap();
/// ```
pub struct StageBuilder {
    resources: Vec<String>,
    stages: Vec<StageDef>,
    context: DagContext,
    error_policy: ErrorPolicy,
    pool: Option<Box<dyn WorkerPool>>,
    global_cache: Option<Box<dyn CacheStrategy>>,
    monitor: Option<Box<dyn DagMonitor>>,
}

impl StageBuilder {
    pub fn new() -> Self {
        Self {
            resources: Vec::new(),
            stages: Vec::new(),
            context: DagContext::new(),
            error_policy: ErrorPolicy::default(),
            pool: None,
            global_cache: None,
            monitor: None,
        }
    }

    /// Set the resources (e.g. hostnames) to apply stages to.
    pub fn resources(mut self, resources: Vec<String>) -> Self {
        self.resources = resources;
        self
    }

    /// Add a stage. For each resource, the factory produces a task.
    ///
    /// Tasks are auto-named as `"stage_name:resource"`.
    /// Dependencies: `stage[n]:resource` depends on `stage[n-1]:resource`.
    pub fn stage<F>(mut self, name: &str, concurrency_limit: Option<usize>, factory: F) -> Self
    where
        F: Fn(&str) -> Box<dyn DagTask> + 'static,
    {
        let factory: StageTaskFactory = Box::new(factory);
        self.stages.push(StageDef {
            name: name.to_string(),
            concurrency_limit,
            factory,
        });
        self
    }

    /// Set the error policy.
    pub fn error_policy(mut self, policy: ErrorPolicy) -> Self {
        self.error_policy = policy;
        self
    }

    /// Set the shared context.
    pub fn context(mut self, ctx: DagContext) -> Self {
        self.context = ctx;
        self
    }

    /// Set the worker pool.
    pub fn pool(mut self, pool: impl WorkerPool + 'static) -> Self {
        self.pool = Some(Box::new(pool));
        self
    }

    /// Set a global cache strategy.
    pub fn cache(mut self, cache: impl CacheStrategy + 'static) -> Self {
        self.global_cache = Some(Box::new(cache));
        self
    }

    /// Set a monitor.
    pub fn monitor(mut self, monitor: impl DagMonitor + 'static) -> Self {
        self.monitor = Some(Box::new(monitor));
        self
    }

    /// Build the DAG executor.
    ///
    /// Generates TaskIds as `"stage:resource"` and adds per-resource
    /// dependencies between consecutive stages.
    pub fn build(self) -> Result<DagExecutor> {
        if self.resources.is_empty() {
            return Err(DagError::EmptyDag);
        }
        if self.stages.is_empty() {
            return Err(DagError::EmptyDag);
        }

        let mut graph = DagGraph::new();

        // Create concurrency groups for each stage
        for stage in &self.stages {
            if let Some(limit) = stage.concurrency_limit {
                graph.set_concurrency_group(&stage.name, limit);
            }
        }

        // Create tasks and dependencies
        for resource in &self.resources {
            let mut prev_id: Option<TaskId> = None;

            for stage in &self.stages {
                let task_id = TaskId(format!("{}:{}", stage.name, resource));
                let task = (stage.factory)(resource);

                graph.add_task(task_id.clone(), task)?;
                graph.assign_group(&task_id, &stage.name);

                // Add dependency on previous stage for this resource
                if let Some(ref prev) = prev_id {
                    graph.add_dep(&task_id, prev)?;
                }

                prev_id = Some(task_id);
            }
        }

        graph.validate()?;

        Ok(DagExecutor {
            graph,
            context: self.context,
            error_policy: self.error_policy,
            pool: self.pool.unwrap_or_else(|| Box::new(UnlimitedPool)),
            global_cache: self.global_cache,
            monitor: self.monitor,
        })
    }
}

impl Default for StageBuilder {
    fn default() -> Self {
        Self::new()
    }
}
