//! Message aggregation tree.
//!
//! Rust implementation of `ClusterShell.MsgTree`.
//!
//! The MsgTree is a message aggregation tree that groups identical output lines from
//! multiple sources (nodes). It's memory-efficient -- identical message lines from
//! different nodes share tree nodes.

use std::collections::{HashMap, HashSet};

use slotmap::{DefaultKey, SlotMap};

/// Aggregates output lines into a tree structure for deduplication.
#[derive(Debug)]
pub struct MsgTree<K> {
    arena: SlotMap<DefaultKey, MsgTreeNode<K>>,
    root: DefaultKey,
    current: HashMap<K, DefaultKey>, // key -> current node position
    mode: MsgTreeMode,
}

/// Tree node stored in the slotmap arena
#[derive(Debug)]
struct MsgTreeNode<K> {
    /// Child nodes keyed by message line bytes
    children: HashMap<Vec<u8>, DefaultKey>,
    /// Parent node key (None for root)
    parent: Option<DefaultKey>,
    /// Message line for this node (None for root)
    msgline: Option<Vec<u8>>,
    /// Keys associated with this node
    keys: HashSet<K>,
}

impl<K> MsgTreeNode<K> {
    fn new(parent: Option<DefaultKey>, msgline: Option<Vec<u8>>) -> Self {
        MsgTreeNode {
            children: HashMap::new(),
            parent,
            msgline,
            keys: HashSet::new(),
        }
    }
}

/// MsgTree behavior modes
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd)]
pub enum MsgTreeMode {
    /// Defer message processing until walk.
    Defer = 0,
    /// Process messages immediately.
    Shift = 1,
    /// Process messages immediately and keep backtrace of keys.
    Trace = 2,
}

impl<K: Eq + std::hash::Hash + Clone> MsgTree<K> {
    /// Create a new MsgTree with the given mode.
    pub fn new(mode: MsgTreeMode) -> Self {
        let mut arena: SlotMap<DefaultKey, MsgTreeNode<K>> = SlotMap::new();
        let root_key = arena.insert(MsgTreeNode::new(None, None));
        MsgTree {
            arena,
            root: root_key,
            current: HashMap::new(),
            mode,
        }
    }

    /// Get the node from the arena by key
    #[allow(dead_code)]
    fn get_node(&self, key: DefaultKey) -> Option<&MsgTreeNode<K>> {
        self.arena.get(key)
    }

    /// Get the node from the arena by key (mutable)
    #[allow(dead_code)]
    fn get_node_mut(&mut self, key: DefaultKey) -> Option<&mut MsgTreeNode<K>> {
        self.arena.get_mut(key)
    }

    /// Add a message line for the given key.
    pub fn add(&mut self, key: K, msgline: Vec<u8>) {
        // Get current node for this key, or root if not tracked
        let current_node = self.current.get(&key).copied().unwrap_or(self.root);

        // Determine if we should do key shifting based on mode
        let should_shift = self.mode >= MsgTreeMode::Shift;
        let key_to_shift = if should_shift {
            Some(key.clone())
        } else {
            None
        };

        // Walk from current node to find or create the child matching msgline
        let new_node = self._append_to_node(current_node, msgline, key_to_shift);

        // Update the key's current position
        self.current.insert(key, new_node);
    }

    /// Append a message line to a node, creating child if needed and shifting keys
    fn _append_to_node(
        &mut self,
        parent_key: DefaultKey,
        msgline: Vec<u8>,
        key_to_shift: Option<K>,
    ) -> DefaultKey {
        // Check if child with this msgline exists
        {
            let parent_node = self
                .arena
                .get_mut(parent_key)
                .expect("parent node not found");
            if let Some(&child_key) = parent_node.children.get(&msgline) {
                // Child exists, return it
                if let Some(k) = key_to_shift {
                    self._shift_key(parent_key, child_key, k);
                }
                return child_key;
            }
        }

        // Create new child node (borrow self.arena mutably without holding parent_node ref)
        let child_key = self
            .arena
            .insert(MsgTreeNode::new(Some(parent_key), Some(msgline.clone())));

        // Add child to parent (need to borrow again)
        {
            let parent_node = self
                .arena
                .get_mut(parent_key)
                .expect("parent node not found");
            parent_node.children.insert(msgline, child_key);
        }

        // If key shifting is enabled, shift the key
        if let Some(k) = key_to_shift {
            self._shift_key(parent_key, child_key, k);
        }

        child_key
    }

    /// Shift a key from parent to child (MODE_SHIFT behavior)
    fn _shift_key(&mut self, parent_key: DefaultKey, child_key: DefaultKey, key: K) {
        if let Some(parent_node) = self.arena.get_mut(parent_key) {
            parent_node.keys.remove(&key);
        }

        if let Some(child_node) = self.arena.get_mut(child_key) {
            child_node.keys.insert(key);
        }
    }

    /// Update keys in MODE_DEFER - assigns keys to tree elements
    fn _update_keys(&mut self) {
        for (key, node_key) in self.current.iter() {
            if let Some(node) = self.arena.get_mut(*node_key) {
                node.keys.insert(key.clone());
            }
        }
        // MODE_DEFER is no longer valid as keys are now assigned
        self.mode = MsgTreeMode::Shift;
    }

    /// Return the number of keys contained in the MsgTree.
    pub fn len(&self) -> usize {
        self.current.len()
    }

    /// Returns true if the MsgTree contains no keys.
    pub fn is_empty(&self) -> bool {
        self.current.is_empty()
    }

    /// Remove all items from the MsgTree.
    pub fn clear(&mut self) {
        self.current.clear();
        // Clear the arena and recreate root
        self.arena.clear();
        self.root = self.arena.insert(MsgTreeNode::new(None, None));
    }

    /// Return the message for key if key is in the MsgTree.
    pub fn get(&self, key: &K) -> Option<MsgTreeElem<'_, K>> {
        let node_key = self.current.get(key)?;
        Some(MsgTreeElem {
            tree: self,
            node_key: *node_key,
        })
    }

    /// Return an iterator over MsgTree's keys.
    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.current.keys()
    }

    /// Walk the tree and return an iterator over (message, keys) tuples.
    pub fn walk<'a>(&'a mut self, match_filter: Option<&'a dyn Fn(&K) -> bool>) -> WalkIter<'a, K> {
        // In Defer mode, update keys first
        if self.mode == MsgTreeMode::Defer {
            self._update_keys();
        }

        WalkIter::new(self, match_filter)
    }

    /// Walk the tree in trace mode. Only valid in Trace mode.
    pub fn walk_trace<'a>(
        &'a mut self,
        match_filter: Option<&'a dyn Fn(&K) -> bool>,
    ) -> WalkTraceIter<'a, K> {
        assert_eq!(
            self.mode,
            MsgTreeMode::Trace,
            "walk_trace() is only callable in trace mode"
        );

        WalkTraceIter::new(self, match_filter)
    }

    /// Remove keys matching the filter
    pub fn remove(&mut self, match_filter: &dyn Fn(&K) -> bool) {
        // Collect keys to remove
        let keys_to_remove: Vec<K> = self
            .current
            .keys()
            .filter(|k| match_filter(*k))
            .cloned()
            .collect();

        // Remove from arena
        for key in &keys_to_remove {
            if let Some(&node_key) = self.current.get(key) {
                if let Some(node) = self.arena.get_mut(node_key) {
                    node.keys.remove(key);
                }
            }
            self.current.remove(key);
        }
    }
}

/// A message element representing a path from a node to the root.
#[derive(Debug)]
pub struct MsgTreeElem<'a, K> {
    tree: &'a MsgTree<K>,
    node_key: DefaultKey,
}

impl<'a, K> MsgTreeElem<'a, K> {
    /// Get the whole message buffer as bytes.
    pub fn message(&self) -> Vec<u8> {
        let mut lines: Vec<&[u8]> = Vec::new();
        let mut current = Some(self.node_key);

        while let Some(node_key) = current {
            if let Some(node) = self.tree.arena.get(node_key) {
                if let Some(ref msgline) = node.msgline {
                    lines.push(msgline.as_slice());
                }
                current = node.parent;
            } else {
                break;
            }
        }

        // Reverse to get root-to-leaf order
        lines.reverse();

        if lines.is_empty() {
            Vec::new()
        } else {
            let mut result = lines[0].to_vec();
            for line in &lines[1..] {
                result.push(b'\n');
                result.extend_from_slice(line);
            }
            result
        }
    }

    /// Get the message lines as an iterator
    pub fn lines(&self) -> LinesIter<'_, K> {
        LinesIter {
            tree: self.tree,
            current: Some(self.node_key),
        }
    }
}

/// Iterator over message lines
pub struct LinesIter<'a, K> {
    tree: &'a MsgTree<K>,
    current: Option<DefaultKey>,
}

impl<'a, K> Iterator for LinesIter<'a, K> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        let node_key = self.current?;
        if let Some(node) = self.tree.arena.get(node_key) {
            self.current = node.parent;
            node.msgline.as_deref()
        } else {
            None
        }
    }
}

/// Iterator over walk results.
pub struct WalkIter<'a, K> {
    tree: &'a mut MsgTree<K>,
    stack: Vec<DefaultKey>,
    match_filter: Option<&'a dyn Fn(&K) -> bool>,
}

impl<'a, K> WalkIter<'a, K> {
    fn new(tree: &'a mut MsgTree<K>, match_filter: Option<&'a dyn Fn(&K) -> bool>) -> Self {
        let mut stack = Vec::new();
        // Start with root's children (collect into Vec to enable rev())
        if let Some(root_node) = tree.arena.get(tree.root) {
            let mut children: Vec<DefaultKey> = root_node.children.values().copied().collect();
            children.reverse();
            stack.extend(children);
        }
        WalkIter {
            tree,
            stack,
            match_filter,
        }
    }
}

impl<'a, K: Clone> Iterator for WalkIter<'a, K> {
    type Item = (Vec<u8>, Vec<K>);

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(node_key) = self.stack.pop() {
            // Add children to stack
            // Collect into Vec to enable rev()
            if let Some(node) = self.tree.arena.get(node_key) {
                let mut children: Vec<DefaultKey> = node.children.values().copied().collect();
                children.reverse();
                self.stack.extend(children);
            }

            // Check if this node has keys
            if let Some(node) = self.tree.arena.get(node_key) {
                if !node.keys.is_empty() {
                    // Filter keys if needed
                    let filtered_keys: Vec<K> = if let Some(filter) = &self.match_filter {
                        node.keys.iter().filter(|k| filter(*k)).cloned().collect()
                    } else {
                        node.keys.iter().cloned().collect()
                    };

                    if !filtered_keys.is_empty() {
                        // Build message for this node
                        let mut lines: Vec<&[u8]> = Vec::new();
                        let mut current = Some(node_key);

                        while let Some(nk) = current {
                            if let Some(n) = self.tree.arena.get(nk) {
                                if let Some(ref msgline) = n.msgline {
                                    lines.push(msgline.as_slice());
                                }
                                current = n.parent;
                            } else {
                                break;
                            }
                        }

                        lines.reverse();

                        let result = if lines.is_empty() {
                            Vec::new()
                        } else {
                            let mut result = lines[0].to_vec();
                            for line in &lines[1..] {
                                result.push(b'\n');
                                result.extend_from_slice(line);
                            }
                            result
                        };

                        return Some((result, filtered_keys));
                    }
                }
            }
        }
        None
    }
}

/// Iterator over walk_trace results.
pub struct WalkTraceIter<'a, K> {
    tree: &'a mut MsgTree<K>,
    stack: Vec<(DefaultKey, usize)>,
    match_filter: Option<&'a dyn Fn(&K) -> bool>,
}

impl<'a, K> WalkTraceIter<'a, K> {
    fn new(tree: &'a mut MsgTree<K>, match_filter: Option<&'a dyn Fn(&K) -> bool>) -> Self {
        let mut stack = Vec::new();
        // Start with root's children at depth 1 (collect into Vec to enable rev())
        if let Some(root_node) = tree.arena.get(tree.root) {
            let mut children: Vec<(DefaultKey, usize)> =
                root_node.children.iter().map(|(_, &k)| (k, 1)).collect();
            children.reverse();
            stack.extend(children);
        }
        WalkTraceIter {
            tree,
            stack,
            match_filter,
        }
    }
}

impl<'a, K: Clone> Iterator for WalkTraceIter<'a, K> {
    type Item = (Vec<u8>, Vec<K>, usize, usize);

    fn next(&mut self) -> Option<Self::Item> {
        while let Some((node_key, depth)) = self.stack.pop() {
            // Add children to stack
            // Collect into Vec to enable rev()
            if let Some(node) = self.tree.arena.get(node_key) {
                let mut children: Vec<(DefaultKey, usize)> =
                    node.children.iter().map(|(_, &k)| (k, depth + 1)).collect();
                children.reverse();
                self.stack.extend(children);
            }

            // Check if this node has keys
            if let Some(node) = self.tree.arena.get(node_key) {
                if !node.keys.is_empty() {
                    // Filter keys if needed
                    let filtered_keys: Vec<K> = if let Some(filter) = &self.match_filter {
                        node.keys.iter().filter(|k| filter(*k)).cloned().collect()
                    } else {
                        node.keys.iter().cloned().collect()
                    };

                    if !filtered_keys.is_empty() {
                        // Get msgline for this node
                        let msgline = node.msgline.clone().unwrap_or_default();

                        // Count children
                        let num_children = node.children.len();

                        return Some((msgline, filtered_keys, depth, num_children));
                    }
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_is_empty() {
        let tree: MsgTree<String> = MsgTree::new(MsgTreeMode::Defer);
        assert!(tree.is_empty());
        assert_eq!(tree.len(), 0);
    }

    #[test]
    fn test_mode_enum() {
        assert_eq!(MsgTreeMode::Defer as u8, 0);
        assert_eq!(MsgTreeMode::Shift as u8, 1);
        assert_eq!(MsgTreeMode::Trace as u8, 2);
    }

    #[test]
    fn test_add_single_key() {
        let mut tree = MsgTree::new(MsgTreeMode::Defer);
        tree.add("key1".to_string(), b"message".to_vec());
        assert_eq!(tree.len(), 1);
    }

    #[test]
    fn test_add_multiple_lines_same_key() {
        let mut tree = MsgTree::new(MsgTreeMode::Defer);
        tree.add("key1".to_string(), b"message1".to_vec());
        tree.add("key1".to_string(), b"message2".to_vec());
        assert_eq!(tree.len(), 1);

        let elem = tree.get(&"key1".to_string()).unwrap();
        assert_eq!(elem.message(), b"message1\nmessage2");
    }

    #[test]
    fn test_add_multiple_keys_same_message() {
        let mut tree = MsgTree::new(MsgTreeMode::Defer);
        tree.add("key1".to_string(), b"message".to_vec());
        tree.add("key2".to_string(), b"message".to_vec());
        assert_eq!(tree.len(), 2);

        let mut walk_results: Vec<_> = tree.walk(None).collect();
        assert_eq!(walk_results.len(), 1);
        let (msg, keys) = walk_results.pop().unwrap();
        assert_eq!(msg, b"message");
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn test_walk_with_different_messages() {
        let mut tree = MsgTree::new(MsgTreeMode::Defer);
        tree.add("key1".to_string(), b"message0".to_vec());
        tree.add("key2".to_string(), b"message2".to_vec());
        tree.add("key3".to_string(), b"message3".to_vec());

        let walk_results: Vec<_> = tree.walk(None).collect();
        assert_eq!(walk_results.len(), 3);
    }

    #[test]
    fn test_walk_with_match_filter() {
        let mut tree: MsgTree<(&str, String)> = MsgTree::new(MsgTreeMode::Shift);
        tree.add(("item1", "key1".to_string()), b"message0".to_vec());
        tree.add(("item2", "key2".to_string()), b"message2".to_vec());
        tree.add(("item3", "key3".to_string()), b"message3".to_vec());

        let walk_results: Vec<_> = tree
            .walk(Some(&|k: &(&str, String)| k.0 == "item2"))
            .collect();
        assert_eq!(walk_results.len(), 1);
    }

    #[test]
    fn test_walk_with_mapper() {
        let mut tree: MsgTree<(&str, String)> = MsgTree::new(MsgTreeMode::Shift);
        tree.add(("item1", "key1".to_string()), b"message0".to_vec());
        tree.add(("item2", "key2".to_string()), b"message2".to_vec());

        let walk_results: Vec<_> = tree
            .walk(Some(&|k: &(&str, String)| k.0.starts_with("item")))
            .collect();
        assert_eq!(walk_results.len(), 2);
    }

    #[test]
    fn test_get_message() {
        let mut tree = MsgTree::new(MsgTreeMode::Shift);
        tree.add("key1".to_string(), b"message1".to_vec());
        tree.add("key1".to_string(), b"message2".to_vec());

        let elem = tree.get(&"key1".to_string()).unwrap();
        assert_eq!(elem.message(), b"message1\nmessage2");
    }

    #[test]
    fn test_get_nonexistent_key() {
        let mut tree = MsgTree::new(MsgTreeMode::Shift);
        tree.add("key1".to_string(), b"message".to_vec());

        let elem = tree.get(&"nonexistent".to_string());
        assert!(elem.is_none());
    }

    #[test]
    fn test_clear() {
        let mut tree = MsgTree::new(MsgTreeMode::Defer);
        tree.add("key1".to_string(), b"message".to_vec());
        tree.add("key2".to_string(), b"message".to_vec());

        tree.clear();
        assert!(tree.is_empty());
        assert_eq!(tree.len(), 0);
    }

    #[test]
    fn test_mode_defer_to_shift() {
        let mut tree = MsgTree::new(MsgTreeMode::Defer);
        tree.add("key1".to_string(), b"message0".to_vec());
        tree.add("key2".to_string(), b"message1".to_vec());

        assert_eq!(tree.mode, MsgTreeMode::Defer);

        // Calling walk should trigger _update_keys and change mode
        let _results: Vec<_> = tree.walk(None).collect();

        assert_eq!(tree.mode, MsgTreeMode::Shift);
    }

    #[test]
    fn test_trace_mode() {
        let mut tree = MsgTree::new(MsgTreeMode::Trace);
        tree.add("key1".to_string(), b"message0".to_vec());
        tree.add("key2".to_string(), b"message2".to_vec());
        tree.add("key3".to_string(), b"message2".to_vec());

        // key2 and key3 share the same message, so walk returns 2 groups
        let walk_results: Vec<_> = tree.walk(None).collect();
        assert_eq!(walk_results.len(), 2);

        // walk_trace should be available
        let trace_results: Vec<_> = tree.walk_trace(None).collect();
        assert_eq!(trace_results.len(), 2);
    }

    #[test]
    #[should_panic(expected = "walk_trace() is only callable in trace mode")]
    fn test_walk_trace_in_shift_mode_panics() {
        let mut tree = MsgTree::new(MsgTreeMode::Shift);
        tree.add("key1".to_string(), b"message".to_vec());
        let _results: Vec<_> = tree.walk_trace(None).collect();
    }

    #[test]
    fn test_remove_keys() {
        let mut tree = MsgTree::new(MsgTreeMode::Shift);
        tree.add("key1".to_string(), b"message0".to_vec());
        tree.add("key2".to_string(), b"message0".to_vec());
        tree.add("key3".to_string(), b"message0".to_vec());

        tree.remove(&|k: &String| k == "key2");

        assert_eq!(tree.len(), 2);
        assert!(tree.get(&"key2".to_string()).is_none());
    }

    #[test]
    fn test_deep_tree() {
        let mut tree = MsgTree::new(MsgTreeMode::Shift);
        for i in 0..100 {
            tree.add("key1".to_string(), format!("line{}", i).as_bytes().to_vec());
        }

        let elem = tree.get(&"key1".to_string()).unwrap();
        let msg = elem.message();
        let lines: Vec<_> = msg.split(|&b| b == b'\n').collect();
        assert_eq!(lines.len(), 100);
    }

    #[test]
    fn test_shared_prefix() {
        let mut tree = MsgTree::new(MsgTreeMode::Shift);

        // Multiple keys with shared prefix
        tree.add("key1".to_string(), b"prefix".to_vec());
        tree.add("key1".to_string(), b"common".to_vec());
        tree.add("key2".to_string(), b"prefix".to_vec());
        tree.add("key2".to_string(), b"common".to_vec());

        let elem = tree.get(&"key1".to_string()).unwrap();
        assert_eq!(elem.message(), b"prefix\ncommon");

        let elem = tree.get(&"key2".to_string()).unwrap();
        assert_eq!(elem.message(), b"prefix\ncommon");
    }

    #[test]
    fn test_keys_iterator() {
        let mut tree = MsgTree::new(MsgTreeMode::Shift);
        tree.add("key1".to_string(), b"message".to_vec());
        tree.add("key2".to_string(), b"message".to_vec());
        tree.add("key3".to_string(), b"message".to_vec());

        let keys: Vec<_> = tree.keys().cloned().collect();
        assert_eq!(keys.len(), 3);
        assert!(keys.contains(&"key1".to_string()));
        assert!(keys.contains(&"key2".to_string()));
        assert!(keys.contains(&"key3".to_string()));
    }
}
