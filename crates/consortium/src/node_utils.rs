//! Group resolution backends.
//!
//! Rust implementation of `ClusterShell.NodeUtils`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use configparser::ini::Ini;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GroupSourceError {
    #[error("no upcall configured for group source")]
    GroupSourceNoUpcall,

    #[error("group source query failed: {0}")]
    GroupSourceQueryFailed(String),
}

#[derive(Debug, Error)]
pub enum GroupResolverSourceError {
    #[error("source not found: {0}")]
    SourceNotFound(String),

    #[error("source error: {0}")]
    SourceError(#[from] GroupSourceError),
}

#[derive(Debug, Error)]
pub enum GroupResolverConfigError {
    #[error("config file not found: {0}")]
    FileNotFound(PathBuf),

    #[error("config parsing error: {0}")]
    ParseError(String),
}

#[derive(Debug, Error)]
pub enum GroupError {
    #[error("group source error: {0}")]
    SourceError(#[from] GroupSourceError),

    #[error("group resolver error: {0}")]
    ResolverError(#[from] GroupResolverSourceError),

    #[error("group resolver config error: {0}")]
    ConfigError(#[from] GroupResolverConfigError),
}

pub type Result<T> = std::result::Result<T, GroupError>;

/// A source of node group definitions.
///
/// Group sources can be backed by various backends: static in-memory data,
/// shell scripts, configuration files, etc.
pub trait GroupSource: Send + Sync {
    /// Resolve a group name to a node pattern string.
    ///
    /// Returns a comma-separated list of node names or a node pattern.
    fn resolve_map(&self, group: &str) -> Result<String>;

    /// Get all nodes from all groups.
    ///
    /// Returns a comma-separated list of all nodes.
    fn resolve_all(&self) -> Result<String> {
        Err(GroupSourceError::GroupSourceNoUpcall.into())
    }

    /// List all available group names.
    fn resolve_list(&self) -> Result<Vec<String>> {
        Err(GroupSourceError::GroupSourceNoUpcall.into())
    }

    /// Find all groups that contain a given node.
    fn resolve_reverse(&self, _node: &str) -> Result<Vec<String>> {
        Err(GroupSourceError::GroupSourceNoUpcall.into())
    }

    /// Returns true if this source supports reverse lookups.
    fn has_reverse(&self) -> bool {
        false
    }

    /// Returns the cache time in seconds, if any.
    fn cache_time(&self) -> Option<f64> {
        None
    }
}

/// In-memory group source backed by a HashMap.
///
/// Supports basic resolve_map, and optionally resolve_all, resolve_list,
/// and resolve_reverse if provided.
pub struct StaticGroupSource {
    map: HashMap<String, String>,
    all: Option<String>,
    list: Option<Vec<String>>,
    reverse: Option<HashMap<String, Vec<String>>>,
}

impl StaticGroupSource {
    /// Create a new StaticGroupSource with the given map.
    pub fn new(map: HashMap<String, String>) -> Self {
        Self {
            map,
            all: None,
            list: None,
            reverse: None,
        }
    }

    /// Set the all nodes string.
    pub fn with_all(mut self, all: impl Into<String>) -> Self {
        self.all = Some(all.into());
        self
    }

    /// Set the list of group names.
    pub fn with_list(mut self, list: Vec<String>) -> Self {
        self.list = Some(list);
        self
    }

    /// Set the reverse lookup map (node -> groups containing it).
    pub fn with_reverse(mut self, reverse: HashMap<String, Vec<String>>) -> Self {
        self.reverse = Some(reverse);
        self
    }
}

impl GroupSource for StaticGroupSource {
    fn resolve_map(&self, group: &str) -> Result<String> {
        self.map
            .get(group)
            .cloned()
            .ok_or(GroupSourceError::GroupSourceNoUpcall.into())
    }

    fn resolve_all(&self) -> Result<String> {
        self.all
            .clone()
            .ok_or(GroupSourceError::GroupSourceNoUpcall.into())
    }

    fn resolve_list(&self) -> Result<Vec<String>> {
        self.list
            .clone()
            .ok_or(GroupSourceError::GroupSourceNoUpcall.into())
    }

    fn resolve_reverse(&self, node: &str) -> Result<Vec<String>> {
        self.reverse
            .as_ref()
            .and_then(|r| r.get(node).cloned())
            .ok_or(GroupSourceError::GroupSourceNoUpcall.into())
    }

    fn has_reverse(&self) -> bool {
        self.reverse.is_some()
    }
}

/// Shell-based group source that runs commands for lookups.
///
/// Commands can use %s as a placeholder for the group name.
pub struct UpcallGroupSource {
    map_upcall: Option<String>,
    all_upcall: Option<String>,
    list_upcall: Option<String>,
    reverse_upcall: Option<String>,
}

impl UpcallGroupSource {
    /// Create a new UpcallGroupSource with the given command templates.
    ///
    /// Command templates can use %s as a placeholder for the group name.
    pub fn new(
        map_upcall: Option<String>,
        all_upcall: Option<String>,
        list_upcall: Option<String>,
        reverse_upcall: Option<String>,
    ) -> Self {
        Self {
            map_upcall,
            all_upcall,
            list_upcall,
            reverse_upcall,
        }
    }

    fn run_command(&self, template: &str, arg: &str) -> Result<String> {
        let command = template.replace("%s", arg);
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .env("GROUP", arg)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| GroupSourceError::GroupSourceQueryFailed(e.to_string()))?;

        if output.status.success() {
            Ok(String::from_utf8(output.stdout)
                .map_err(|e| GroupSourceError::GroupSourceQueryFailed(e.to_string()))?
                .trim()
                .to_string())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(GroupSourceError::GroupSourceQueryFailed(stderr.to_string()).into())
        }
    }
}

impl GroupSource for UpcallGroupSource {
    fn resolve_map(&self, group: &str) -> Result<String> {
        match self.map_upcall.as_ref() {
            Some(t) => self.run_command(t, group),
            None => Err(GroupSourceError::GroupSourceNoUpcall.into()),
        }
    }

    fn resolve_all(&self) -> Result<String> {
        match self.all_upcall.as_ref() {
            Some(t) => self.run_command(t, ""),
            None => Err(GroupSourceError::GroupSourceNoUpcall.into()),
        }
    }

    fn resolve_list(&self) -> Result<Vec<String>> {
        match self.list_upcall.as_ref() {
            Some(t) => {
                let s = self.run_command(t, "")?;
                Ok(s.split(|c: char| c == ',' || c.is_whitespace())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect())
            }
            None => Err(GroupSourceError::GroupSourceNoUpcall.into()),
        }
    }

    fn resolve_reverse(&self, node: &str) -> Result<Vec<String>> {
        match self.reverse_upcall.as_ref() {
            Some(t) => {
                let s = self.run_command(t, node)?;
                Ok(s.split(|c: char| c == ',' || c.is_whitespace())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect())
            }
            None => Err(GroupSourceError::GroupSourceNoUpcall.into()),
        }
    }

    fn has_reverse(&self) -> bool {
        self.reverse_upcall.is_some()
    }
}

/// Manages named group sources with a default source.
///
/// Resolves group references like `@group` to node sets.
pub struct GroupResolver {
    sources: HashMap<String, Box<dyn GroupSource>>,
    default_source_name: Option<String>,
    illegal_chars: HashSet<char>,
}

impl GroupResolver {
    /// Create a new GroupResolver.
    pub fn new() -> Self {
        Self {
            sources: HashMap::new(),
            default_source_name: None,
            illegal_chars: HashSet::new(),
        }
    }

    /// Add a named source to the resolver.
    pub fn add_source(&mut self, name: impl Into<String>, source: Box<dyn GroupSource>) {
        self.sources.insert(name.into(), source);
    }

    /// Set the default source name.
    pub fn set_default(&mut self, name: impl Into<String>) {
        self.default_source_name = Some(name.into());
    }

    /// Get the source by name, or the default if name is None.
    fn get_source(&self, namespace: Option<&str>) -> Result<&dyn GroupSource> {
        let name = match namespace {
            Some(n) => n,
            None => self.default_source_name.as_ref().ok_or(
                GroupResolverSourceError::SourceNotFound("no default source set".to_string()),
            )?,
        };

        self.sources
            .get(name)
            .map(|s| s.as_ref())
            .ok_or(GroupResolverSourceError::SourceNotFound(name.to_string()).into())
    }

    /// Resolve a group to a list of nodes.
    ///
    /// If namespace is Some, uses that source; otherwise uses the default.
    pub fn group_nodes(&self, group: &str, namespace: Option<&str>) -> Result<Vec<String>> {
        let source = self.get_source(namespace)?;
        let pattern = source.resolve_map(group)?;
        Ok(pattern
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect())
    }

    /// Get all nodes from all groups in the specified namespace.
    pub fn all_nodes(&self, namespace: Option<&str>) -> Result<Vec<String>> {
        let source = self.get_source(namespace)?;
        let all = source.resolve_all()?;
        Ok(all
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect())
    }

    /// List all group names in the specified namespace.
    pub fn grouplist(&self, namespace: Option<&str>) -> Result<Vec<String>> {
        let source = self.get_source(namespace)?;
        source.resolve_list()
    }

    /// Find all groups containing a given node.
    pub fn node_groups(&self, node: &str, namespace: Option<&str>) -> Result<Vec<String>> {
        let source = self.get_source(namespace)?;
        source.resolve_reverse(node)
    }

    /// Check if the namespace supports node group lookups.
    pub fn has_node_groups(&self, namespace: Option<&str>) -> bool {
        self.get_source(namespace)
            .map(|s| s.has_reverse())
            .unwrap_or(false)
    }

    /// Set illegal characters for group names.
    pub fn set_illegal_chars(&mut self, chars: HashSet<char>) {
        self.illegal_chars = chars;
    }

    /// Check if a group name contains illegal characters.
    pub fn is_valid_group_name(&self, name: &str) -> Result<()> {
        for c in name.chars() {
            if self.illegal_chars.contains(&c) {
                return Err(GroupResolverSourceError::SourceError(
                    GroupSourceError::GroupSourceQueryFailed(format!(
                        "group name contains illegal character: '{}'",
                        c
                    )),
                )
                .into());
            }
        }
        Ok(())
    }
}

impl Default for GroupResolver {
    fn default() -> Self {
        Self::new()
    }
}

/// Loads group sources from INI configuration files.
///
/// Supports the groups.conf format with:
/// - `[Main]` section for groupsdir, confdir, autodir, default
/// - Other sections as UpcallGroupSource definitions
pub struct GroupResolverConfig {
    filenames: Vec<PathBuf>,
    illegal_chars: HashSet<char>,
    initialized: bool,
    resolver: Option<GroupResolver>,
}

impl GroupResolverConfig {
    /// Create a new GroupResolverConfig with the given filenames.
    pub fn new(filenames: Vec<PathBuf>, illegal_chars: HashSet<char>) -> Self {
        Self {
            filenames,
            illegal_chars,
            initialized: false,
            resolver: None,
        }
    }

    /// Initialize the config if not already done.
    fn ensure_initialized(&mut self) -> Result<&mut GroupResolver> {
        if !self.initialized {
            self.initialize()?;
            self.initialized = true;
        }
        Ok(self.resolver.as_mut().unwrap())
    }

    /// Initialize the resolver from config files.
    fn initialize(&mut self) -> Result<()> {
        let mut resolver = GroupResolver::new();
        resolver.set_illegal_chars(self.illegal_chars.clone());

        for filename in &self.filenames {
            if !filename.exists() {
                return Err(GroupResolverConfigError::FileNotFound(filename.clone()).into());
            }

            let mut ini = Ini::new();
            // configparser lowercases keys/sections by default
            ini.load(filename.to_string_lossy().as_ref())
                .map_err(|e| GroupResolverConfigError::ParseError(e.to_string()))?;

            // Process [Main] section (lowercased by configparser)
            if let Some(default) = ini.get("main", "default") {
                resolver.set_default(default);
            }

            // Process other sections as UpcallGroupSource
            for section_name in ini.sections() {
                if section_name == "main" || section_name == "default" {
                    continue;
                }

                let map_upcall = ini.get(&section_name, "map");
                let all_upcall = ini.get(&section_name, "all");
                let list_upcall = ini.get(&section_name, "list");
                let reverse_upcall = ini.get(&section_name, "reverse");

                // Only add source if at least map is defined
                if map_upcall.is_some() {
                    let source =
                        UpcallGroupSource::new(map_upcall, all_upcall, list_upcall, reverse_upcall);
                    resolver.add_source(section_name.clone(), Box::new(source));
                }
            }
        }

        self.resolver = Some(resolver);
        Ok(())
    }

    /// Get the underlying resolver.
    pub fn resolver(&mut self) -> Result<&mut GroupResolver> {
        self.ensure_initialized()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn test_static_group_source_basic_resolve() {
        let mut map = HashMap::new();
        map.insert("web".to_string(), "web1,web2,web3".to_string());
        map.insert("db".to_string(), "db1,db2".to_string());

        let source = StaticGroupSource::new(map);

        assert_eq!(source.resolve_map("web").unwrap(), "web1,web2,web3");
        assert_eq!(source.resolve_map("db").unwrap(), "db1,db2");
    }

    #[test]
    fn test_static_group_source_missing_group() {
        let source = StaticGroupSource::new(HashMap::new());
        assert!(source.resolve_map("nonexistent").is_err());
    }

    #[test]
    fn test_static_group_source_with_all_and_list() {
        let mut map = HashMap::new();
        map.insert("web".to_string(), "web1,web2".to_string());
        map.insert("db".to_string(), "db1,db2".to_string());

        let source = StaticGroupSource::new(map)
            .with_all("web1,web2,db1,db2".to_string())
            .with_list(vec!["web".to_string(), "db".to_string()]);

        assert_eq!(source.resolve_all().unwrap(), "web1,web2,db1,db2");
        assert_eq!(source.resolve_list().unwrap(), vec!["web", "db"]);
    }

    #[test]
    fn test_static_group_source_reverse_lookup() {
        let mut map = HashMap::new();
        map.insert("web".to_string(), "web1,web2".to_string());
        map.insert("db".to_string(), "db1,db2".to_string());

        let mut reverse = HashMap::new();
        reverse.insert("web1".to_string(), vec!["web".to_string()]);
        reverse.insert("web2".to_string(), vec!["web".to_string()]);
        reverse.insert("db1".to_string(), vec!["db".to_string()]);
        reverse.insert("db2".to_string(), vec!["db".to_string()]);

        let source = StaticGroupSource::new(map).with_reverse(reverse);

        assert!(source.has_reverse());
        assert_eq!(
            source.resolve_reverse("web1").unwrap(),
            vec!["web".to_string()]
        );
        assert_eq!(
            source.resolve_reverse("db2").unwrap(),
            vec!["db".to_string()]
        );
    }

    #[test]
    fn test_group_resolver_with_multiple_sources() {
        let mut resolver = GroupResolver::new();

        let mut map1 = HashMap::new();
        map1.insert("web".to_string(), "web1,web2".to_string());
        resolver.add_source("cluster1", Box::new(StaticGroupSource::new(map1)));

        let mut map2 = HashMap::new();
        map2.insert("db".to_string(), "db1,db2".to_string());
        resolver.add_source("cluster2", Box::new(StaticGroupSource::new(map2)));

        let web_nodes = resolver.group_nodes("web", Some("cluster1")).unwrap();
        assert_eq!(web_nodes, vec!["web1", "web2"]);

        let db_nodes = resolver.group_nodes("db", Some("cluster2")).unwrap();
        assert_eq!(db_nodes, vec!["db1", "db2"]);
    }

    #[test]
    fn test_group_resolver_default_source() {
        let mut resolver = GroupResolver::new();

        let mut map1 = HashMap::new();
        map1.insert("web".to_string(), "web1,web2".to_string());
        resolver.add_source("default", Box::new(StaticGroupSource::new(map1)));

        resolver.set_default("default");

        // Use default source (no namespace)
        let web_nodes = resolver.group_nodes("web", None).unwrap();
        assert_eq!(web_nodes, vec!["web1", "web2"]);
    }

    #[test]
    fn test_group_resolver_missing_source() {
        let resolver = GroupResolver::new();
        assert!(resolver.group_nodes("web", Some("nonexistent")).is_err());
    }

    #[test]
    fn test_group_resolver_missing_default() {
        let resolver = GroupResolver::new();
        assert!(resolver.group_nodes("web", None).is_err());
    }

    #[test]
    fn test_illegal_chars_validation() {
        let mut resolver = GroupResolver::new();
        let mut illegal = HashSet::new();
        illegal.insert('$');
        illegal.insert(' ');

        resolver.set_illegal_chars(illegal);

        assert!(resolver.is_valid_group_name("web").is_ok());
        assert!(resolver.is_valid_group_name("web-group").is_ok());
        assert!(resolver.is_valid_group_name("web_group").is_ok());
        assert!(resolver.is_valid_group_name("web$group").is_err());
        assert!(resolver.is_valid_group_name("web group").is_err());
    }

    #[test]
    fn test_upcall_source_no_upcall() {
        let source = UpcallGroupSource::new(None, None, None, None);
        assert!(source.resolve_map("test").is_err());
        assert!(source.resolve_all().is_err());
        assert!(source.resolve_list().is_err());
        assert!(source.resolve_reverse("node").is_err());
        assert!(!source.has_reverse());
    }

    #[test]
    fn test_upcall_source_with_map() {
        let source = UpcallGroupSource::new(Some("echo web1,web2".to_string()), None, None, None);
        assert_eq!(source.resolve_map("test").unwrap(), "web1,web2");
    }

    /// Upcall commands must not block reading from stdin.
    ///
    /// Mirrors upstream fix cea-hpc/clustershell@129d6ed: pass DEVNULL as stdin
    /// so that upcall scripts that accidentally read stdin do not hang the caller.
    #[test]
    fn test_upcall_source_stdin_is_null() {
        // A command that reads all of stdin and echoes its length would hang
        // forever if stdin were an open pipe; with Stdio::null() it sees EOF
        // immediately and returns "0".
        let source = UpcallGroupSource::new(
            Some("wc -c < /dev/stdin | tr -d ' '".to_string()),
            None,
            None,
            None,
        );
        let result = source.resolve_map("test").unwrap();
        assert_eq!(
            result, "0",
            "upcall should receive no stdin (got {:?})",
            result
        );
    }

    #[test]
    fn test_group_resolver_sources_from_ini() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"[Main]
default = cluster1

[cluster1]
map = echo web1,web2

[cluster2]
map = echo db1,db2
list = echo web db
all = echo web1,web2,db1,db2
reverse = echo web
"#
        )
        .unwrap();

        let mut config = GroupResolverConfig::new(vec![file.path().to_path_buf()], HashSet::new());
        let resolver = config.resolver().unwrap();

        let web_nodes = resolver.group_nodes("test", Some("cluster1")).unwrap();
        assert_eq!(web_nodes, vec!["web1", "web2"]);

        let db_nodes = resolver.group_nodes("test", Some("cluster2")).unwrap();
        assert_eq!(db_nodes, vec!["db1", "db2"]);

        let list = resolver.grouplist(Some("cluster2")).unwrap();
        assert_eq!(list, vec!["web", "db"]);
    }

    #[test]
    fn test_group_resolver_config_file_not_found() {
        let mut config = GroupResolverConfig::new(
            vec![PathBuf::from("/nonexistent/path/groups.conf")],
            HashSet::new(),
        );
        assert!(config.resolver().is_err());
    }

    #[test]
    fn test_whitespace_splitting() {
        let mut map = HashMap::new();
        // Test with various whitespace
        map.insert(
            "web".to_string(),
            "web1, web2  ,web3\tweb4\nweb5".to_string(),
        );

        let source = StaticGroupSource::new(map);
        let nodes = source.resolve_map("web").unwrap();

        let resolver = GroupResolver::new();
        let parsed = nodes
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();

        assert_eq!(parsed, vec!["web1", "web2", "web3", "web4", "web5"]);
    }
}
