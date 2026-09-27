//! Node name sets with bracket-based range expansion.
//!
//! Rust implementation of `ClusterShell.NodeSet`.
//!
//! Supports bracket notation like `node[1-10,15]`, set operations
//! (union `,`, difference `!`, intersection `&`, symmetric difference `^`),
//! and automatic folding for compact display.

use crate::range_set::RangeSet;
use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use thiserror::Error;

// ============================================================================
// Errors
// ============================================================================

#[derive(Debug, Error)]
pub enum NodeSetError {
    #[error("parse error: {msg} (in \"{part}\")")]
    ParseError { part: String, msg: String },

    #[error("range error: {0}")]
    RangeError(#[from] crate::range_set::RangeSetError),

    #[error("external error: {0}")]
    ExternalError(String),
}

pub type Result<T> = std::result::Result<T, NodeSetError>;

// ============================================================================
// Internal: NodeSetBase
// ============================================================================

/// Internal representation: maps pattern templates (with `%s` placeholders for
/// range indices) to their `RangeSet` (or `None` for non-indexed nodes).
///
/// Examples:
///  - `"node%s"` → `Some(RangeSet{1,2,3})` represents `node1`, `node2`, `node3`
///  - `"switch"` → `None` represents a single non-indexed node `switch`
#[derive(Debug, Clone)]
struct NodeSetBase {
    patterns: HashMap<String, Option<RangeSet>>,
    autostep: Option<u32>,
}

impl NodeSetBase {
    fn new() -> Self {
        Self {
            patterns: HashMap::new(),
            autostep: None,
        }
    }

    fn with_autostep(autostep: Option<u32>) -> Self {
        Self {
            patterns: HashMap::new(),
            autostep,
        }
    }

    /// Add a pattern with its rangeset. If the pattern already exists, merge.
    fn add(&mut self, pat: &str, rangeset: Option<RangeSet>) {
        if let Some(existing) = self.patterns.get_mut(pat) {
            if let (Some(existing_rs), Some(new_rs)) = (existing.as_mut(), &rangeset) {
                existing_rs.update(new_rs);
            }
            // If existing is None (non-indexed) and we add None, nothing to do.
            // If mixed indexed/non-indexed, keep existing behavior.
        } else {
            self.patterns.insert(pat.to_string(), rangeset);
        }
    }

    fn len(&self) -> usize {
        self.patterns
            .values()
            .map(|rs| match rs {
                Some(rs) => rs.len(),
                None => 1, // non-indexed node
            })
            .sum()
    }

    fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    fn clear(&mut self) {
        self.patterns.clear();
    }

    /// Update (union) with another NodeSetBase.
    fn update(&mut self, other: &NodeSetBase) {
        for (pat, rangeset) in &other.patterns {
            self.add(pat, rangeset.clone());
        }
    }

    /// Intersection update: keep only elements in both.
    fn intersection_update(&mut self, other: &NodeSetBase) {
        let mut result = HashMap::new();
        for (pat, other_rs) in &other.patterns {
            if let Some(self_rs) = self.patterns.get(pat) {
                match (self_rs, other_rs) {
                    (Some(s), Some(o)) => {
                        let intersection = s.intersection(o);
                        if !intersection.is_empty() {
                            result.insert(pat.clone(), Some(intersection));
                        }
                    }
                    (None, None) => {
                        // Both are non-indexed, keep it
                        result.insert(pat.clone(), None);
                    }
                    _ => {} // mixed indexed/non-indexed: no intersection
                }
            }
        }
        self.patterns = result;
    }

    /// Difference update: remove elements in other from self.
    fn difference_update(&mut self, other: &NodeSetBase) {
        let mut purge = Vec::new();
        for (pat, other_rs) in &other.patterns {
            if let Some(self_rs) = self.patterns.get_mut(pat) {
                match (self_rs, other_rs) {
                    (Some(s), Some(o)) => {
                        s.difference_update(o);
                        if s.is_empty() {
                            purge.push(pat.clone());
                        }
                    }
                    (None, None) => {
                        purge.push(pat.clone());
                    }
                    _ => {} // mixed: no removal
                }
            }
        }
        for pat in purge {
            self.patterns.remove(&pat);
        }
    }

    /// Symmetric difference update.
    fn symmetric_difference_update(&mut self, other: &NodeSetBase) {
        let mut purge = Vec::new();

        // Process patterns in self against other
        for (pat, self_rs) in self.patterns.iter_mut() {
            if let Some(other_rs) = other.patterns.get(pat) {
                match (self_rs.as_mut(), other_rs) {
                    (Some(s), Some(o)) => {
                        s.symmetric_difference_update(o);
                        if s.is_empty() {
                            purge.push(pat.clone());
                        }
                    }
                    (None, None) => {
                        // Both non-indexed: xor removes
                        purge.push(pat.clone());
                    }
                    _ => {}
                }
            }
        }

        // Add patterns in other but not in self
        for (pat, other_rs) in &other.patterns {
            if !self.patterns.contains_key(pat) {
                self.add(pat, other_rs.clone());
            }
        }

        for pat in purge {
            self.patterns.remove(&pat);
        }
    }

    /// Check if a node is contained.
    ///
    /// Strategy: parse the node into (pattern, digit_strings), then look
    /// up by matching stored patterns. For multi-dimensional stored patterns
    /// (expanded), we also try the fully-expanded form.
    fn contains(&self, node: &str) -> bool {
        let (pat, indices) = parse_node_indices(node);

        // Try exact pattern match first
        if let Some(rs_opt) = self.patterns.get(&pat) {
            return match (rs_opt, indices.as_slice()) {
                (Some(rs), &[(_, _)]) => {
                    // Use string-based lookup to handle padding correctly
                    let digit_str = extract_last_digit_str(node);
                    rs.contains_str(&digit_str)
                }
                (None, &[]) => true,
                _ => false,
            };
        }

        // For multi-dimensional patterns that were expanded during parsing,
        // the stored patterns have fewer %s placeholders. Try substituting
        // leading indices to find a match.
        if indices.len() > 1 {
            // Extract digit strings from the node
            let digit_strs = extract_all_digit_strs(node);
            // Try substituting first N-1 indices into the pattern to match stored keys
            for n_sub in 1..indices.len() {
                let mut candidate_pat = pat.clone();
                for digit_str in digit_strs.iter().take(n_sub) {
                    candidate_pat = candidate_pat.replacen("%s", digit_str, 1);
                }
                if let Some(Some(rs)) = self.patterns.get(&candidate_pat) {
                    // Check last digit against the rangeset
                    let last_digit = &digit_strs[digit_strs.len() - 1];
                    if rs.contains_str(last_digit) {
                        return true;
                    }
                }
            }
        }

        false
    }

    /// Iterate all individual node names in sorted order.
    fn iter(&self) -> Vec<String> {
        let mut nodes = Vec::new();
        for (pat, rs_opt) in &self.patterns {
            match rs_opt {
                Some(rs) => {
                    for s in rs.striter() {
                        nodes.push(pat.replacen("%s", &s, 1));
                    }
                }
                None => {
                    nodes.push(pat.clone());
                }
            }
        }
        nodes.sort();
        nodes
    }
}

impl fmt::Display for NodeSetBase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts: Vec<String> = Vec::new();
        // Sort patterns for deterministic output
        let mut sorted_pats: Vec<_> = self.patterns.iter().collect();
        sorted_pats.sort_by(|a, b| a.0.cmp(b.0));

        for (pat, rs_opt) in sorted_pats {
            match rs_opt {
                Some(rs) => {
                    let rs_str = format!("{}", rs);
                    if rs.len() == 1 {
                        // Single element: no brackets
                        parts.push(pat.replacen("%s", &rs_str, 1));
                    } else {
                        // Multiple elements: brackets
                        let bracketed = format!("[{}]", rs_str);
                        parts.push(pat.replacen("%s", &bracketed, 1));
                    }
                }
                None => {
                    parts.push(pat.clone());
                }
            }
        }
        write!(f, "{}", parts.join(","))
    }
}

// ============================================================================
// Parsing helpers
// ============================================================================

/// Scan a single node name like `node003` into (pattern, optional int value).
/// Returns (pattern_with_%s, Option<integer_value>).
#[allow(dead_code)]
fn scan_node_single(node: &str) -> (String, Option<i64>) {
    // Split into alternating non-digit/digit segments
    // e.g., "node003" -> [("node", "003")]
    // e.g., "rack2-node05" -> [("rack", "2"), ("-node", "05")]
    // e.g., "switch" -> [("switch", "")]

    let mut pat = String::new();
    let mut has_index = false;
    let mut last_value: Option<i64> = None;

    let chars: Vec<char> = node.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // Consume non-digit prefix
        let prefix_start = i;
        while i < len && !chars[i].is_ascii_digit() {
            i += 1;
        }
        let prefix = &node[prefix_start..i];

        // Consume digit suffix
        let digit_start = i;
        while i < len && chars[i].is_ascii_digit() {
            i += 1;
        }
        let digits = &node[digit_start..i];

        if digits.is_empty() {
            pat.push_str(prefix);
        } else {
            pat.push_str(prefix);
            pat.push_str("%s");
            has_index = true;
            last_value = Some(digits.parse().unwrap_or(0));
        }
    }

    if has_index {
        (pat, last_value)
    } else {
        (pat, None)
    }
}

/// Parse a single node into (pattern, Vec<(value, pad)>).
/// Handles multi-dimensional indices like "rack2-node05".
fn parse_node_indices(node: &str) -> (String, Vec<(i64, u32)>) {
    let mut pat = String::new();
    let mut indices = Vec::new();

    let chars: Vec<char> = node.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        let prefix_start = i;
        while i < len && !chars[i].is_ascii_digit() {
            i += 1;
        }
        pat.push_str(&node[prefix_start..i]);

        let digit_start = i;
        while i < len && chars[i].is_ascii_digit() {
            i += 1;
        }
        let digits = &node[digit_start..i];

        if !digits.is_empty() {
            pat.push_str("%s");
            let value: i64 = digits.parse().unwrap_or(0);
            let pad = if value != 0 {
                let stripped = digits.trim_start_matches('0');
                if digits.len() > stripped.len() {
                    digits.len() as u32
                } else {
                    0
                }
            } else if digits.len() > 1 {
                digits.len() as u32
            } else {
                0
            };
            indices.push((value, pad));
        }
    }

    (pat, indices)
}

/// Parse a nodeset string, handling brackets and operators.
///
/// Supports:
///  - Simple nodes: `node1`, `switch`
///  - Bracket ranges: `node[1-10]`, `node[1,3,5-8]`
///  - Multiple patterns: `web[1-3],db[1-2]`
///  - Operators: `!` (difference), `&` (intersection), `^` (xor)
///  - Leading/trailing digit amendment around brackets
fn parse_nodeset_string(input: &str, autostep: Option<u32>) -> Result<NodeSetBase> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(NodeSetBase::with_autostep(autostep));
    }

    let mut result = NodeSetBase::with_autostep(autostep);
    let mut current_op = Op::Update; // Default operation is union

    // Tokenize into segments separated by operators (, ! & ^)
    // But operators inside brackets should be ignored
    let tokens = tokenize(input)?;

    for token in tokens {
        match token {
            Token::Operator(op) => {
                current_op = op;
            }
            Token::Pattern(s) => {
                let parsed = parse_single_pattern(&s, autostep)?;
                match current_op {
                    Op::Update => result.update(&parsed),
                    Op::Difference => result.difference_update(&parsed),
                    Op::Intersection => result.intersection_update(&parsed),
                    Op::SymmetricDifference => result.symmetric_difference_update(&parsed),
                }
            }
        }
    }

    Ok(result)
}

#[derive(Debug, Clone, Copy)]
enum Op {
    Update,
    Difference,
    Intersection,
    SymmetricDifference,
}

#[derive(Debug)]
enum Token {
    Operator(Op),
    Pattern(String),
}

/// Tokenize a nodeset string, respecting brackets.
fn tokenize(input: &str) -> Result<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut bracket_depth = 0;
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];
        match ch {
            '[' => {
                bracket_depth += 1;
                current.push(ch);
            }
            ']' => {
                if bracket_depth == 0 {
                    return Err(NodeSetError::ParseError {
                        part: input.to_string(),
                        msg: "illegal closing bracket".to_string(),
                    });
                }
                bracket_depth -= 1;
                current.push(ch);
            }
            ',' | '!' | '&' | '^' if bracket_depth == 0 => {
                // This is an operator
                if !current.is_empty() {
                    tokens.push(Token::Pattern(current.trim().to_string()));
                    current = String::new();
                }
                let op = match ch {
                    ',' => Op::Update,
                    '!' => Op::Difference,
                    '&' => Op::Intersection,
                    '^' => Op::SymmetricDifference,
                    _ => unreachable!(),
                };
                tokens.push(Token::Operator(op));
            }
            _ => {
                current.push(ch);
            }
        }
        i += 1;
    }

    if bracket_depth != 0 {
        return Err(NodeSetError::ParseError {
            part: input.to_string(),
            msg: "missing bracket".to_string(),
        });
    }

    if !current.is_empty() {
        tokens.push(Token::Pattern(current.trim().to_string()));
    }

    // Validate: first token should be a pattern (or empty input)
    // Insert an implicit Update at the beginning if first is a pattern
    let mut result = Vec::new();
    let mut expect_pattern = true;
    for token in tokens {
        match &token {
            Token::Pattern(_) => {
                if expect_pattern {
                    if result.is_empty() {
                        // First pattern: implicit Update
                        result.push(Token::Operator(Op::Update));
                    }
                    result.push(token);
                    expect_pattern = false;
                } else {
                    // Two patterns in a row without operator — shouldn't happen
                    // with our tokenizer since comma is an operator
                    result.push(Token::Operator(Op::Update));
                    result.push(token);
                    expect_pattern = false;
                }
            }
            Token::Operator(_) => {
                result.push(token);
                expect_pattern = true;
            }
        }
    }

    Ok(result)
}

/// Parse a single pattern segment (no operators at this level).
/// Handles brackets and plain node names.
fn parse_single_pattern(pat: &str, autostep: Option<u32>) -> Result<NodeSetBase> {
    let mut base = NodeSetBase::with_autostep(autostep);

    if pat.contains('[') {
        // Bracket notation: e.g., "node[1-10]", "rack[1-2]-node[1-5]"
        parse_bracketed(pat, autostep, &mut base)?;
    } else {
        // Plain node name like "node1" or "switch"
        let (pattern, indices) = parse_node_indices(pat);
        if indices.is_empty() {
            base.add(&pattern, None);
        } else {
            // Build a rangeset from the single index values
            let mut rs = RangeSet::new();
            rs.set_autostep(autostep);
            // For single-dimensional simple nodes
            if indices.len() == 1 {
                let (val, pad) = indices[0];
                rs.add_int(val, pad);
                base.add(&pattern, Some(rs));
            } else {
                // Multi-dimensional: expand leading indices into the pattern,
                // keeping only the last dimension as the rangeset.
                // e.g., "192.168.1.29" → pattern "%s.%s.%s.%s", indices [(192,0),(168,0),(1,0),(29,0)]
                //   → expanded pattern "192.168.1.%s", rangeset {29}
                let digit_strs = extract_all_digit_strs(pat);
                let mut expanded_pat = pattern.clone();
                for ds in &digit_strs[..digit_strs.len() - 1] {
                    expanded_pat = expanded_pat.replacen("%s", ds, 1);
                }
                let (val, pad) = indices[indices.len() - 1];
                rs.add_int(val, pad);
                base.add(&expanded_pat, Some(rs));
            }
        }
    }

    Ok(base)
}

/// Parse bracketed notation: "node[1-10,15]", "rack[1-2]-node[1-5]"
fn parse_bracketed(pat: &str, autostep: Option<u32>, base: &mut NodeSetBase) -> Result<()> {
    // Split into prefix[range]suffix segments
    let mut remaining = pat;
    let mut template = String::new();
    let mut rangesets: Vec<RangeSet> = Vec::new();

    while let Some(bracket_start) = remaining.find('[') {
        let prefix = &remaining[..bracket_start];

        // Handle leading digits amendment:
        // "node10[1-3]" → prefix="node10", amend range to "101-103"
        let (clean_prefix, leading_digits) = split_trailing_digits(prefix);
        template.push_str(clean_prefix);
        template.push_str("%s");

        let after_bracket = &remaining[bracket_start + 1..];
        let bracket_end = after_bracket
            .find(']')
            .ok_or_else(|| NodeSetError::ParseError {
                part: pat.to_string(),
                msg: "missing bracket".to_string(),
            })?;

        let range_str = &after_bracket[..bracket_end];
        remaining = &after_bracket[bracket_end + 1..];

        // Handle trailing digits amendment:
        // "node[1-3]5" → suffix starts with "5", amend range
        let (trailing_digits, clean_remaining) = split_leading_digits(remaining);

        // Amend the range string if we have leading/trailing digits
        let amended_range = amend_range(range_str, &leading_digits, &trailing_digits)?;

        let rs = RangeSet::parse(&amended_range, autostep)?;
        rangesets.push(rs);

        remaining = clean_remaining;
    }

    // Append any remaining suffix
    if !remaining.is_empty() {
        // Check for trailing node-like content (non-digit)
        template.push_str(remaining);
    }

    // For single-dimensional case
    if rangesets.len() == 1 {
        base.add(&template, Some(rangesets.into_iter().next().unwrap()));
    } else if rangesets.is_empty() {
        base.add(&template, None);
    } else {
        // Multi-dimensional: expand all combinations
        // e.g., rack[1-2]-node[1-3] → 6 nodes
        expand_multi_dim(&template, &rangesets, base);
    }

    Ok(())
}

/// Expand multi-dimensional bracket notation into individual patterns.
fn expand_multi_dim(template: &str, rangesets: &[RangeSet], base: &mut NodeSetBase) {
    // For multi-dimensional, we expand all but the last dimension
    // and create individual patterns with the last dimension as the rangeset.
    if rangesets.is_empty() {
        return;
    }

    if rangesets.len() == 1 {
        base.add(template, Some(rangesets[0].clone()));
        return;
    }

    // Collect all values from each dimension except the last
    let last_rs = &rangesets[rangesets.len() - 1];
    let prefixes = expand_dimensions(&rangesets[..rangesets.len() - 1]);

    // For each combination of prefix dimensions, create a pattern
    for prefix_vals in &prefixes {
        let mut pat = template.to_string();
        for val in prefix_vals {
            pat = pat.replacen("%s", val, 1);
        }
        // pat now has one %s left for the last dimension
        base.add(&pat, Some(last_rs.clone()));
    }
}

/// Expand multiple RangeSets into all combinations of their string values.
fn expand_dimensions(rangesets: &[RangeSet]) -> Vec<Vec<String>> {
    if rangesets.is_empty() {
        return vec![vec![]];
    }

    let first_vals: Vec<String> = rangesets[0].striter().collect();
    let rest = expand_dimensions(&rangesets[1..]);

    let mut result = Vec::new();
    for val in &first_vals {
        for suffix in &rest {
            let mut combined = vec![val.clone()];
            combined.extend(suffix.iter().cloned());
            result.push(combined);
        }
    }
    result
}

/// Extract the last run of digits from a node string, preserving padding.
/// e.g., "node001" → "001", "rack2-node05" → "05"
fn extract_last_digit_str(node: &str) -> String {
    let bytes = node.as_bytes();
    let mut end = bytes.len();
    while end > 0 && !bytes[end - 1].is_ascii_digit() {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && bytes[start - 1].is_ascii_digit() {
        start -= 1;
    }
    node[start..end].to_string()
}

/// Extract all runs of digits from a node string, preserving padding.
/// e.g., "rack2-node05" → ["2", "05"]
fn extract_all_digit_strs(node: &str) -> Vec<String> {
    let mut result = Vec::new();
    let chars: Vec<char> = node.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // Skip non-digits
        while i < len && !chars[i].is_ascii_digit() {
            i += 1;
        }
        let start = i;
        while i < len && chars[i].is_ascii_digit() {
            i += 1;
        }
        if i > start {
            result.push(node[start..i].to_string());
        }
    }
    result
}

/// Split trailing digits from a prefix: "node10" → ("node", "10")
fn split_trailing_digits(s: &str) -> (&str, String) {
    let bytes = s.as_bytes();
    let mut i = bytes.len();
    while i > 0 && bytes[i - 1].is_ascii_digit() {
        i -= 1;
    }
    (&s[..i], s[i..].to_string())
}

/// Split leading digits from remaining: "5-ilo" → ("5", "-ilo")
fn split_leading_digits(s: &str) -> (String, &str) {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    (s[..i].to_string(), &s[i..])
}

/// Amend a range string with leading/trailing digits.
///
/// Leading digits: "10" + "[1-3]" → "101-103"
/// Trailing digits: "[1-3]" + "5" → "15-35/10"
fn amend_range(range: &str, leading: &str, trailing: &str) -> Result<String> {
    let mut result = range.to_string();

    if !leading.is_empty() {
        // Prepend leading digits to each bound
        // "1-3,5-7" with leading "10" → "101-103,105-107"
        let rs = RangeSet::parse(&result, None)?;
        let mut parts = Vec::new();
        // Get contiguous sub-ranges
        for val_str in rs.striter() {
            let combined = format!("{}{}", leading, val_str);
            parts.push(combined);
        }
        // Re-parse the expanded list
        result = parts.join(",");
    }

    if !trailing.is_empty() {
        // Append trailing digits to each bound
        let rs = RangeSet::parse(&result, None)?;
        let mut parts = Vec::new();
        let trail_len = trailing.len();
        let power = 10i64.pow(trail_len as u32);
        let trail_val: i64 = trailing.parse().unwrap_or(0);

        for val in rs.intiter() {
            let combined = val * power + trail_val;
            parts.push(format!("{}", combined));
        }
        result = parts.join(",");
    }

    Ok(result)
}

// ============================================================================
// Public API: NodeSet
// ============================================================================

/// A set of cluster node names supporting range notation like `node[1-100]`.
///
/// # Examples
///
/// ```
/// use consortium::node_set::NodeSet;
///
/// let ns = NodeSet::parse("node[1-5,10]").unwrap();
/// assert_eq!(ns.len(), 6);
/// assert!(ns.contains("node3"));
/// assert!(!ns.contains("node7"));
/// ```
#[derive(Debug, Clone)]
pub struct NodeSet {
    base: NodeSetBase,
}

impl NodeSet {
    /// Create an empty NodeSet.
    pub fn new() -> Self {
        Self {
            base: NodeSetBase::new(),
        }
    }

    /// Parse a node set from a string like `"node[1-10,15],other[001-050]"`.
    pub fn parse(pattern: &str) -> Result<Self> {
        let base = parse_nodeset_string(pattern, None)?;
        Ok(Self { base })
    }

    /// Parse with explicit autostep setting.
    pub fn parse_with_autostep(pattern: &str, autostep: Option<u32>) -> Result<Self> {
        let base = parse_nodeset_string(pattern, autostep)?;
        Ok(Self { base })
    }

    /// Number of individual nodes in the set.
    pub fn len(&self) -> usize {
        self.base.len()
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.base.is_empty()
    }

    /// Check if a specific node name is in the set.
    pub fn contains(&self, node: &str) -> bool {
        self.base.contains(node)
    }

    /// Update (union) this nodeset with another.
    pub fn update(&mut self, other: &NodeSet) {
        self.base.update(&other.base);
    }

    /// Update from a string pattern.
    pub fn update_str(&mut self, pattern: &str) -> Result<()> {
        let other = parse_nodeset_string(pattern, self.base.autostep)?;
        self.base.update(&other);
        Ok(())
    }

    /// Return the union of this nodeset and another.
    pub fn union(&self, other: &NodeSet) -> NodeSet {
        let mut copy = self.clone();
        copy.update(other);
        copy
    }

    /// Intersection update: keep only elements in both.
    pub fn intersection_update(&mut self, other: &NodeSet) {
        self.base.intersection_update(&other.base);
    }

    /// Return the intersection of this nodeset and another.
    pub fn intersection(&self, other: &NodeSet) -> NodeSet {
        let mut copy = self.clone();
        copy.intersection_update(other);
        copy
    }

    /// Difference update: remove elements of other from self.
    pub fn difference_update(&mut self, other: &NodeSet) {
        self.base.difference_update(&other.base);
    }

    /// Return the difference of this nodeset and another.
    pub fn difference(&self, other: &NodeSet) -> NodeSet {
        let mut copy = self.clone();
        copy.difference_update(other);
        copy
    }

    /// Symmetric difference update.
    pub fn symmetric_difference_update(&mut self, other: &NodeSet) {
        self.base.symmetric_difference_update(&other.base);
    }

    /// Return the symmetric difference of this nodeset and another.
    pub fn symmetric_difference(&self, other: &NodeSet) -> NodeSet {
        let mut copy = self.clone();
        copy.symmetric_difference_update(other);
        copy
    }

    /// Clear all nodes from this set.
    pub fn clear(&mut self) {
        self.base.clear();
    }

    /// Iterate all individual node names in sorted order.
    pub fn iter(&self) -> impl Iterator<Item = String> + '_ {
        self.base.iter().into_iter()
    }

    /// Split the nodeset into `nbr` sub-nodesets (at most).
    pub fn split(&self, nbr: usize) -> Vec<NodeSet> {
        assert!(nbr > 0);
        let all_nodes: Vec<String> = self.iter().collect();
        let total = all_nodes.len();
        let slice_size = total / nbr;
        let leftover = total % nbr;

        let mut result = Vec::new();
        let mut begin = 0;

        for i in 0..nbr.min(total) {
            let length = slice_size + if i < leftover { 1 } else { 0 };
            let slice = &all_nodes[begin..begin + length];
            let pattern = slice.join(",");
            if let Ok(ns) = NodeSet::parse(&pattern) {
                result.push(ns);
            }
            begin += length;
        }

        result
    }

    /// Check if this nodeset is a subset of another.
    pub fn is_subset(&self, other: &NodeSet) -> bool {
        self.difference(other).is_empty()
    }

    /// Check if this nodeset is a superset of another.
    pub fn is_superset(&self, other: &NodeSet) -> bool {
        other.is_subset(self)
    }
}

impl Default for NodeSet {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for NodeSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.base)
    }
}

impl FromStr for NodeSet {
    type Err = NodeSetError;

    fn from_str(s: &str) -> Result<Self> {
        NodeSet::parse(s)
    }
}

// ============================================================================
// Convenience functions
// ============================================================================

/// Expand a nodeset pattern into a list of individual node names.
///
/// ```
/// use consortium::node_set::expand;
/// let nodes = expand("node[1-3]").unwrap();
/// assert_eq!(nodes, vec!["node1", "node2", "node3"]);
/// ```
pub fn expand(pat: &str) -> Result<Vec<String>> {
    let ns = NodeSet::parse(pat)?;
    Ok(ns.iter().collect())
}

/// Fold a nodeset pattern: remove duplicates and compress ranges.
///
/// ```
/// use consortium::node_set::fold;
/// let s = fold("node3,node1,node2").unwrap();
/// assert_eq!(s, "node[1-3]");
/// ```
pub fn fold(pat: &str) -> Result<String> {
    let ns = NodeSet::parse(pat)?;
    Ok(format!("{}", ns))
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_is_empty() {
        let ns = NodeSet::new();
        assert!(ns.is_empty());
        assert_eq!(ns.len(), 0);
    }

    #[test]
    fn test_parse_simple_node() {
        let ns = NodeSet::parse("node1").unwrap();
        assert_eq!(ns.len(), 1);
        assert!(ns.contains("node1"));
        assert!(!ns.contains("node2"));
    }

    #[test]
    fn test_parse_range() {
        let ns = NodeSet::parse("node[1-5]").unwrap();
        assert_eq!(ns.len(), 5);
        for i in 1..=5 {
            assert!(ns.contains(&format!("node{}", i)));
        }
        assert!(!ns.contains("node0"));
        assert!(!ns.contains("node6"));
    }

    #[test]
    fn test_parse_multiple_ranges() {
        let ns = NodeSet::parse("node[1-3,7-9]").unwrap();
        assert_eq!(ns.len(), 6);
        assert!(ns.contains("node1"));
        assert!(ns.contains("node3"));
        assert!(!ns.contains("node4"));
        assert!(ns.contains("node7"));
        assert!(ns.contains("node9"));
    }

    #[test]
    fn test_parse_padded() {
        let ns = NodeSet::parse("node[001-003]").unwrap();
        assert_eq!(ns.len(), 3);
        assert!(ns.contains("node001"));
        assert!(ns.contains("node002"));
        assert!(ns.contains("node003"));
        let display = format!("{}", ns);
        assert!(display.contains("001"));
    }

    #[test]
    fn test_parse_comma_separated() {
        let ns = NodeSet::parse("node1,node2,node3").unwrap();
        assert_eq!(ns.len(), 3);
        assert!(ns.contains("node1"));
        assert!(ns.contains("node2"));
        assert!(ns.contains("node3"));
    }

    #[test]
    fn test_parse_difference_operator() {
        let ns = NodeSet::parse("node[1-10]!node[5-7]").unwrap();
        assert_eq!(ns.len(), 7);
        assert!(ns.contains("node1"));
        assert!(ns.contains("node4"));
        assert!(!ns.contains("node5"));
        assert!(!ns.contains("node6"));
        assert!(!ns.contains("node7"));
        assert!(ns.contains("node8"));
        assert!(ns.contains("node10"));
    }

    #[test]
    fn test_parse_intersection_operator() {
        let ns = NodeSet::parse("node[1-10]&node[5-15]").unwrap();
        assert_eq!(ns.len(), 6);
        for i in 5..=10 {
            assert!(ns.contains(&format!("node{}", i)));
        }
        assert!(!ns.contains("node1"));
        assert!(!ns.contains("node11"));
    }

    #[test]
    fn test_parse_xor_operator() {
        let ns = NodeSet::parse("node[1-5]^node[3-7]").unwrap();
        assert_eq!(ns.len(), 4);
        assert!(ns.contains("node1"));
        assert!(ns.contains("node2"));
        assert!(!ns.contains("node3"));
        assert!(!ns.contains("node5"));
        assert!(ns.contains("node6"));
        assert!(ns.contains("node7"));
    }

    #[test]
    fn test_multi_pattern() {
        let ns = NodeSet::parse("web[1-3],db[1-2]").unwrap();
        assert_eq!(ns.len(), 5);
        assert!(ns.contains("web1"));
        assert!(ns.contains("web3"));
        assert!(ns.contains("db1"));
        assert!(ns.contains("db2"));
    }

    #[test]
    fn test_display_fold() {
        let ns = NodeSet::parse("node3,node1,node2").unwrap();
        let display = format!("{}", ns);
        assert_eq!(display, "node[1-3]");
    }

    #[test]
    fn test_set_union() {
        let a = NodeSet::parse("node[1-3]").unwrap();
        let b = NodeSet::parse("node[5-7]").unwrap();
        let c = a.union(&b);
        assert_eq!(c.len(), 6);
    }

    #[test]
    fn test_set_intersection() {
        let a = NodeSet::parse("node[1-10]").unwrap();
        let b = NodeSet::parse("node[5-15]").unwrap();
        let c = a.intersection(&b);
        assert_eq!(c.len(), 6);
    }

    #[test]
    fn test_set_difference() {
        let a = NodeSet::parse("node[1-10]").unwrap();
        let b = NodeSet::parse("node[5-7]").unwrap();
        let c = a.difference(&b);
        assert_eq!(c.len(), 7);
    }

    #[test]
    fn test_set_symmetric_difference() {
        let a = NodeSet::parse("node[1-5]").unwrap();
        let b = NodeSet::parse("node[3-7]").unwrap();
        let c = a.symmetric_difference(&b);
        assert_eq!(c.len(), 4);
    }

    #[test]
    fn test_iter_sorted() {
        let ns = NodeSet::parse("node[3,1,2]").unwrap();
        let nodes: Vec<String> = ns.iter().collect();
        assert_eq!(nodes, vec!["node1", "node2", "node3"]);
    }

    #[test]
    fn test_non_indexed_node() {
        let ns = NodeSet::parse("switch").unwrap();
        assert_eq!(ns.len(), 1);
        assert!(ns.contains("switch"));
    }

    #[test]
    fn test_expand_function() {
        let nodes = expand("node[1-3]").unwrap();
        assert_eq!(nodes, vec!["node1", "node2", "node3"]);
    }

    #[test]
    fn test_fold_function() {
        let s = fold("node3,node1,node2").unwrap();
        assert_eq!(s, "node[1-3]");
    }

    #[test]
    fn test_from_str() {
        let ns: NodeSet = "node[1-3]".parse().unwrap();
        assert_eq!(ns.len(), 3);
    }

    #[test]
    fn test_update_str() {
        let mut ns = NodeSet::parse("node[1-3]").unwrap();
        ns.update_str("node[5-7]").unwrap();
        assert_eq!(ns.len(), 6);
    }

    #[test]
    fn test_split() {
        let ns = NodeSet::parse("node[1-6]").unwrap();
        let parts = ns.split(3);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].len(), 2);
        assert_eq!(parts[1].len(), 2);
        assert_eq!(parts[2].len(), 2);
    }

    #[test]
    fn test_subset_superset() {
        let a = NodeSet::parse("node[1-5]").unwrap();
        let b = NodeSet::parse("node[1-10]").unwrap();
        assert!(a.is_subset(&b));
        assert!(b.is_superset(&a));
        assert!(!b.is_subset(&a));
    }

    #[test]
    fn test_multi_dimensional() {
        let ns = NodeSet::parse("rack[1-2]-node[1-3]").unwrap();
        assert_eq!(ns.len(), 6);
        assert!(ns.contains("rack1-node1"));
        assert!(ns.contains("rack1-node3"));
        assert!(ns.contains("rack2-node1"));
        assert!(ns.contains("rack2-node3"));
        assert!(!ns.contains("rack3-node1"));
    }

    #[test]
    fn test_empty_parse() {
        let ns = NodeSet::parse("").unwrap();
        assert!(ns.is_empty());
    }

    #[test]
    fn test_missing_bracket_error() {
        let result = NodeSet::parse("node[1-3");
        assert!(result.is_err());
    }

    #[test]
    fn test_clear() {
        let mut ns = NodeSet::parse("node[1-5]").unwrap();
        assert!(!ns.is_empty());
        ns.clear();
        assert!(ns.is_empty());
    }

    // ── Dotted hostname / IP address tests ──────────────────────────────

    #[test]
    fn test_dotted_hostname_single() {
        let ns = NodeSet::parse("server1.example.com").unwrap();
        assert_eq!(ns.len(), 1);
        let nodes: Vec<String> = ns.iter().collect();
        assert_eq!(nodes, vec!["server1.example.com"]);
        assert_eq!(format!("{ns}"), "server1.example.com");
    }

    #[test]
    fn test_dotted_hostname_multiple() {
        let ns = NodeSet::parse("server1.local,server2.local").unwrap();
        assert_eq!(ns.len(), 2);
        let nodes: Vec<String> = ns.iter().collect();
        assert_eq!(nodes, vec!["server1.local", "server2.local"]);
    }

    #[test]
    fn test_ip_address_single() {
        let ns = NodeSet::parse("192.168.1.29").unwrap();
        assert_eq!(ns.len(), 1);
        let nodes: Vec<String> = ns.iter().collect();
        assert_eq!(nodes, vec!["192.168.1.29"]);
        assert_eq!(format!("{ns}"), "192.168.1.29");
    }

    #[test]
    fn test_ip_address_multiple_same_subnet() {
        let ns = NodeSet::parse("192.168.1.3,192.168.1.29").unwrap();
        assert_eq!(ns.len(), 2);
        let mut nodes: Vec<String> = ns.iter().collect();
        nodes.sort();
        assert_eq!(nodes, vec!["192.168.1.29", "192.168.1.3"]);
        assert_eq!(format!("{ns}"), "192.168.1.[3,29]");
    }

    #[test]
    fn test_ip_address_different_subnets() {
        let ns = NodeSet::parse("10.0.0.1,192.168.1.1").unwrap();
        assert_eq!(ns.len(), 2);
        let mut nodes: Vec<String> = ns.iter().collect();
        nodes.sort();
        assert_eq!(nodes, vec!["10.0.0.1", "192.168.1.1"]);
    }

    #[test]
    fn test_ip_address_contains() {
        let ns = NodeSet::parse("192.168.1.3,192.168.1.29").unwrap();
        assert!(ns.contains("192.168.1.3"));
        assert!(ns.contains("192.168.1.29"));
        assert!(!ns.contains("192.168.1.4"));
        assert!(!ns.contains("10.0.0.1"));
    }

    #[test]
    fn test_dotted_hostname_with_numbers() {
        // Hostname like "node1.rack2.dc3" has numbers in each component
        let ns = NodeSet::parse("node1.rack2.dc3").unwrap();
        assert_eq!(ns.len(), 1);
        let nodes: Vec<String> = ns.iter().collect();
        assert_eq!(nodes, vec!["node1.rack2.dc3"]);
    }

    #[test]
    fn test_mdns_hostname() {
        let ns = NodeSet::parse("GN9CFLM92K-MBP.local").unwrap();
        assert_eq!(ns.len(), 1);
        let nodes: Vec<String> = ns.iter().collect();
        assert_eq!(nodes, vec!["GN9CFLM92K-MBP.local"]);
    }

    #[test]
    fn test_mdns_hostname_comma_list() {
        let ns = NodeSet::parse("CK2Q9LN7PM-MBA.local,GJHC5VVN49-MBP.local,GN9CFLM92K-MBP.local")
            .unwrap();
        assert_eq!(ns.len(), 3);
        let nodes: Vec<String> = ns.iter().collect();
        assert!(nodes.contains(&"CK2Q9LN7PM-MBA.local".to_string()));
        assert!(nodes.contains(&"GJHC5VVN49-MBP.local".to_string()));
        assert!(nodes.contains(&"GN9CFLM92K-MBP.local".to_string()));
    }

    #[test]
    fn test_thunderbolt_hostname() {
        let ns = NodeSet::parse("CK2Q9LN7PM-MBA.tb").unwrap();
        assert_eq!(ns.len(), 1);
        let nodes: Vec<String> = ns.iter().collect();
        assert_eq!(nodes, vec!["CK2Q9LN7PM-MBA.tb"]);
    }
}
