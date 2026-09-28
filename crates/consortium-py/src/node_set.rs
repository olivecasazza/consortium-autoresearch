//! PyO3 wrappers for consortium::node_set.

use pyo3::exceptions::{PyException, PyValueError};
use pyo3::prelude::*;

/// Python-visible NodeSet class.
#[pyclass(name = "NodeSet")]
#[derive(Debug, Clone)]
pub struct PyNodeSet {
    inner: consortium::node_set::NodeSet,
}

pyo3::create_exception!(ClusterShell.NodeSet, NodeSetException, PyException);
pyo3::create_exception!(ClusterShell.NodeSet, NodeSetError, NodeSetException);
pyo3::create_exception!(ClusterShell.NodeSet, NodeSetExternalError, NodeSetError);
// NodeSetParseError stays on PyValueError: it predates this hierarchy, and
// re-basing it would break any caller doing `except ValueError`. Upstream
// derives it from NodeSetError instead; that divergence is left for the
// NodeSet port to settle, not smuggled in here.
pyo3::create_exception!(ClusterShell.NodeSet, NodeSetParseError, PyValueError);

#[pymethods]
impl PyNodeSet {
    #[new]
    #[pyo3(signature = (pattern=None))]
    fn new(pattern: Option<&str>) -> PyResult<Self> {
        match pattern {
            Some(p) => {
                let inner = consortium::node_set::NodeSet::parse(p)
                    .map_err(|e| NodeSetParseError::new_err(e.to_string()))?;
                Ok(Self { inner })
            }
            None => Ok(Self {
                inner: consortium::node_set::NodeSet::new(),
            }),
        }
    }

    fn __str__(&self) -> String {
        self.inner.to_string()
    }

    fn __repr__(&self) -> String {
        format!("NodeSet(\"{}\")", self.inner)
    }

    fn __len__(&self) -> usize {
        self.inner.len()
    }

    fn __contains__(&self, node: &str) -> bool {
        self.inner.contains(node)
    }

    fn __or__(&self, other: &PyNodeSet) -> Self {
        Self {
            inner: self.inner.union(&other.inner),
        }
    }

    fn __and__(&self, other: &PyNodeSet) -> Self {
        Self {
            inner: self.inner.intersection(&other.inner),
        }
    }

    fn __sub__(&self, other: &PyNodeSet) -> Self {
        Self {
            inner: self.inner.difference(&other.inner),
        }
    }

    fn __xor__(&self, other: &PyNodeSet) -> Self {
        Self {
            inner: self.inner.symmetric_difference(&other.inner),
        }
    }
}

/// Register node_set types into the parent module.
pub fn register(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    parent.add_class::<PyNodeSet>()?;
    // The exception hierarchy was declared but never added to the module, so
    // `from ClusterShell.NodeSet import NodeSetExternalError` raised
    // ImportError. ClusterShell.CLI.Error imports exactly that name, which is
    // why all five CLI test modules died at collection under the Rust backend.
    // NodeSetParseError was also unreachable, so the NodeSet shim's
    // `from ClusterShell._consortium import NodeSet, NodeSetParseError` probe
    // failed and silently degraded to the pure-Python NodeSet.
    for (name, ty) in [
        (
            "NodeSetException",
            parent.py().get_type_bound::<NodeSetException>(),
        ),
        ("NodeSetError", parent.py().get_type_bound::<NodeSetError>()),
        (
            "NodeSetExternalError",
            parent.py().get_type_bound::<NodeSetExternalError>(),
        ),
        (
            "NodeSetParseError",
            parent.py().get_type_bound::<NodeSetParseError>(),
        ),
    ] {
        parent.add(name, ty)?;
    }
    Ok(())
}
