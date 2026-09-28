//! Numeric range sets with folding and autostep support.
//!
//! This module is the Rust implementation of `ClusterShell.RangeSet`.
//!
//! A [`RangeSet`] manages a set of non-negative integers as a union of
//! string-represented values, with optional step detection (autostep) for
//! compact string representation (e.g. `"0-8/2"`).
//!
//! Internally, like Python's ClusterShell, the set stores string representations
//! of integers, preserving zero-padding per element (e.g. "001", "02", "3").

use std::collections::BTreeSet;
use std::fmt;
use thiserror::Error;

/// Special constant used to disable autostep feature.
pub const AUTOSTEP_DISABLED: f64 = 1e100;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum RangeSetError {
    #[error("parse error: {msg} (in \"{part}\")")]
    ParseError { part: String, msg: String },

    #[error("padding mismatch: {msg} (in \"{part}\")")]
    PaddingError { part: String, msg: String },
}

pub type Result<T> = std::result::Result<T, RangeSetError>;

// ---------------------------------------------------------------------------
// Sorting key
// ---------------------------------------------------------------------------

/// Sort key matching Python's RangeSet._sorted():
/// - Negative numbers (starting with '-'): sort by (-len, int_value)
/// - Non-negative: sort by (len, string_value)
fn sort_key(s: &str) -> (i64, i64, String) {
    if s.starts_with('-') {
        let ival: i64 = s.parse().unwrap_or(0);
        (-(s.len() as i64), ival, String::new())
    } else {
        (s.len() as i64, 0, s.to_string())
    }
}

fn sorted_elements(elements: &BTreeSet<String>) -> Vec<String> {
    let mut v: Vec<String> = elements.iter().cloned().collect();
    v.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
    v
}

// ---------------------------------------------------------------------------
// Slice representation (used internally for display)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct RangeSlice {
    start: i64,
    stop: i64, // exclusive
    step: i64,
    pad: usize,
}

// ---------------------------------------------------------------------------
// RangeSet
// ---------------------------------------------------------------------------

/// An ordered set of integer string representations, supporting cluster
/// range notation like `"1-10,15,20-30/2"`.
#[derive(Debug, Clone)]
pub struct RangeSet {
    /// Internal storage: string representations of integers, preserving padding.
    elements: BTreeSet<String>,
    /// Autostep threshold (internal representation = user_value - 1).
    /// When >= AUTOSTEP_DISABLED, stepping is off.
    _autostep: f64,
}

impl RangeSet {
    /// Create an empty `RangeSet`.
    pub fn new() -> Self {
        Self {
            elements: BTreeSet::new(),
            _autostep: AUTOSTEP_DISABLED,
        }
    }

    /// Parse a range set from a string pattern like `"1-4,6,8-12/2"`.
    ///
    /// `autostep` is the user-facing autostep threshold (minimum element count
    /// to trigger step folding). Pass `None` to disable.
    pub fn parse(pattern: &str, autostep: Option<u32>) -> Result<Self> {
        let mut rs = Self::new();
        rs.set_autostep(autostep);
        rs._parse(pattern)?;
        Ok(rs)
    }

    /// Internal parse: comma-separated subranges.
    fn _parse(&mut self, pattern: &str) -> Result<()> {
        for subrange in pattern.split(',') {
            let subrange = subrange.trim();

            let (baserange, step) = if subrange.contains('/') {
                let parts: Vec<&str> = subrange.splitn(2, '/').collect();
                // Python's int() tolerates surrounding whitespace, so trim here too
                let step: i64 = parts[1]
                    .trim()
                    .parse()
                    .map_err(|_| RangeSetError::ParseError {
                        part: subrange.to_string(),
                        msg: "cannot convert string to integer".to_string(),
                    })?;
                (parts[0], step)
            } else {
                (subrange, 1i64)
            };

            // Parse begin-end part of range, handling negative numbers.
            let (begin_str, end_str, begin_sign, end_sign) =
                self.parse_range_part(baserange, subrange, step)?;

            // Compute padding
            let (start, stop, pad) = self.compute_padding(&begin_str, &end_str, subrange)?;

            // Validate
            if pad > 0 && begin_sign < 0 {
                return Err(RangeSetError::ParseError {
                    part: subrange.to_string(),
                    msg: "padding not supported in negative ranges".to_string(),
                });
            }
            if stop > 1e100 as i64 || start * begin_sign > stop * end_sign || step < 1 {
                return Err(RangeSetError::ParseError {
                    part: subrange.to_string(),
                    msg: "invalid values in range".to_string(),
                });
            }

            self.add_range(start * begin_sign, stop * end_sign + 1, step, pad as u32);
        }
        Ok(())
    }

    /// Parse the begin-end part of a range, handling negative numbers.
    fn parse_range_part<'a>(
        &self,
        baserange: &'a str,
        subrange: &str,
        step: i64,
    ) -> Result<(String, String, i64, i64)> {
        if !baserange.contains('-') {
            // Single value
            if step != 1 {
                return Err(RangeSetError::ParseError {
                    part: subrange.to_string(),
                    msg: "invalid step usage".to_string(),
                });
            }
            return Ok((baserange.to_string(), baserange.to_string(), 1, 1));
        }

        // Has a dash — could be a range or a negative number
        let parts: Vec<&str> = baserange.split('-').collect();
        match parts.len() {
            2 => {
                // "begin-end"
                let begin = parts[0].trim().to_string();
                let end = parts[1].trim().to_string();
                if begin.is_empty() {
                    // "-5" → single negative number
                    if end.is_empty() {
                        return Err(RangeSetError::ParseError {
                            part: subrange.to_string(),
                            msg: "cannot convert string to integer".to_string(),
                        });
                    }
                    return Ok((end.clone(), end, -1, -1));
                }
                Ok((begin, end, 1, 1))
            }
            3 => {
                // "-begin-end" (negative start)
                let begin = parts[1].trim().to_string();
                let end = parts[2].trim().to_string();
                Ok((begin, end, -1, 1))
            }
            4 => {
                // "-begin--end" (both negative)
                let begin = parts[1].trim().to_string();
                let end = parts[3].trim().to_string();
                Ok((begin, end, -1, -1))
            }
            _ => Err(RangeSetError::ParseError {
                part: subrange.to_string(),
                msg: "cannot convert string to integer".to_string(),
            }),
        }
    }

    /// Compute padding and numeric start/stop from begin/end strings.
    fn compute_padding(&self, begin: &str, end: &str, subrange: &str) -> Result<(i64, i64, usize)> {
        let mut pad: usize = 0;

        // Parse begin
        let begin_int: i64 = begin.parse().map_err(|_| {
            if subrange.is_empty() {
                RangeSetError::ParseError {
                    part: subrange.to_string(),
                    msg: "empty range".to_string(),
                }
            } else {
                RangeSetError::ParseError {
                    part: subrange.to_string(),
                    msg: "cannot convert string to integer".to_string(),
                }
            }
        })?;

        if begin_int != 0 {
            let begins = begin.trim_start_matches('0');
            if begin.len() - begins.len() > 0 {
                pad = begin.len();
            }
        } else if begin.len() > 1 {
            pad = begin.len();
        }

        // Parse end
        let end_trimmed = if end.parse::<i64>().unwrap_or(1) != 0 {
            end.trim_start_matches('0')
        } else {
            end
        };

        let mut endpad: usize = 0;
        if end.len() - end_trimmed.len() > 0 {
            endpad = end.len();
        }

        // Check padding consistency
        if (pad > 0 || endpad > 0) && begin.len() != end.len() {
            return Err(RangeSetError::ParseError {
                part: subrange.to_string(),
                msg: "padding length mismatch".to_string(),
            });
        }

        let end_int: i64 = end_trimmed.parse().map_err(|_| RangeSetError::ParseError {
            part: subrange.to_string(),
            msg: "cannot convert string to integer".to_string(),
        })?;

        Ok((begin_int, end_int, pad))
    }

    /// Add a range of integers [start, stop) with given step and padding.
    pub fn add_range(&mut self, start: i64, stop: i64, step: i64, pad: u32) {
        assert!(start < stop, "please provide ordered node index ranges");
        assert!(step > 0);
        assert!(stop - start < 1_000_000_000, "range too large");

        let mut i = start;
        while i < stop {
            if pad == 0 {
                self.elements.insert(format!("{}", i));
            } else {
                self.elements
                    .insert(format!("{:0>width$}", i, width = pad as usize));
            }
            i += step;
        }
    }

    /// Number of individual values in the set.
    pub fn len(&self) -> usize {
        self.elements.len()
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.elements.is_empty()
    }

    /// Clear all elements from this set.
    pub fn clear(&mut self) {
        self.elements.clear();
    }

    /// Build a set from an ordered iterable of node expressions.
    ///
    /// Mirrors `RangeSet.fromlist()`: every item is parsed as range notation,
    /// so `"5-8"` contributes the four elements `5,6,7,8`. Items that fail to
    /// parse are reported through [`RangeSetError::ParseError`].
    pub fn from_strings<I, S>(items: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut rs = Self::new();
        for item in items {
            rs._parse(item.as_ref())?;
        }
        Ok(rs)
    }

    /// Build a set from a half-open integer range `[start, stop)`.
    ///
    /// Mirrors `RangeSet(range(start, stop))`.
    pub fn from_range(start: i64, stop: i64) -> Self {
        let mut rs = Self::new();
        if start < stop {
            rs.add_range(start, stop, 1, 0);
        }
        rs
    }

    /// Add a string element directly.
    pub fn add_str(&mut self, s: &str) {
        self.elements.insert(s.to_string());
    }

    /// Add an integer element with optional zero-padding.
    pub fn add_int(&mut self, value: i64, pad: u32) {
        if pad == 0 {
            self.elements.insert(format!("{}", value));
        } else {
            self.elements
                .insert(format!("{:0>width$}", value, width = pad as usize));
        }
    }

    /// Remove an element by string. Panics if not present.
    pub fn remove_str(&mut self, s: &str) {
        if !self.elements.remove(s) {
            panic!("element '{}' not found in RangeSet", s);
        }
    }

    /// Remove an element by string if present (no error).
    pub fn discard_str(&mut self, s: &str) {
        self.elements.remove(s);
    }

    /// Check if a string element is in the set.
    pub fn contains_str(&self, s: &str) -> bool {
        self.elements.contains(s)
    }

    /// Check if an integer (unpadded) is in the set.
    pub fn contains_int(&self, value: i64) -> bool {
        self.elements.contains(&format!("{}", value))
    }

    /// Get sorted elements as owned strings.
    pub fn sorted(&self) -> Vec<String> {
        sorted_elements(&self.elements)
    }

    /// Iterate as strings (owned).
    pub fn striter(&self) -> impl Iterator<Item = String> {
        sorted_elements(&self.elements).into_iter()
    }

    /// Iterate as integer values.
    pub fn intiter(&self) -> impl Iterator<Item = i64> {
        sorted_elements(&self.elements)
            .into_iter()
            .map(|s| s.parse::<i64>().unwrap_or(0))
    }

    /// Access the element at `index` in sorted order, as `RangeSet.__getitem__`
    /// does for an integer index. Negative indices count from the end.
    ///
    /// Returns `None` where Python raises `IndexError`.
    pub fn get(&self, index: isize) -> Option<String> {
        let sorted = sorted_elements(&self.elements);
        let len = sorted.len();
        let idx = if index < 0 {
            len as isize + index
        } else {
            index
        };
        if idx < 0 || idx >= len as isize {
            return None;
        }
        Some(sorted[idx as usize].clone())
    }

    /// Sub-set of the elements in sorted-order range `[start, stop)`.
    ///
    /// Mirrors `RangeSet.__getitem__` for a slice: out-of-range indices are
    /// clamped, and the result inherits this set's autostep setting.
    pub fn slice(&self, start: isize, stop: isize) -> RangeSet {
        let sorted = sorted_elements(&self.elements);
        let len = sorted.len() as isize;
        let clamp = |i: isize| -> usize {
            let i = if i < 0 { len + i } else { i };
            i.clamp(0, len) as usize
        };
        let mut result = RangeSet::new();
        result.set_autostep(self.autostep());
        let (begin, end) = (clamp(start), clamp(stop));
        if begin >= end {
            return result;
        }
        for elem in &sorted[begin..end] {
            result.add_str(elem);
        }
        result
    }

    /// Split the set into at most `nbr` sub-sets of near-equal size.
    ///
    /// Mirrors `RangeSet.split()`.
    pub fn split(&self, nbr: usize) -> Vec<RangeSet> {
        assert!(nbr > 0);
        let len = self.len();
        let slice_size = len / nbr;
        let leftover = len % nbr;

        let mut result = Vec::new();
        let mut begin = 0;
        for i in 0..nbr.min(len) {
            let length = slice_size + usize::from(i < leftover);
            result.push(self.slice(begin as isize, (begin + length) as isize));
            begin += length;
        }
        result
    }

    /// Number of dimensions of this set: 1 when non-empty, 0 when empty.
    pub fn dim(&self) -> usize {
        usize::from(!self.is_empty())
    }

    // -----------------------------------------------------------------------
    // Set operations
    // -----------------------------------------------------------------------

    /// Return the union of two range sets.
    pub fn union(&self, other: &RangeSet) -> RangeSet {
        let mut result = self.clone();
        result.update(other);
        result
    }

    /// Return the intersection of two range sets.
    pub fn intersection(&self, other: &RangeSet) -> RangeSet {
        let mut result = self.clone();
        result.intersection_update(other);
        result
    }

    /// Return the difference (self - other).
    pub fn difference(&self, other: &RangeSet) -> RangeSet {
        let mut result = self.clone();
        result.difference_update(other);
        result
    }

    /// Return the symmetric difference.
    pub fn symmetric_difference(&self, other: &RangeSet) -> RangeSet {
        let mut result = self.clone();
        result.symmetric_difference_update(other);
        result
    }

    /// Update self to be the union with other.
    pub fn update(&mut self, other: &RangeSet) {
        for elem in &other.elements {
            self.elements.insert(elem.clone());
        }
    }

    /// Update self to be the intersection with other.
    pub fn intersection_update(&mut self, other: &RangeSet) {
        self.elements = self
            .elements
            .intersection(&other.elements)
            .cloned()
            .collect();
    }

    /// Update self to be self - other.
    pub fn difference_update(&mut self, other: &RangeSet) {
        self.elements = self.elements.difference(&other.elements).cloned().collect();
    }

    /// Update self to be the symmetric difference with other.
    pub fn symmetric_difference_update(&mut self, other: &RangeSet) {
        self.elements = self
            .elements
            .symmetric_difference(&other.elements)
            .cloned()
            .collect();
    }

    /// Check if this set is a subset of another.
    pub fn is_subset(&self, other: &RangeSet) -> bool {
        self.difference(other).is_empty()
    }

    /// Check if this set is a superset of another.
    pub fn is_superset(&self, other: &RangeSet) -> bool {
        other.is_subset(self)
    }

    // -----------------------------------------------------------------------
    // Autostep
    // -----------------------------------------------------------------------

    /// Get autostep value (user-facing: minimum node count, or None if disabled).
    pub fn autostep(&self) -> Option<u32> {
        if self._autostep >= AUTOSTEP_DISABLED {
            None
        } else {
            Some((self._autostep as u32) + 1)
        }
    }

    /// Set autostep value (user-facing: minimum node count, or None to disable).
    pub fn set_autostep(&mut self, val: Option<u32>) {
        match val {
            None => self._autostep = AUTOSTEP_DISABLED,
            Some(v) => self._autostep = (v as f64) - 1.0,
        }
    }

    // -----------------------------------------------------------------------
    // Display internals: _slices_padding, _strslices
    // -----------------------------------------------------------------------

    /// Iterator over (RangeSlice, padding) pairs, implementing Python's
    /// `_slices_padding(autostep)`.
    fn slices_padding(&self, autostep: f64) -> Vec<RangeSlice> {
        let sorted = sorted_elements(&self.elements);
        if sorted.is_empty() {
            return Vec::new();
        }

        let mut result: Vec<RangeSlice> = Vec::new();

        let mut cur_pad: usize = 0;
        let mut cur_padded: bool = false;
        let mut cur_start: Option<i64> = None;
        let mut cur_step: Option<i64> = None;
        let mut last_idx: i64 = 0;

        for si in &sorted {
            let idx: i64 = si.parse().unwrap_or(0);
            let digitlen = si.len();
            let padded = digitlen > 1 && si.starts_with('0');

            if let Some(cs) = cur_start {
                // Check for padding mismatch
                let padding_mismatch = if cur_padded {
                    digitlen != cur_pad
                } else {
                    padded
                };

                // Check for step mismatch
                let step_mismatch = if let Some(cstep) = cur_step {
                    cstep != idx - last_idx
                } else {
                    false
                };

                if padding_mismatch || step_mismatch {
                    let (stepped, step) = if let Some(cstep) = cur_step {
                        let stepped =
                            (cstep == 1) || ((last_idx - cs) as f64 >= autostep * cstep as f64);
                        (stepped, cstep)
                    } else {
                        (true, 1i64)
                    };

                    if stepped {
                        result.push(RangeSlice {
                            start: cs,
                            stop: last_idx + 1,
                            step,
                            pad: if cur_padded { cur_pad } else { 0 },
                        });
                        cur_start = Some(idx);
                        cur_padded = padded;
                        cur_pad = digitlen;
                    } else {
                        if padding_mismatch {
                            let stop = last_idx + 1;
                            let mut j = cs;
                            while j < stop {
                                result.push(RangeSlice {
                                    start: j,
                                    stop: j + 1,
                                    step: 1,
                                    pad: if cur_padded { cur_pad } else { 0 },
                                });
                                j += step;
                            }
                            cur_start = Some(idx);
                            cur_padded = padded;
                            cur_pad = digitlen;
                        } else {
                            let stop = last_idx - step + 1;
                            let mut j = cs;
                            while j < stop {
                                result.push(RangeSlice {
                                    start: j,
                                    stop: j + 1,
                                    step: 1,
                                    pad: if cur_padded { cur_pad } else { 0 },
                                });
                                j += step;
                            }
                            cur_start = Some(last_idx);
                        }
                    }

                    cur_step = if step_mismatch {
                        Some(idx - last_idx)
                    } else {
                        None
                    };
                    last_idx = idx;
                    continue;
                }
            } else {
                // First element
                cur_padded = padded;
                cur_pad = digitlen;
                cur_start = Some(idx);
                cur_step = None;
                last_idx = idx;
                continue;
            }

            cur_step = Some(idx - last_idx);
            last_idx = idx;
        }

        // Flush remaining
        if let Some(cs) = cur_start {
            if let Some(cstep) = cur_step {
                let stepped = (last_idx - cs) as f64 >= self._autostep * cstep as f64;
                if stepped || cstep == 1 {
                    result.push(RangeSlice {
                        start: cs,
                        stop: last_idx + 1,
                        step: cstep,
                        pad: if cur_padded { cur_pad } else { 0 },
                    });
                } else {
                    let mut j = cs;
                    while j <= last_idx {
                        result.push(RangeSlice {
                            start: j,
                            stop: j + 1,
                            step: 1,
                            pad: if cur_padded { cur_pad } else { 0 },
                        });
                        j += cstep;
                    }
                }
            } else {
                // Single element
                result.push(RangeSlice {
                    start: cs,
                    stop: last_idx + 1,
                    step: 1,
                    pad: if cur_padded { cur_pad } else { 0 },
                });
            }
        }

        result
    }

    /// Folded slices using the instance's autostep setting.
    fn folded_slices(&self) -> Vec<RangeSlice> {
        self.slices_padding(self._autostep)
    }

    /// Format slices as strings.
    fn strslices(&self) -> Vec<String> {
        self.folded_slices()
            .iter()
            .map(|sli| {
                if sli.start + 1 == sli.stop {
                    // Single element
                    if sli.pad > 0 {
                        format!("{:0>width$}", sli.start, width = sli.pad)
                    } else {
                        format!("{}", sli.start)
                    }
                } else if sli.step == 1 {
                    // Contiguous range
                    if sli.pad > 0 {
                        format!(
                            "{:0>width$}-{:0>width$}",
                            sli.start,
                            sli.stop - 1,
                            width = sli.pad
                        )
                    } else {
                        format!("{}-{}", sli.start, sli.stop - 1)
                    }
                } else {
                    // Stepped range
                    if sli.pad > 0 {
                        format!(
                            "{:0>width$}-{:0>width$}/{}",
                            sli.start,
                            sli.stop - 1,
                            sli.step,
                            width = sli.pad
                        )
                    } else {
                        format!("{}-{}/{}", sli.start, sli.stop - 1, sli.step)
                    }
                }
            })
            .collect()
    }

    /// Get the maximum padding value in the set.
    pub fn padding(&self) -> Option<usize> {
        let mut result = None;
        for si in sorted_elements(&self.elements) {
            let digitlen = si.len();
            if digitlen > 1 && si.starts_with('0') {
                result = Some(digitlen);
            }
        }
        result
    }

    /// Reformat every element to the given zero-padding width.
    ///
    /// Mirrors the `RangeSet.padding` property setter; `None` (or a width
    /// that is too small to keep a leading zero) drops the padding.
    pub fn set_padding(&mut self, pad: Option<usize>) {
        let pad = pad.unwrap_or(0);
        self.elements = self
            .elements
            .iter()
            .map(|si| {
                let value: i64 = si.parse().unwrap_or(0);
                if pad == 0 {
                    format!("{}", value)
                } else {
                    format!("{:0>width$}", value, width = pad)
                }
            })
            .collect();
    }

    /// Split the set into maximal contiguous sub-sets.
    ///
    /// Mirrors `RangeSet.contiguous()`, which uses the non-folding slice
    /// iterator, so no autostep step-collapsing happens here.
    pub fn contiguous(&self) -> Vec<RangeSet> {
        self.slices_padding(AUTOSTEP_DISABLED)
            .iter()
            .map(|sli| {
                let mut result = RangeSet::new();
                result.add_range(sli.start, sli.stop, sli.step, sli.pad as u32);
                result
            })
            .collect()
    }

    /// Contiguous/stepped slices as `(start, stop, step)` tuples, with
    /// autostep folding applied. Padding is not carried over.
    ///
    /// Mirrors `RangeSet.slices()`.
    pub fn slices(&self) -> Vec<(i64, i64, i64)> {
        self.folded_slices()
            .iter()
            .map(|sli| (sli.start, sli.stop, sli.step))
            .collect()
    }
}

impl Default for RangeSet {
    fn default() -> Self {
        Self::new()
    }
}

impl PartialEq for RangeSet {
    /// Compare element contents only, like `RangeSet.__eq__`, which is
    /// `len(other) == len(self) and self.issubset(other)`. Autostep is a
    /// display setting and does not take part in equality.
    fn eq(&self, other: &Self) -> bool {
        self.elements == other.elements
    }
}

impl fmt::Display for RangeSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.strslices().join(","))
    }
}

// ---------------------------------------------------------------------------
// RangeSetND (N-dimensional) — stub
// ---------------------------------------------------------------------------

/// N-dimensional range set, used internally by NodeSet for multi-range patterns.
#[derive(Debug, Clone)]
pub struct RangeSetND {
    pub vecrangesets: Vec<Vec<RangeSet>>,
    _autostep: f64,
}

impl RangeSetND {
    pub fn new(autostep: Option<u32>) -> Self {
        Self {
            vecrangesets: Vec::new(),
            _autostep: autostep.map(|v| v as f64).unwrap_or(AUTOSTEP_DISABLED),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_rs(input: &str, expected: &str, expected_len: usize) {
        let rs = RangeSet::parse(input, Some(3)).unwrap();
        assert_eq!(
            rs.to_string(),
            expected,
            "parse '{}' display mismatch",
            input
        );
        assert_eq!(rs.len(), expected_len, "parse '{}' len mismatch", input);
    }

    #[test]
    fn test_simple() {
        test_rs("0", "0", 1);
        test_rs("1", "1", 1);
        test_rs("0-2", "0-2", 3);
        test_rs("1-3", "1-3", 3);
        test_rs("1-3,4-6", "1-6", 6);
        test_rs("1-3,4-6,7-10", "1-10", 10);
    }

    #[test]
    fn test_step_simple() {
        test_rs("0-4/2", "0-4/2", 3);
        test_rs("1-4/2", "1,3", 2);
        test_rs("1-4/3", "1,4", 2);
        test_rs("1-4/4", "1", 1);
    }

    #[test]
    fn test_step_advanced() {
        test_rs("1-4/4,2-6/2", "1-2,4,6", 4);
        test_rs("6-24/6,9-21/6", "6-24/3", 7);
        test_rs("0-24/2,9-21/2", "0-8/2,9-22,24", 20);
        test_rs("0-24/2,9-21/2,100", "0-8/2,9-22,24,100", 21);
        test_rs("3-21/9,6-24/9,9-27/9", "3-27/3", 9);
        test_rs("1-17/2,2-18/2", "1-18", 18);
        test_rs("1-17/01", "1-17", 17);
        test_rs("1-17/02", "1-17/2", 9);
        test_rs("1-17/03", "1-16/3", 6);
    }

    #[test]
    fn test_step_advanced_more() {
        test_rs("0-24/2,9-21/2,100-101", "0-8/2,9-22,24,100-101", 22);
        test_rs("101-121/4,1-225/112", "1,101-121/4,225", 8);
        test_rs("1-32/3,13-28/9", "1-31/3", 11);
        test_rs("1-32/3,13-22/9", "1-31/3", 11);
        test_rs("1-32/3,13-31/9", "1-31/3", 11);
        test_rs("1-32/3,13-40/9", "1-31/3,40", 12);
    }

    #[test]
    fn test_step_more_complex() {
        test_rs("1-16/3,13-28/6", "1-19/3,25", 8);
        test_rs("1-16/3,1-16/6", "1-16/3", 6);
        test_rs("1-16/6,1-16/3", "1-16/3", 6);
        test_rs("1-17/2,33-41/2,2-18/2", "1-18,33-41/2", 23);
        test_rs("1-17/2,33-41/2,2-20/2", "1-18,20,33-41/2", 24);
        test_rs("1-17/2,33-41/2,2-19/2", "1-18,33-41/2", 23);
    }

    #[test]
    fn test_bad_syntax() {
        assert!(RangeSet::parse("", None).is_err());
        assert!(RangeSet::parse("-", None).is_err());
        assert!(RangeSet::parse("A", None).is_err());
        assert!(RangeSet::parse("2-5/a", None).is_err());
        assert!(RangeSet::parse("3/2", None).is_err());
        assert!(RangeSet::parse("3-/2", None).is_err());
        assert!(RangeSet::parse("-/2", None).is_err());
        assert!(RangeSet::parse("4-a/2", None).is_err());
        assert!(RangeSet::parse("4-3/2", None).is_err());
        assert!(RangeSet::parse("4-5/-2", None).is_err());
        assert!(RangeSet::parse("004-002", None).is_err());
    }

    #[test]
    fn test_intersection() {
        let mut r1 = RangeSet::parse("4-34", None).unwrap();
        let r2 = RangeSet::parse("27-42", None).unwrap();
        r1.intersection_update(&r2);
        assert_eq!(r1.to_string(), "27-34");
        assert_eq!(r1.len(), 8);
    }

    #[test]
    fn test_intersection_complex() {
        let mut r1 = RangeSet::parse("2-450,654-700,800", None).unwrap();
        let r2 = RangeSet::parse("500-502,690-820,830-840,900", None).unwrap();
        r1.intersection_update(&r2);
        assert_eq!(r1.to_string(), "690-700,800");
        assert_eq!(r1.len(), 12);
    }

    #[test]
    fn test_intersection_step() {
        let mut r1 = RangeSet::parse("4-34/2", None).unwrap();
        let r2 = RangeSet::parse("28-42/2", None).unwrap();
        r1.intersection_update(&r2);
        assert_eq!(r1.to_string(), "28,30,32,34");
        assert_eq!(r1.len(), 4);

        let mut r1 = RangeSet::parse("4-34/2", None).unwrap();
        let r2 = RangeSet::parse("27-42/2", None).unwrap();
        r1.intersection_update(&r2);
        assert_eq!(r1.to_string(), "");
        assert_eq!(r1.len(), 0);

        let mut r1 = RangeSet::parse("2-60/3", Some(3)).unwrap();
        let r2 = RangeSet::parse("3-50/2", Some(3)).unwrap();
        r1.intersection_update(&r2);
        assert_eq!(r1.to_string(), "5-47/6");
        assert_eq!(r1.len(), 8);
    }

    #[test]
    fn test_difference() {
        let mut r1 = RangeSet::parse("4,7-33", None).unwrap();
        let r2 = RangeSet::parse("8-33", None).unwrap();
        r1.difference_update(&r2);
        assert_eq!(r1.to_string(), "4,7");
        assert_eq!(r1.len(), 2);
    }

    #[test]
    fn test_difference_bounds() {
        let mut r1 = RangeSet::parse("1-10,39-41,50-60", None).unwrap();
        let r2 = RangeSet::parse("1-10,38-39,50-60", None).unwrap();
        r1.difference_update(&r2);
        assert_eq!(r1.len(), 2);
        assert_eq!(r1.to_string(), "40-41");
    }

    #[test]
    fn test_symmetric_difference() {
        let mut r1 = RangeSet::parse("4,7-33", None).unwrap();
        let r2 = RangeSet::parse("8-34", None).unwrap();
        r1.symmetric_difference_update(&r2);
        assert_eq!(r1.to_string(), "4,7,34");
        assert_eq!(r1.len(), 3);
    }

    #[test]
    fn test_symmetric_difference_complex() {
        let mut r1 = RangeSet::parse("5,7,10-12,33-50", None).unwrap();
        let r2 = RangeSet::parse("8-34", None).unwrap();
        r1.symmetric_difference_update(&r2);
        assert_eq!(r1.to_string(), "5,7-9,13-32,35-50");
        assert_eq!(r1.len(), 40);

        let mut r1 = RangeSet::parse("8-30", None).unwrap();
        let r2 = RangeSet::parse("31-40", None).unwrap();
        r1.symmetric_difference_update(&r2);
        assert_eq!(r1.to_string(), "8-40");
        assert_eq!(r1.len(), 33);

        let mut r1 = RangeSet::parse("8-30", None).unwrap();
        let r2 = RangeSet::parse("8-30", None).unwrap();
        r1.symmetric_difference_update(&r2);
        assert_eq!(r1.to_string(), "");
        assert_eq!(r1.len(), 0);
    }

    #[test]
    fn test_union() {
        let r1 = RangeSet::parse("1-10", None).unwrap();
        let r2 = RangeSet::parse("8-20", None).unwrap();
        let r3 = r1.union(&r2);
        assert_eq!(r3.to_string(), "1-20");
        assert_eq!(r3.len(), 20);
    }

    #[test]
    fn test_contains() {
        let r1 = RangeSet::parse("1-100,102,105-242,800", None).unwrap();
        assert_eq!(r1.len(), 240);
        assert!(r1.contains_str("99"));
        assert!(!r1.contains_str("099"));
        assert!(!r1.contains_str("101"));
    }

    #[test]
    fn test_padding() {
        let r1 = RangeSet::parse("001-010", None).unwrap();
        assert_eq!(r1.len(), 10);
        assert!(r1.contains_str("001"));
        assert!(r1.contains_str("010"));
        assert!(!r1.contains_str("1"));
        assert!(!r1.contains_str("10"));

        let r2 = RangeSet::parse("00-02", None).unwrap();
        assert!(r2.contains_str("00"));
        assert!(!r2.contains_str("0"));
        assert!(r2.contains_str("01"));
        assert!(!r2.contains_str("1"));

        let r3 = RangeSet::parse("0003-0143,0360-1000", None).unwrap();
        assert!(!r3.contains_str("360"));
        assert!(r3.contains_str("0360"));
    }

    #[test]
    fn test_padding_display() {
        test_rs("001-010", "001-010", 10);
        test_rs("00-02", "00-02", 3);
    }

    #[test]
    fn test_empty() {
        let rs = RangeSet::new();
        assert!(rs.is_empty());
        assert_eq!(rs.len(), 0);
        assert_eq!(rs.to_string(), "");
    }

    #[test]
    fn test_add_remove() {
        let mut rs = RangeSet::new();
        rs.add_int(5, 0);
        rs.add_int(3, 0);
        rs.add_int(1, 0);
        assert_eq!(rs.len(), 3);
        assert_eq!(rs.to_string(), "1,3,5");
        rs.discard_str("3");
        assert_eq!(rs.len(), 2);
        assert_eq!(rs.to_string(), "1,5");
    }

    #[test]
    fn test_intiter() {
        let rs = RangeSet::parse("3,1,5,10", None).unwrap();
        let vals: Vec<i64> = rs.intiter().collect();
        assert_eq!(vals, vec![1, 3, 5, 10]);
    }

    #[test]
    fn test_large_range() {
        let rs = RangeSet::parse("1-1000", None).unwrap();
        assert_eq!(rs.len(), 1000);
        assert_eq!(rs.to_string(), "1-1000");
    }

    #[test]
    fn test_diff_step() {
        let mut r1 = RangeSet::parse("4-34/2", Some(3)).unwrap();
        let r2 = RangeSet::parse("3-33/2", Some(3)).unwrap();
        r1.difference_update(&r2);
        assert_eq!(r1.to_string(), "4-34/2");
        assert_eq!(r1.len(), 16);

        let mut r1 = RangeSet::parse("4-34/2", Some(3)).unwrap();
        let r2 = RangeSet::parse("2-14/2", Some(3)).unwrap();
        r1.difference_update(&r2);
        assert_eq!(r1.to_string(), "16-34/2");
        assert_eq!(r1.len(), 10);

        let mut r1 = RangeSet::parse("4-34/2", Some(3)).unwrap();
        let r2 = RangeSet::parse("28-52/2", Some(3)).unwrap();
        r1.difference_update(&r2);
        assert_eq!(r1.to_string(), "4-26/2");
        assert_eq!(r1.len(), 12);

        let mut r1 = RangeSet::parse("4-34/2", Some(3)).unwrap();
        let r2 = RangeSet::parse("12-18/2", Some(3)).unwrap();
        r1.difference_update(&r2);
        assert_eq!(r1.to_string(), "4-10/2,20-34/2");
        assert_eq!(r1.len(), 12);
    }

    #[test]
    fn test_diff_step_complex() {
        let mut r1 = RangeSet::parse("1-100", Some(3)).unwrap();
        let r2 = RangeSet::parse("2-98/2", Some(3)).unwrap();
        r1.difference_update(&r2);
        assert_eq!(r1.to_string(), "1-99/2,100");
        assert_eq!(r1.len(), 51);

        let mut r1 = RangeSet::parse("1-1000", Some(3)).unwrap();
        let r2 = RangeSet::parse("2-999/2", Some(3)).unwrap();
        r1.difference_update(&r2);
        assert_eq!(r1.to_string(), "1-999/2,1000");
        assert_eq!(r1.len(), 501);
    }

    #[test]
    fn test_folding() {
        // Python: testFolding
        let r1 = RangeSet::parse(
            "112,114-117,119,121,130,132,134,136,138,139-141,144,147-148",
            Some(6),
        )
        .unwrap();
        assert_eq!(
            r1.to_string(),
            "112,114-117,119,121,130,132,134,136,138-141,144,147-148"
        );

        let r1 = RangeSet::parse(
            "112,114-117,119,121,130,132,134,136,138,139-141,144,147-148",
            Some(5),
        )
        .unwrap();
        assert_eq!(
            r1.to_string(),
            "112,114-117,119,121,130-138/2,139-141,144,147-148"
        );

        let r1 = RangeSet::parse("1,3-4,6,8", None).unwrap();
        assert_eq!(r1.to_string(), "1,3-4,6,8");

        let r1 = RangeSet::parse("1,3-4,6,8", Some(4)).unwrap();
        assert_eq!(r1.to_string(), "1,3-4,6,8");

        let r1 = RangeSet::parse("1,3-4,6,8", Some(2)).unwrap();
        assert_eq!(r1.to_string(), "1-3/2,4,6-8/2");

        let r1 = RangeSet::parse("1,3-4,6,8", Some(3)).unwrap();
        assert_eq!(r1.to_string(), "1,3-4,6,8");
    }

    #[test]
    fn test_mixed_padding() {
        // Python: test_mixed_padding
        let r0 = RangeSet::parse("030-031,032-100/2,101-489", Some(3)).unwrap();
        assert_eq!(r0.to_string(), "030-032,034-100/2,101-489");

        let r1 = RangeSet::parse("030-032,033-100/3,102", Some(3)).unwrap();
        assert_eq!(r1.to_string(), "030-033,036-102/3");

        let r2 = RangeSet::parse("030-032,033-100/3,101", Some(3)).unwrap();
        assert_eq!(r2.to_string(), "030-033,036-099/3,101");

        let r3 = RangeSet::parse("030-032,033-100/3,100", Some(3)).unwrap();
        assert_eq!(r3.to_string(), "030-033,036-099/3,100");
    }

    #[test]
    fn test_mixed_padding_with_different_widths() {
        // Python: test_mixed_padding r5 case
        let r5 = RangeSet::parse("030-032,033-100/3,99-105/3,0001", Some(3)).unwrap();
        assert_eq!(r5.to_string(), "99,030-033,036-105/3,0001");
    }

    #[test]
    fn test_padding_mismatch_errors() {
        // Python: test_mixed_padding_mismatch
        assert!(RangeSet::parse("1-044", None).is_err());
        assert!(RangeSet::parse("01-044", None).is_err());
        assert!(RangeSet::parse("001-44", None).is_err());
        assert!(RangeSet::parse("0-9,1-044", None).is_err());
        assert!(RangeSet::parse("0-9,01-044", None).is_err());
        assert!(RangeSet::parse("0-9,001-44", None).is_err());
        assert!(RangeSet::parse("030-032,033-99/3,100", None).is_err());
    }

    #[test]
    fn test_update_complex() {
        // Python: testUpdate
        let mut r1 = RangeSet::parse("1-100,102,105-242,800", None).unwrap();
        assert_eq!(r1.len(), 240);
        let r2 = RangeSet::parse("243-799,1924-1984", None).unwrap();
        assert_eq!(r2.len(), 618);
        r1.update(&r2);
        assert_eq!(r1.len(), 240 + 618);
        assert_eq!(r1.to_string(), "1-100,102,105-800,1924-1984");
    }

    #[test]
    fn test_union_complex() {
        // Python: testUnion
        let r1 = RangeSet::parse("1-100,102,105-242,800", None).unwrap();
        let r2 = RangeSet::parse("243-799,1924-1984", None).unwrap();
        let r3 = r1.union(&r2);
        assert_eq!(r3.len(), 858);
        assert_eq!(r3.to_string(), "1-100,102,105-800,1924-1984");

        // with overlap
        let r2 = RangeSet::parse("200-799", None).unwrap();
        let r3 = r1.union(&r2);
        assert_eq!(r3.len(), 797);
        assert_eq!(r3.to_string(), "1-100,102,105-800");
    }

    #[test]
    fn test_intersection_length() {
        // Python: testIntersectionLength
        let r1 = RangeSet::parse("115-117,130,166-170,4780-4999", None).unwrap();
        assert_eq!(r1.len(), 229);
        let r2 = RangeSet::parse("116-117,130,4781-4999", None).unwrap();
        assert_eq!(r2.len(), 222);
        let res = r1.intersection(&r2);
        assert_eq!(res.len(), 222);

        let r1 = RangeSet::parse("115-200", None).unwrap();
        assert_eq!(r1.len(), 86);
        let r2 = RangeSet::parse("116-117,119,123-131,133,149,199", None).unwrap();
        assert_eq!(r2.len(), 15);
        let res = r1.intersection(&r2);
        assert_eq!(res.len(), 15);

        // StopIteration test
        let r1 = RangeSet::parse("115-117,130,166-170,4780-4999,5003", None).unwrap();
        assert_eq!(r1.len(), 230);
        let r2 = RangeSet::parse("116-117,130,4781-4999", None).unwrap();
        assert_eq!(r2.len(), 222);
        let res = r1.intersection(&r2);
        assert_eq!(res.len(), 222);

        // StopIteration test2
        let r1 = RangeSet::parse("130,166-170,4780-4999", None).unwrap();
        assert_eq!(r1.len(), 226);
        let r2 = RangeSet::parse("116-117", None).unwrap();
        assert_eq!(r2.len(), 2);
        let res = r1.intersection(&r2);
        assert_eq!(res.len(), 0);
    }

    #[test]
    fn test_ior() {
        // Python: test_ior
        let mut r1 = RangeSet::parse("1,3-9,14-21,30-39,42", None).unwrap();
        let r2 = RangeSet::parse("2-5,10-32,35,40-41", None).unwrap();
        r1.update(&r2);
        assert_eq!(r1.len(), 42);
        assert_eq!(r1.to_string(), "1-42");
    }

    #[test]
    fn test_iand() {
        // Python: test_iand
        let mut r1 = RangeSet::parse("1,3-9,14-21,30-39,42", None).unwrap();
        let r2 = RangeSet::parse("2-5,10-32,35,40-41", None).unwrap();
        r1.intersection_update(&r2);
        assert_eq!(r1.len(), 15);
        assert_eq!(r1.to_string(), "3-5,14-21,30-32,35");
    }

    #[test]
    fn test_ixor() {
        // Python: test_ixor
        let mut r1 = RangeSet::parse("1,3-9,14-21,30-39,42", None).unwrap();
        let r2 = RangeSet::parse("2-5,10-32,35,40-41", None).unwrap();
        r1.symmetric_difference_update(&r2);
        assert_eq!(r1.len(), 27);
        assert_eq!(r1.to_string(), "1-2,6-13,22-29,33-34,36-42");
    }

    #[test]
    fn test_isub() {
        // Python: test_isub
        let mut r1 = RangeSet::parse("1,3-9,14-21,30-39,42", None).unwrap();
        let r2 = RangeSet::parse("2-5,10-32,35,40-41", None).unwrap();
        r1.difference_update(&r2);
        assert_eq!(r1.len(), 12);
        assert_eq!(r1.to_string(), "1,6-9,33-34,36-39,42");
    }

    #[test]
    fn test_add_range_api() {
        // Python: testAddRange
        let mut r1 = RangeSet::new();
        r1.add_range(1, 100, 1, 0);
        assert_eq!(r1.len(), 99);
        assert_eq!(r1.to_string(), "1-99");
        r1.add_range(40, 101, 1, 0);
        assert_eq!(r1.len(), 100);
        assert_eq!(r1.to_string(), "1-100");
        r1.add_range(399, 423, 2, 0);
        assert_eq!(r1.len(), 112);
        assert_eq!(
            r1.to_string(),
            "1-100,399,401,403,405,407,409,411,413,415,417,419,421"
        );

        // With autostep
        let mut r1 = RangeSet::new();
        r1.set_autostep(Some(3));
        r1.add_range(1, 100, 1, 0);
        assert_eq!(r1.autostep(), Some(3));
        assert_eq!(r1.len(), 99);
        assert_eq!(r1.to_string(), "1-99");
        r1.add_range(40, 101, 1, 0);
        assert_eq!(r1.len(), 100);
        assert_eq!(r1.to_string(), "1-100");
        r1.add_range(399, 423, 2, 0);
        assert_eq!(r1.len(), 112);
        assert_eq!(r1.to_string(), "1-100,399-421/2");
    }

    #[test]
    fn test_add_range_bounds() {
        let mut r1 = RangeSet::parse("1-30", Some(2)).unwrap();
        assert_eq!(r1.len(), 30);
        assert_eq!(r1.to_string(), "1-30");
        r1.add_range(32, 35, 1, 0);
        assert_eq!(r1.len(), 33);
        assert_eq!(r1.to_string(), "1-30,32-34");
        r1.add_range(31, 32, 1, 0);
        assert_eq!(r1.len(), 34);
        assert_eq!(r1.to_string(), "1-34");

        let mut r1 = RangeSet::parse("1-30", Some(3)).unwrap();
        r1.add_range(40, 65, 10, 0);
        assert_eq!(r1.autostep(), Some(3));
        assert_eq!(r1.len(), 33);
        assert_eq!(r1.to_string(), "1-30,40-60/10");

        // One
        r1.add_range(103, 104, 1, 0);
        assert_eq!(r1.len(), 34);
        assert_eq!(r1.to_string(), "1-30,40-60/10,103");
    }

    #[test]
    fn test_autostep_property() {
        let mut rs = RangeSet::new();
        assert_eq!(rs.autostep(), None);
        rs.set_autostep(Some(3));
        assert_eq!(rs.autostep(), Some(3));
        rs.set_autostep(None);
        assert_eq!(rs.autostep(), None);
    }

    #[test]
    fn test_padding_property() {
        let r1 = RangeSet::parse("001-010", None).unwrap();
        assert_eq!(r1.padding(), Some(3));

        let r2 = RangeSet::parse("1-10", None).unwrap();
        assert_eq!(r2.padding(), None);

        let r3 = RangeSet::parse("0003-0143,0360-1000", None).unwrap();
        assert_eq!(r3.padding(), Some(4));
    }

    #[test]
    fn test_copy() {
        // Python: testCopy
        let rangeset = RangeSet::parse("115-117,130,166-170,4780-4999", None).unwrap();
        assert_eq!(rangeset.len(), 229);
        assert_eq!(rangeset.to_string(), "115-117,130,166-170,4780-4999");
        let mut r1 = rangeset.clone();
        let mut r2 = rangeset.clone();
        r1.remove_str("166");
        assert_eq!(rangeset.len(), r1.len() + 1);
        assert_eq!(rangeset.to_string(), "115-117,130,166-170,4780-4999");
        assert_eq!(r1.to_string(), "115-117,130,167-170,4780-4999");
        r2.update(&RangeSet::parse("118", None).unwrap());
        assert_eq!(rangeset.len() + 1, r2.len());
        assert_eq!(r2.to_string(), "115-118,130,166-170,4780-4999");
    }

    #[test]
    fn test_remove_and_discard() {
        // Python: testRemove / testDiscard
        let mut r1 = RangeSet::parse("1-100,102,105-242,800", None).unwrap();
        assert_eq!(r1.len(), 240);
        r1.remove_str("100");
        assert_eq!(r1.len(), 239);
        assert_eq!(r1.to_string(), "1-99,102,105-242,800");

        r1.discard_str("101"); // should not panic
        r1.discard_str("105");
        assert_eq!(r1.len(), 238);
        assert_eq!(r1.to_string(), "1-99,102,106-242,800");
    }

    #[test]
    #[should_panic(expected = "not found")]
    fn test_remove_missing_panics() {
        let mut r1 = RangeSet::parse("1-100", None).unwrap();
        r1.remove_str("101");
    }

    // -----------------------------------------------------------------------
    // Ports of upstream ClusterShell tests/RangeSetTest.py
    //
    // Each test below names the Python method it ports in its doc comment so
    // harness/generate_test_mapping.py can pair them up.
    // -----------------------------------------------------------------------

    /// Python: testIsSuperSet
    #[test]
    fn test_is_superset() {
        let r1 = RangeSet::parse("1-100,102,105-242,800", None).unwrap();
        assert_eq!(r1.len(), 240);
        let r2 = RangeSet::parse("3-98,140-199,800", None).unwrap();
        assert_eq!(r2.len(), 157);
        assert!(r1.is_superset(&r1));
        assert!(r1.is_superset(&r2));

        let r2 = RangeSet::parse("3-98,140-199,243,800", None).unwrap();
        assert_eq!(r2.len(), 158);
        assert!(!r1.is_superset(&r2));
    }

    /// Python: testIsSubSet
    #[test]
    fn test_is_subset() {
        let r1 = RangeSet::parse("1-100,102,105-242,800-900/2", Some(3)).unwrap();
        assert!(r1.is_subset(&r1));
        assert!(r1.is_superset(&r1));

        let empty = RangeSet::new();
        assert!(empty.is_subset(&r1));
        assert!(r1.is_superset(&empty));
        assert!(!r1.is_subset(&empty));
        assert!(!empty.is_superset(&r1));

        let r2 = RangeSet::parse("3,800,802,804,888", Some(3)).unwrap();
        assert!(r2.is_subset(&r2));
        assert!(r2.is_subset(&r1));
        assert!(r1.is_superset(&r2));
        assert!(!r1.is_subset(&r2));
        assert!(!r2.is_superset(&r1));

        // fixed in v1.9 where mixed padding is now supported: "001-100" and
        // "1-100" hold different elements even though they are numerically equal
        let r1 = RangeSet::parse("1-100", Some(3)).unwrap();
        let r2 = RangeSet::parse("001-100", Some(3)).unwrap();
        assert!(!r1.is_subset(&r2));
        assert!(r1 != r2);
    }

    /// Python: testGetItem
    #[test]
    fn test_get_item() {
        let r1 = RangeSet::parse("1-100,102,105-242,800", None).unwrap();
        assert_eq!(r1.len(), 240);
        assert_eq!(r1.get(0).as_deref(), Some("1"));
        assert_eq!(r1.get(1).as_deref(), Some("2"));
        assert_eq!(r1.get(2).as_deref(), Some("3"));
        assert_eq!(r1.get(99).as_deref(), Some("100"));
        assert_eq!(r1.get(100).as_deref(), Some("102"));
        assert_eq!(r1.get(101).as_deref(), Some("105"));
        assert_eq!(r1.get(102).as_deref(), Some("106"));
        assert_eq!(r1.get(103).as_deref(), Some("107"));
        assert_eq!(r1.get(237).as_deref(), Some("241"));
        assert_eq!(r1.get(238).as_deref(), Some("242"));
        assert_eq!(r1.get(239).as_deref(), Some("800"));
        // IndexError territory
        assert_eq!(r1.get(240), None);
        assert_eq!(r1.get(241), None);

        // negative indices
        assert_eq!(r1.get(-1).as_deref(), Some("800"));
        assert_eq!(r1.get(-240).as_deref(), Some("1"));
        for n in 1..(r1.len() as isize) {
            assert_eq!(r1.get(-n), r1.get(r1.len() as isize - n));
        }
        assert_eq!(r1.get(-(r1.len() as isize) - 1), None);
        assert_eq!(r1.get(-(r1.len() as isize) - 2), None);

        let r2 = RangeSet::parse("1-37/3,43-52/3,58-67/3,73-100/3,102-106/2", Some(3)).unwrap();
        assert_eq!(r2.len(), 34);
        assert_eq!(r2.get(0).as_deref(), Some("1"));
        assert_eq!(r2.get(1).as_deref(), Some("4"));
        assert_eq!(r2.get(2).as_deref(), Some("7"));
        assert_eq!(r2.get(12).as_deref(), Some("37"));
        assert_eq!(r2.get(13).as_deref(), Some("43"));
        assert_eq!(r2.get(14).as_deref(), Some("46"));
        assert_eq!(r2.get(16).as_deref(), Some("52"));
        assert_eq!(r2.get(17).as_deref(), Some("58"));
        assert_eq!(r2.get(29).as_deref(), Some("97"));
        assert_eq!(r2.get(30).as_deref(), Some("100"));
        assert_eq!(r2.get(31).as_deref(), Some("102"));
        assert_eq!(r2.get(32).as_deref(), Some("104"));
        assert_eq!(r2.get(33).as_deref(), Some("106"));
    }

    /// Python: testGetSlice (the RangeSet.__getitem__ slice behaviour)
    #[test]
    fn test_get_slice() {
        let r0 = RangeSet::parse("1-12", Some(3)).unwrap();
        assert_eq!(r0.slice(0, 3), RangeSet::parse("1-3", Some(3)).unwrap());
        assert_eq!(r0.slice(2, 7), RangeSet::parse("3-7", Some(3)).unwrap());
        // negative start, stop clamped at 0
        assert!(r0.slice(-1, 0).is_empty());
        assert!(r0.slice(-2, 0).is_empty());
        assert!(r0.slice(-11, 0).is_empty());
        assert!(r0.slice(-12, 0).is_empty());
        assert!(r0.slice(-13, 0).is_empty());
        assert!(r0.slice(-1000, 0).is_empty());
        assert_eq!(
            r0.slice(-1, isize::MAX),
            RangeSet::parse("12", Some(3)).unwrap()
        );
        // the slice keeps the parent's autostep, like the Python version
        let parent = RangeSet::parse("1-12", Some(2)).unwrap();
        assert_eq!(parent.slice(0, 4).autostep(), Some(2));
    }

    /// Python: testSplit
    #[test]
    fn test_split() {
        // Empty rangeset
        assert_eq!(RangeSet::new().split(2).len(), 0);
        // Not enough elements
        assert_eq!(
            RangeSet::parse("1", Some(3)).unwrap().split(2),
            vec![RangeSet::parse("1", Some(3)).unwrap()]
        );
        // Exact number of elements
        assert_eq!(
            RangeSet::parse("1-6", Some(3)).unwrap().split(3),
            vec![
                RangeSet::parse("1-2", Some(3)).unwrap(),
                RangeSet::parse("3-4", Some(3)).unwrap(),
                RangeSet::parse("5-6", Some(3)).unwrap(),
            ]
        );
        // More splits than elements: capped at len(self) sub-sets
        for i in [4, 5] {
            assert_eq!(
                RangeSet::parse("0-3", Some(3)).unwrap().split(i),
                vec![
                    RangeSet::parse("0", Some(3)).unwrap(),
                    RangeSet::parse("1", Some(3)).unwrap(),
                    RangeSet::parse("2", Some(3)).unwrap(),
                    RangeSet::parse("3", Some(3)).unwrap(),
                ]
            );
        }
    }

    /// Python: testClear
    #[test]
    fn test_clear() {
        let mut r1 = RangeSet::parse("1-100,102,105-242,800", None).unwrap();
        assert_eq!(r1.len(), 240);
        assert_eq!(r1.to_string(), "1-100,102,105-242,800");
        r1.clear();
        assert_eq!(r1.len(), 0);
        assert_eq!(r1.to_string(), "");
    }

    /// Python: testConstructorIterate
    #[test]
    fn test_constructor_iterate() {
        // from list
        let mut rgs = RangeSet::from_strings(["3", "5", "6", "7", "8", "1"]).unwrap();
        assert_eq!(rgs.to_string(), "1,3,5-8");
        assert_eq!(rgs.len(), 6);
        rgs.add_int(10, 0);
        assert_eq!(rgs.to_string(), "1,3,5-8,10");
        assert_eq!(rgs.len(), 7);

        // from a RangeSet
        let r1 = RangeSet::parse("1,3,5-8", None).unwrap();
        let rgs = RangeSet::from_strings(r1.striter()).unwrap();
        assert_eq!(rgs.to_string(), "1,3,5-8");
        assert_eq!(rgs.len(), 6);
        assert_eq!(rgs, r1);
    }

    /// Python: testFromListConstructor
    #[test]
    fn test_from_list_constructor() {
        let rgs = RangeSet::from_strings(["3", "5-8", "1"]).unwrap();
        assert_eq!(rgs.to_string(), "1,3,5-8");
        assert_eq!(rgs.len(), 6);

        let rgs = RangeSet::from_strings(
            RangeSet::parse("3", None)
                .unwrap()
                .striter()
                .chain(RangeSet::parse("5-8", None).unwrap().striter())
                .chain(RangeSet::parse("1", None).unwrap().striter()),
        )
        .unwrap();
        assert_eq!(rgs.to_string(), "1,3,5-8");
        assert_eq!(rgs.len(), 6);
    }

    /// Python: testIterator
    #[test]
    fn test_iterator() {
        let matches = ["1", "3", "4", "5", "6", "7", "8", "11"];
        let rgs = RangeSet::from_strings(["11", "3", "5-8", "1", "4"]).unwrap();
        let seen: Vec<String> = rgs.striter().collect();
        assert_eq!(seen, matches);

        // with padding: iteration still yields strings, never ints (true since v1.9)
        let matches = ["001", "003", "004", "005", "006", "007", "008", "011"];
        let rgs = RangeSet::from_strings(["011", "003", "005-008", "001", "004"]).unwrap();
        let seen: Vec<String> = rgs.striter().collect();
        assert_eq!(seen, matches);
    }

    /// Python: testStringIterator
    #[test]
    fn test_string_iterator() {
        let matches = ["1", "3", "4", "5", "6", "7", "8", "11"];
        let rgs = RangeSet::from_strings(["11", "3", "5-8", "1", "4"]).unwrap();
        let seen: Vec<String> = rgs.striter().collect();
        assert_eq!(seen, matches);

        // with padding
        let matches = ["001", "003", "004", "005", "006", "007", "008", "011"];
        let rgs = RangeSet::from_strings(["011", "003", "005-008", "001", "004"]).unwrap();
        let seen: Vec<String> = rgs.striter().collect();
        assert_eq!(seen, matches);
    }

    /// Python: testEquality
    #[test]
    fn test_equality() {
        assert_eq!(RangeSet::new(), RangeSet::new());
        let rg1 = RangeSet::parse("1-4", None).unwrap();
        let rg2 = RangeSet::parse("1-4", None).unwrap();
        assert_eq!(rg1, rg2);
        let rg3 = RangeSet::parse("2-5", None).unwrap();
        assert_ne!(rg1, rg3);
        // same elements, different spelling and different autostep
        let rg4 = RangeSet::parse("1,2,3,4", None).unwrap();
        assert_eq!(rg1, rg4);
        let rg4 = RangeSet::parse("1,2,3,4", Some(2)).unwrap();
        assert_eq!(rg1, rg4);
        let rg5 = RangeSet::parse("1,2,4", None).unwrap();
        assert_ne!(rg1, rg5);
    }

    /// Python: testSlices
    #[test]
    fn test_slices() {
        let r1 = RangeSet::new();
        assert_eq!(r1.len(), 0);
        assert_eq!(r1.slices(), vec![]);

        // Without autostep
        let r1 = RangeSet::parse("1-7/2,8-12,3000-3019", None).unwrap();
        assert_eq!(r1.autostep(), None);
        assert_eq!(r1.len(), 29);
        assert_eq!(
            r1.slices(),
            vec![(1, 2, 1), (3, 4, 1), (5, 6, 1), (7, 13, 1), (3000, 3020, 1)]
        );

        // With autostep
        let r1 = RangeSet::parse("1-7/2,8-12,3000-3019", Some(2)).unwrap();
        assert_eq!(r1.len(), 29);
        assert_eq!(r1.autostep(), Some(2));
        assert_eq!(r1.slices(), vec![(1, 8, 2), (8, 13, 1), (3000, 3020, 1)]);
    }

    /// Python: test_contiguous
    #[test]
    fn test_contiguous() {
        let r0 = RangeSet::new();
        assert_eq!(r0.contiguous().len(), 0);
        let r1 = RangeSet::parse("1,3-9,14-21,30-39,42", Some(3)).unwrap();
        let parts: Vec<String> = r1.contiguous().iter().map(|rs| rs.to_string()).collect();
        assert_eq!(parts, ["1", "3-9", "14-21", "30-39", "42"]);
    }

    /// Python: test_dim
    #[test]
    fn test_dim() {
        assert_eq!(RangeSet::new().dim(), 0);
        assert_eq!(RangeSet::parse("1-10,15-20", Some(3)).unwrap().dim(), 1);
    }

    /// Python: test_strip_whitespaces
    #[test]
    fn test_strip_whitespaces() {
        // whitespaces around and inside subranges
        for input in [
            " 1,5,39-42,100",
            "1 ,5,39-42,100",
            "1, 5,39-42,100",
            "1,5 ,39-42,100",
            "1,5, 39-42,100",
            "1,5,39-42 ,100",
            "1,5,39-42, 100",
            "1,5,39-42,100 ",
            " 1 ,5,39-42,100",
            "1 , 5 , 39-42 , 100",
            " 1 , 5 , 39-42 , 100 ",
        ] {
            test_rs(input, "1,5,39-42,100", 7);
        }

        // whitespaces within ranges
        test_rs("1 - 2", "1-2", 2);
        for input in [
            "01 - 02",
            "01- 02",
            "01 -02",
            " 01-02",
            " 01 -02",
            " 01 - 02",
            " 01 - 02 ",
            "01 - 02 ",
            "01- 02 ",
            "01-02 ",
        ] {
            test_rs(input, "01-02", 2);
        }

        // stepped ranges with whitespaces
        for (input, expected, len) in [
            ("0-8/2", "0-8/2", 5),
            ("0-8 /2", "0-8/2", 5),
            ("0-8/ 2", "0-8/2", 5),
            ("0-8 / 2", "0-8/2", 5),
            ("0 -8 / 2", "0-8/2", 5),
            ("0 - 8 / 2", "0-8/2", 5),
            ("00-08/2", "00-08/2", 5),
            ("00-08 /2", "00-08/2", 5),
            ("00-08/ 2", "00-08/2", 5),
            ("00-08 / 2", "00-08/2", 5),
            ("1-7/2", "1-7/2", 4),
            ("1-7 /2", "1-7/2", 4),
            ("1-7/ 2", "1-7/2", 4),
            ("1-7 / 2", "1-7/2", 4),
            ("1 -7 / 2", "1-7/2", 4),
            ("1 - 7 / 2", "1-7/2", 4),
            ("01-07/2", "01-07/2", 4),
            ("01-07 /2", "01-07/2", 4),
            ("01-07/ 2", "01-07/2", 4),
            ("01-07 / 2", "01-07/2", 4),
        ] {
            test_rs(input, expected, len);
        }

        // invalid patterns
        for input in [
            " 0 0",
            " 1 2",
            "0 1",
            "0 1 ",
            "1,5,39-42,10 0",
            "1,5,39-42,12 3,300",
            "1,5,",
            "1,5, ",
            "1,5,, ",
        ] {
            assert!(
                RangeSet::parse(input, Some(3)).is_err(),
                "expected parse error for {:?}",
                input
            );
        }
    }

    /// Python: test_init_ranges
    #[test]
    fn test_init_ranges() {
        let r1 = RangeSet::from_range(5, 7);
        assert_eq!(r1.to_string(), "5-6");
        assert_eq!(r1.len(), 2);
    }

    /// Python: test_init_negative_ranges (GH#515)
    #[test]
    fn test_init_negative_ranges() {
        let r1 = RangeSet::from_range(-1, 1);
        assert_eq!(r1.to_string(), "-1-0");
        assert_eq!(r1.len(), 2);

        let r1 = RangeSet::parse("-1-0", None).unwrap();
        assert_eq!(r1.to_string(), "-1-0");
        assert_eq!(r1.len(), 2);

        let r1 = RangeSet::from_range(-5, 7);
        assert_eq!(r1.to_string(), "-5-6");
        assert_eq!(r1.len(), 12);

        let r1 = RangeSet::parse("-5-6", None).unwrap();
        assert_eq!(r1.to_string(), "-5-6");
        assert_eq!(r1.len(), 12);
    }

    /// Python: test_mixed_padding_mismatch
    #[test]
    fn test_mixed_padding_mismatch() {
        for input in [
            "1-044",
            "01-044",
            "001-44",
            "0-9,1-044",
            "0-9,01-044",
            "0-9,001-44",
            "030-032,033-99/3,100",
        ] {
            assert!(
                RangeSet::parse(input, Some(3)).is_err(),
                "expected padding mismatch error for {:?}",
                input
            );
        }
    }

    /// Python: test_padding_property_compat
    #[test]
    fn test_padding_property_compat() {
        let mut r0 = RangeSet::parse("0-10,15-20", Some(3)).unwrap();
        assert_eq!(r0.padding(), None);
        r0.set_padding(Some(1));
        assert_eq!(r0.padding(), None);
        assert_eq!(r0.to_string(), "0-10,15-20");
        r0.set_padding(Some(2));
        assert_eq!(r0.padding(), Some(2));
        assert_eq!(r0.to_string(), "00-10,15-20");
        r0.set_padding(Some(3));
        assert_eq!(r0.padding(), Some(3));
        assert_eq!(r0.to_string(), "000-010,015-020");
        // reset padding using None is allowed
        r0.set_padding(None);
        assert_eq!(r0.padding(), None);
        assert_eq!(r0.to_string(), "0-10,15-20");
    }

    /// Python: testDiscard
    #[test]
    fn test_discard() {
        let mut r1 = RangeSet::parse("1-100,102,105-242,800", None).unwrap();
        assert_eq!(r1.len(), 240);
        r1.discard_str("100");
        assert_eq!(r1.len(), 239);
        assert_eq!(r1.to_string(), "1-99,102,105-242,800");
        r1.discard_str("101"); // should not raise
        r1.discard_str("105");
        assert_eq!(r1.len(), 238);
        assert_eq!(r1.to_string(), "1-99,102,106-242,800");
        r1.discard_str("foo"); // never present, still a no-op
        assert_eq!(r1.len(), 238);
    }
}
