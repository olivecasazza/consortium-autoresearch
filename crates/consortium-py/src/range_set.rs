//! PyO3 wrappers for consortium::range_set.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

/// Python-visible RangeSet class.
///
/// This wraps `consortium::range_set::RangeSet` and exposes the same API
/// as `ClusterShell.RangeSet.RangeSet`.
#[pyclass(name = "RangeSet")]
#[derive(Debug, Clone)]
pub struct PyRangeSet {
    inner: consortium::range_set::RangeSet,
}

pyo3::create_exception!(ClusterShell.RangeSet, RangeSetParseError, PyValueError);

#[pymethods]
impl PyRangeSet {
    #[new]
    #[pyo3(signature = (pattern=None, autostep=None))]
    fn new(pattern: Option<&str>, autostep: Option<u32>) -> PyResult<Self> {
        match pattern {
            Some(p) => {
                let inner = consortium::range_set::RangeSet::parse(p, autostep)
                    .map_err(|e| RangeSetParseError::new_err(e.to_string()))?;
                Ok(Self { inner })
            }
            None => Ok(Self {
                inner: consortium::range_set::RangeSet::new(),
            }),
        }
    }

    fn __str__(&self) -> String {
        self.inner.to_string()
    }

    fn __repr__(&self) -> String {
        format!("RangeSet(\"{}\")", self.inner)
    }

    fn __len__(&self) -> usize {
        self.inner.len()
    }

    fn __contains__(&self, value: i64) -> bool {
        self.inner.contains_int(value)
    }

    fn __iter__(slf: PyRef<'_, Self>) -> PyResult<Py<RangeSetIterator>> {
        let values: Vec<i64> = slf.inner.intiter().collect();
        Py::new(slf.py(), RangeSetIterator { values, index: 0 })
    }

    fn __or__(&self, other: &PyRangeSet) -> Self {
        Self {
            inner: self.inner.union(&other.inner),
        }
    }

    fn __and__(&self, other: &PyRangeSet) -> Self {
        Self {
            inner: self.inner.intersection(&other.inner),
        }
    }

    fn __sub__(&self, other: &PyRangeSet) -> Self {
        Self {
            inner: self.inner.difference(&other.inner),
        }
    }

    fn __xor__(&self, other: &PyRangeSet) -> Self {
        Self {
            inner: self.inner.symmetric_difference(&other.inner),
        }
    }

    fn union(&self, other: &PyRangeSet) -> Self {
        self.__or__(other)
    }

    fn intersection(&self, other: &PyRangeSet) -> Self {
        self.__and__(other)
    }

    fn difference(&self, other: &PyRangeSet) -> Self {
        self.__sub__(other)
    }

    fn symmetric_difference(&self, other: &PyRangeSet) -> Self {
        self.__xor__(other)
    }

    fn update(&mut self, other: &PyRangeSet) {
        self.inner.update(&other.inner);
    }

    fn intersection_update(&mut self, other: &PyRangeSet) {
        self.inner.intersection_update(&other.inner);
    }

    fn difference_update(&mut self, other: &PyRangeSet) {
        self.inner.difference_update(&other.inner);
    }

    fn symmetric_difference_update(&mut self, other: &PyRangeSet) {
        self.inner.symmetric_difference_update(&other.inner);
    }
}

#[pyclass]
struct RangeSetIterator {
    values: Vec<i64>,
    index: usize,
}

#[pymethods]
impl RangeSetIterator {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&mut self) -> Option<i64> {
        if self.index < self.values.len() {
            let val = self.values[self.index];
            self.index += 1;
            Some(val)
        } else {
            None
        }
    }
}

/// Register range_set types into the parent module.
pub fn register(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    parent.add_class::<PyRangeSet>()?;
    // RangeSetParseError is raised by __new__ but was never added to the
    // module, so `from ClusterShell._consortium import RangeSetParseError`
    // raised ImportError and the RangeSet shim never loaded. An exception that
    // cannot be imported is an exception callers cannot catch.
    parent.add(
        "RangeSetParseError",
        parent.py().get_type_bound::<RangeSetParseError>(),
    )?;
    Ok(())
}
