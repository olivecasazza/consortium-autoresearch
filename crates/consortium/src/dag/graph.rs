//! DAG data structure with validation and topological sorting.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::dag::cache::CacheStrategy;
use crate::dag::error::{DagError, Result};
use crate::dag::types::{ConcurrencyLimit, DagTask, TaskId};

/// Internal DAG representation using adjacency lists.
#[derive(Default)]
pub struct DagGraph {
    /// All tasks keyed by ID.
    pub(crate) tasks: HashMap<TaskId, Box<dyn DagTask>>,
    /// Forward edges: task -> set of tasks that depend on it (dependents).
    pub(crate) dependents: HashMap<TaskId, HashSet<TaskId>>,
    /// Reverse edges: task -> set of tasks it depends on (dependencies).
    pub(crate) dependencies: HashMap<TaskId, HashSet<TaskId>>,
    /// Concurrency groups with their limits.
    pub(crate) concurrency_groups: HashMap<String, ConcurrencyLimit>,
    /// Which concurrency group each task belongs to.
    pub(crate) task_groups: HashMap<TaskId, String>,
    /// Per-task cache strategy.
    pub(crate) task_cache: HashMap<TaskId, Box<dyn CacheStrategy>>,
}

impl DagGraph {
    /// Create an empty graph.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a task to the graph.
    pub fn add_task(&mut self, id: TaskId, task: Box<dyn DagTask>) -> Result<()> {
        if self.tasks.contains_key(&id) {
            return Err(DagError::DuplicateTask(id));
        }
        self.tasks.insert(id.clone(), task);
        self.dependents.entry(id.clone()).or_default();
        self.dependencies.entry(id).or_default();
        Ok(())
    }

    /// Add a dependency edge: `dependent` cannot start until `dependency` succeeds.
    pub fn add_dep(&mut self, dependent: &TaskId, dependency: &TaskId) -> Result<()> {
        if !self.tasks.contains_key(dependent) {
            return Err(DagError::UnknownTask(dependent.clone()));
        }
        if !self.tasks.contains_key(dependency) {
            return Err(DagError::UnknownTask(dependency.clone()));
        }
        self.dependents
            .entry(dependency.clone())
            .or_default()
            .insert(dependent.clone());
        self.dependencies
            .entry(dependent.clone())
            .or_default()
            .insert(dependency.clone());
        Ok(())
    }

    /// Set a concurrency group with a limit.
    pub fn set_concurrency_group(&mut self, name: &str, max_concurrent: usize) {
        self.concurrency_groups.insert(
            name.to_string(),
            ConcurrencyLimit {
                max_concurrent: Some(max_concurrent),
            },
        );
    }

    /// Assign a task to a concurrency group.
    pub fn assign_group(&mut self, task_id: &TaskId, group: &str) {
        self.task_groups.insert(task_id.clone(), group.to_string());
    }

    /// Validate the graph: check for cycles using Kahn's algorithm.
    pub fn validate(&self) -> Result<()> {
        if self.tasks.is_empty() {
            return Err(DagError::EmptyDag);
        }

        // Kahn's algorithm for cycle detection
        let mut in_degree: HashMap<&TaskId, usize> = HashMap::new();
        for id in self.tasks.keys() {
            let deps = self.dependencies.get(id).map(|d| d.len()).unwrap_or(0);
            in_degree.insert(id, deps);
        }

        let mut queue: VecDeque<&TaskId> = in_degree
            .iter()
            .filter(|(_, &deg)| deg == 0)
            .map(|(&id, _)| id)
            .collect();

        let mut visited = 0;

        while let Some(id) = queue.pop_front() {
            visited += 1;
            if let Some(deps) = self.dependents.get(id) {
                for dep in deps {
                    if let Some(degree) = in_degree.get_mut(dep) {
                        *degree -= 1;
                        if *degree == 0 {
                            queue.push_back(dep);
                        }
                    }
                }
            }
        }

        if visited != self.tasks.len() {
            // Find a task involved in the cycle
            let cycle_task = in_degree
                .iter()
                .find(|(_, &deg)| deg > 0)
                .map(|(&id, _)| id.clone())
                .unwrap_or_else(|| TaskId("unknown".into()));
            return Err(DagError::CycleDetected(cycle_task));
        }

        Ok(())
    }

    /// Return task IDs with zero dependencies (roots/starting points).
    pub fn roots(&self) -> Vec<TaskId> {
        self.tasks
            .keys()
            .filter(|id| {
                self.dependencies
                    .get(*id)
                    .map(|d| d.is_empty())
                    .unwrap_or(true)
            })
            .cloned()
            .collect()
    }

    /// Return a topological ordering of task IDs.
    pub fn topo_sort(&self) -> Result<Vec<TaskId>> {
        let mut in_degree: HashMap<&TaskId, usize> = HashMap::new();
        for id in self.tasks.keys() {
            let deps = self.dependencies.get(id).map(|d| d.len()).unwrap_or(0);
            in_degree.insert(id, deps);
        }

        let mut queue: VecDeque<&TaskId> = in_degree
            .iter()
            .filter(|(_, &deg)| deg == 0)
            .map(|(&id, _)| id)
            .collect();

        let mut order = Vec::new();

        while let Some(id) = queue.pop_front() {
            order.push(id.clone());
            if let Some(deps) = self.dependents.get(id) {
                for dep in deps {
                    if let Some(degree) = in_degree.get_mut(dep) {
                        *degree -= 1;
                        if *degree == 0 {
                            queue.push_back(dep);
                        }
                    }
                }
            }
        }

        if order.len() != self.tasks.len() {
            let cycle_task = in_degree
                .iter()
                .find(|(_, &deg)| deg > 0)
                .map(|(&id, _)| id.clone())
                .unwrap_or_else(|| TaskId("unknown".into()));
            return Err(DagError::CycleDetected(cycle_task));
        }

        Ok(order)
    }

    /// Take a task out of the graph (for dispatching to a worker thread).
    pub fn take_task(&mut self, id: &TaskId) -> Option<Box<dyn DagTask>> {
        self.tasks.remove(id)
    }

    /// Number of tasks.
    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    /// Whether the graph is empty.
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::types::{FnTask, TaskOutcome};

    fn noop_task(name: &str) -> Box<dyn DagTask> {
        let n = name.to_string();
        Box::new(FnTask::new(n, |_| TaskOutcome::Success))
    }

    #[test]
    fn test_empty_dag() {
        let g = DagGraph::new();
        assert!(matches!(g.validate(), Err(DagError::EmptyDag)));
    }

    #[test]
    fn test_single_task() {
        let mut g = DagGraph::new();
        g.add_task(TaskId::from("a"), noop_task("a")).unwrap();
        assert!(g.validate().is_ok());
        assert_eq!(g.roots(), vec![TaskId::from("a")]);
    }

    #[test]
    fn test_linear_chain() {
        let mut g = DagGraph::new();
        g.add_task(TaskId::from("a"), noop_task("a")).unwrap();
        g.add_task(TaskId::from("b"), noop_task("b")).unwrap();
        g.add_task(TaskId::from("c"), noop_task("c")).unwrap();
        g.add_dep(&TaskId::from("b"), &TaskId::from("a")).unwrap();
        g.add_dep(&TaskId::from("c"), &TaskId::from("b")).unwrap();

        assert!(g.validate().is_ok());
        assert_eq!(g.roots(), vec![TaskId::from("a")]);

        let order = g.topo_sort().unwrap();
        let a_pos = order.iter().position(|t| t.0 == "a").unwrap();
        let b_pos = order.iter().position(|t| t.0 == "b").unwrap();
        let c_pos = order.iter().position(|t| t.0 == "c").unwrap();
        assert!(a_pos < b_pos);
        assert!(b_pos < c_pos);
    }

    #[test]
    fn test_diamond_dag() {
        let mut g = DagGraph::new();
        g.add_task(TaskId::from("a"), noop_task("a")).unwrap();
        g.add_task(TaskId::from("b"), noop_task("b")).unwrap();
        g.add_task(TaskId::from("c"), noop_task("c")).unwrap();
        g.add_task(TaskId::from("d"), noop_task("d")).unwrap();
        g.add_dep(&TaskId::from("b"), &TaskId::from("a")).unwrap();
        g.add_dep(&TaskId::from("c"), &TaskId::from("a")).unwrap();
        g.add_dep(&TaskId::from("d"), &TaskId::from("b")).unwrap();
        g.add_dep(&TaskId::from("d"), &TaskId::from("c")).unwrap();

        assert!(g.validate().is_ok());
        let order = g.topo_sort().unwrap();
        let a_pos = order.iter().position(|t| t.0 == "a").unwrap();
        let d_pos = order.iter().position(|t| t.0 == "d").unwrap();
        assert!(a_pos < d_pos);
    }

    #[test]
    fn test_cycle_detection() {
        let mut g = DagGraph::new();
        g.add_task(TaskId::from("a"), noop_task("a")).unwrap();
        g.add_task(TaskId::from("b"), noop_task("b")).unwrap();
        g.add_dep(&TaskId::from("b"), &TaskId::from("a")).unwrap();
        g.add_dep(&TaskId::from("a"), &TaskId::from("b")).unwrap();

        assert!(matches!(g.validate(), Err(DagError::CycleDetected(_))));
    }

    #[test]
    fn test_duplicate_task() {
        let mut g = DagGraph::new();
        g.add_task(TaskId::from("a"), noop_task("a")).unwrap();
        assert!(matches!(
            g.add_task(TaskId::from("a"), noop_task("a")),
            Err(DagError::DuplicateTask(_))
        ));
    }

    #[test]
    fn test_unknown_dep() {
        let mut g = DagGraph::new();
        g.add_task(TaskId::from("a"), noop_task("a")).unwrap();
        assert!(matches!(
            g.add_dep(&TaskId::from("a"), &TaskId::from("missing")),
            Err(DagError::UnknownTask(_))
        ));
    }

    #[test]
    fn test_self_loop_cycle() {
        let mut g = DagGraph::new();
        g.add_task(TaskId::from("a"), noop_task("a")).unwrap();
        g.add_dep(&TaskId::from("a"), &TaskId::from("a")).unwrap();
        assert!(matches!(g.validate(), Err(DagError::CycleDetected(_))));
    }

    #[test]
    fn test_deep_cycle() {
        let mut g = DagGraph::new();
        for name in &["a", "b", "c", "d", "e"] {
            g.add_task(TaskId::from(*name), noop_task(name)).unwrap();
        }
        g.add_dep(&TaskId::from("b"), &TaskId::from("a")).unwrap();
        g.add_dep(&TaskId::from("c"), &TaskId::from("b")).unwrap();
        g.add_dep(&TaskId::from("d"), &TaskId::from("c")).unwrap();
        g.add_dep(&TaskId::from("e"), &TaskId::from("d")).unwrap();
        g.add_dep(&TaskId::from("a"), &TaskId::from("e")).unwrap(); // closes the cycle
        assert!(matches!(g.validate(), Err(DagError::CycleDetected(_))));
    }

    #[test]
    fn test_large_dag_performance() {
        let mut g = DagGraph::new();
        // 1000 tasks in a linear chain — should validate in <100ms
        for i in 0..1000 {
            let id = format!("t{}", i);
            g.add_task(TaskId::from(id.as_str()), noop_task(&id))
                .unwrap();
            if i > 0 {
                let prev = format!("t{}", i - 1);
                g.add_dep(&TaskId::from(id.as_str()), &TaskId::from(prev.as_str()))
                    .unwrap();
            }
        }
        let start = std::time::Instant::now();
        assert!(g.validate().is_ok());
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "1000-task DAG validation took {:?}",
            elapsed
        );
        let order = g.topo_sort().unwrap();
        assert_eq!(order.len(), 1000);
        assert_eq!(order[0], TaskId::from("t0"));
        assert_eq!(order[999], TaskId::from("t999"));
    }
}
