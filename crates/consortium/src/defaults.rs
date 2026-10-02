//! Configuration defaults.
//!
//! Rust implementation of `ClusterShell.Defaults`.
//!
//! Provides global configuration loaded from INI files, controlling task
//! defaults, engine settings, and NodeSet fold behavior.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::LazyLock;

use configparser::ini::Ini;

/// Configuration section names matching the Python implementation.
const CFG_SECTION_TASK_DEFAULT: &str = "task.default";
const CFG_SECTION_TASK_INFO: &str = "task.info";
const CFG_SECTION_NODESET: &str = "nodeset";
const CFG_SECTION_ENGINE: &str = "engine";

/// Globally accessible Defaults singleton, loaded from standard config paths.
pub static DEFAULTS: LazyLock<Defaults> = LazyLock::new(|| {
    let paths = config_paths("defaults.conf");
    Defaults::from_config(&paths)
});

/// Return default path list for a ClusterShell config file name.
///
/// Search order (later entries have higher priority):
/// 1. `/etc/clustershell/<name>`
/// 2. `~/.local/etc/clustershell/<name>`
/// 3. `$XDG_CONFIG_HOME/clustershell/<name>` (default: `~/.config`)
/// 4. `$CLUSTERSHELL_CFGDIR/<name>` (highest priority, if set)
pub fn config_paths(name: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();

    // System-wide
    paths.push(PathBuf::from("/etc/clustershell").join(name));

    // Per-user pip --user style
    if let Some(home) = home_dir() {
        paths.push(home.join(".local/etc/clustershell").join(name));
    }

    // XDG config
    let xdg_config = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            home_dir()
                .map(|h| h.join(".config"))
                .unwrap_or_else(|| PathBuf::from(".config"))
        });
    paths.push(xdg_config.join("clustershell").join(name));

    // $CLUSTERSHELL_CFGDIR has precedence over any other config paths
    if let Ok(cfgdir) = std::env::var("CLUSTERSHELL_CFGDIR") {
        paths.push(PathBuf::from(cfgdir).join(name));
    }

    paths
}

/// Get the user's home directory.
fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

/// A typed config value that can be stored in the defaults dictionaries.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    IntTuple(Vec<i64>),
}

impl ConfigValue {
    /// Get as bool, panicking if wrong type.
    pub fn as_bool(&self) -> bool {
        match self {
            ConfigValue::Bool(v) => *v,
            _ => panic!("expected bool, got {:?}", self),
        }
    }

    /// Get as i64, panicking if wrong type.
    pub fn as_int(&self) -> i64 {
        match self {
            ConfigValue::Int(v) => *v,
            _ => panic!("expected int, got {:?}", self),
        }
    }

    /// Get as f64, panicking if wrong type.
    pub fn as_float(&self) -> f64 {
        match self {
            ConfigValue::Float(v) => *v,
            ConfigValue::Int(v) => *v as f64,
            _ => panic!("expected float, got {:?}", self),
        }
    }

    /// Get as &str, panicking if wrong type.
    pub fn as_str(&self) -> &str {
        match self {
            ConfigValue::Str(v) => v,
            _ => panic!("expected str, got {:?}", self),
        }
    }

    /// Get as tuple of integers.
    pub fn as_int_tuple(&self) -> &[i64] {
        match self {
            ConfigValue::IntTuple(v) => v,
            _ => panic!("expected int tuple, got {:?}", self),
        }
    }
}

/// Runtime configuration with sane defaults.
///
/// Mirrors the four config dictionaries from the Python implementation:
/// `_task_default`, `_task_info`, `_nodeset`, `_engine`.
#[derive(Debug, Clone)]
pub struct Defaults {
    task_default: HashMap<String, ConfigValue>,
    task_info: HashMap<String, ConfigValue>,
    nodeset: HashMap<String, ConfigValue>,
    engine: HashMap<String, ConfigValue>,
}

/// Which type a given key should be parsed as.
#[derive(Clone, Copy)]
enum ValueType {
    Bool,
    Int,
    Float,
    Str,
    IntTuple,
}

impl Defaults {
    /// Create a new Defaults with built-in default values.
    pub fn new() -> Self {
        let mut task_default = HashMap::new();
        task_default.insert("stderr".into(), ConfigValue::Bool(false));
        task_default.insert("stdin".into(), ConfigValue::Bool(true));
        task_default.insert("stdout_msgtree".into(), ConfigValue::Bool(true));
        task_default.insert("stderr_msgtree".into(), ConfigValue::Bool(true));
        task_default.insert("engine".into(), ConfigValue::Str("auto".into()));
        task_default.insert("port_qlimit".into(), ConfigValue::Int(100)); // 1.8 compat
        task_default.insert("auto_tree".into(), ConfigValue::Bool(true));
        task_default.insert("local_workername".into(), ConfigValue::Str("exec".into()));
        task_default.insert("distant_workername".into(), ConfigValue::Str("ssh".into()));

        let mut task_info = HashMap::new();
        task_info.insert("debug".into(), ConfigValue::Bool(false));
        task_info.insert("fanout".into(), ConfigValue::Int(64));
        task_info.insert("grooming_delay".into(), ConfigValue::Float(0.25));
        task_info.insert("connect_timeout".into(), ConfigValue::Float(10.0));
        task_info.insert("command_timeout".into(), ConfigValue::Float(0.0));

        let mut nodeset = HashMap::new();
        nodeset.insert("fold_axis".into(), ConfigValue::IntTuple(vec![]));

        let mut engine = HashMap::new();
        engine.insert("port_qlimit".into(), ConfigValue::Int(100));

        Defaults {
            task_default,
            task_info,
            nodeset,
            engine,
        }
    }

    /// Load defaults from config file paths.
    ///
    /// Files are read in order; later files override earlier ones.
    /// Only files that actually exist on disk are parsed.
    pub fn from_config(paths: &[PathBuf]) -> Self {
        let mut defaults = Self::new();

        for path in paths {
            let mut ini = Ini::new();
            if ini.load(path.to_string_lossy().as_ref()).is_ok() {
                defaults.parse_ini(&ini);
            }
        }

        defaults
    }

    /// Parse an INI config, overriding current values.
    fn parse_ini(&mut self, ini: &Ini) {
        // Task default converters
        let task_default_types: &[(&str, ValueType)] = &[
            ("stderr", ValueType::Bool),
            ("stdin", ValueType::Bool),
            ("stdout_msgtree", ValueType::Bool),
            ("stderr_msgtree", ValueType::Bool),
            ("engine", ValueType::Str),
            ("port_qlimit", ValueType::Int),
            ("auto_tree", ValueType::Bool),
            ("local_workername", ValueType::Str),
            ("distant_workername", ValueType::Str),
        ];

        Self::apply_section(
            ini,
            CFG_SECTION_TASK_DEFAULT,
            task_default_types,
            &mut self.task_default,
        );

        // Task info converters
        let task_info_types: &[(&str, ValueType)] = &[
            ("debug", ValueType::Bool),
            ("fanout", ValueType::Int),
            ("grooming_delay", ValueType::Float),
            ("connect_timeout", ValueType::Float),
            ("command_timeout", ValueType::Float),
        ];

        Self::apply_section(
            ini,
            CFG_SECTION_TASK_INFO,
            task_info_types,
            &mut self.task_info,
        );

        // NodeSet converters
        let nodeset_types: &[(&str, ValueType)] = &[("fold_axis", ValueType::IntTuple)];

        Self::apply_section(ini, CFG_SECTION_NODESET, nodeset_types, &mut self.nodeset);

        // Engine converters
        let engine_types: &[(&str, ValueType)] = &[("port_qlimit", ValueType::Int)];

        Self::apply_section(ini, CFG_SECTION_ENGINE, engine_types, &mut self.engine);
    }

    /// Apply values from one INI section to a config dictionary.
    fn apply_section(
        ini: &Ini,
        section: &str,
        types: &[(&str, ValueType)],
        target: &mut HashMap<String, ConfigValue>,
    ) {
        for &(key, vtype) in types {
            if let Some(raw) = ini.get(section, key) {
                if let Some(val) = Self::parse_value(&raw, vtype) {
                    target.insert(key.to_string(), val);
                }
            }
        }
    }

    /// Parse a raw string value according to the expected type.
    fn parse_value(raw: &str, vtype: ValueType) -> Option<ConfigValue> {
        match vtype {
            ValueType::Bool => {
                let lower = raw.to_lowercase();
                match lower.as_str() {
                    "true" | "yes" | "1" | "on" => Some(ConfigValue::Bool(true)),
                    "false" | "no" | "0" | "off" => Some(ConfigValue::Bool(false)),
                    _ => None,
                }
            }
            ValueType::Int => raw.trim().parse::<i64>().ok().map(ConfigValue::Int),
            ValueType::Float => raw.trim().parse::<f64>().ok().map(ConfigValue::Float),
            ValueType::Str => Some(ConfigValue::Str(raw.to_string())),
            ValueType::IntTuple => {
                let vals: Vec<i64> = raw
                    .split(',')
                    .filter_map(|s| {
                        let trimmed = s.trim();
                        if trimmed.is_empty() {
                            None
                        } else {
                            trimmed.parse::<i64>().ok()
                        }
                    })
                    .collect();
                Some(ConfigValue::IntTuple(vals))
            }
        }
    }

    /// Look up a config attribute by name.
    ///
    /// Search order matches the Python `__getattr__`:
    /// engine -> task_default -> task_info -> nodeset.
    ///
    /// Special case: `port_qlimit` falls back to task_default if engine
    /// value is unchanged from the built-in default (1.8 compat).
    pub fn get(&self, name: &str) -> Option<&ConfigValue> {
        // 1.8 compat: port_qlimit moved into engine section
        if name == "port_qlimit" {
            if let Some(engine_val) = self.engine.get(name) {
                if *engine_val == ConfigValue::Int(100) {
                    // Engine still at default, check task_default
                    if let Some(td_val) = self.task_default.get(name) {
                        return Some(td_val);
                    }
                }
                return Some(engine_val);
            }
        }

        if let Some(v) = self.engine.get(name) {
            return Some(v);
        }
        if let Some(v) = self.task_default.get(name) {
            return Some(v);
        }
        if let Some(v) = self.task_info.get(name) {
            return Some(v);
        }
        if let Some(v) = self.nodeset.get(name) {
            return Some(v);
        }
        None
    }

    /// Set a config attribute by name.
    ///
    /// Searches the same order as get().
    pub fn set(&mut self, name: &str, value: ConfigValue) -> bool {
        if self.engine.contains_key(name) {
            self.engine.insert(name.to_string(), value);
            return true;
        }
        if self.task_default.contains_key(name) {
            self.task_default.insert(name.to_string(), value);
            return true;
        }
        if self.task_info.contains_key(name) {
            self.task_info.insert(name.to_string(), value);
            return true;
        }
        if self.nodeset.contains_key(name) {
            self.nodeset.insert(name.to_string(), value);
            return true;
        }
        false
    }

    // Convenience accessors for common fields

    /// Get the fanout value.
    pub fn fanout(&self) -> i64 {
        self.get("fanout").unwrap().as_int()
    }

    /// Get the connect timeout.
    pub fn connect_timeout(&self) -> f64 {
        self.get("connect_timeout").unwrap().as_float()
    }

    /// Get the command timeout.
    pub fn command_timeout(&self) -> f64 {
        self.get("command_timeout").unwrap().as_float()
    }

    /// Whether stderr is enabled.
    pub fn stderr(&self) -> bool {
        self.get("stderr").unwrap().as_bool()
    }

    /// Whether stdin is enabled.
    pub fn stdin(&self) -> bool {
        self.get("stdin").unwrap().as_bool()
    }

    /// Get the engine name.
    pub fn engine(&self) -> &str {
        self.get("engine").unwrap().as_str()
    }

    /// Get the local worker name.
    pub fn local_workername(&self) -> &str {
        self.get("local_workername").unwrap().as_str()
    }

    /// Get the distant worker name.
    pub fn distant_workername(&self) -> &str {
        self.get("distant_workername").unwrap().as_str()
    }

    /// Get the port queue limit.
    pub fn port_qlimit(&self) -> i64 {
        self.get("port_qlimit").unwrap().as_int()
    }

    /// Get the fold axis.
    pub fn fold_axis(&self) -> &[i64] {
        self.get("fold_axis").unwrap().as_int_tuple()
    }

    /// Whether debug mode is enabled.
    pub fn debug(&self) -> bool {
        self.get("debug").unwrap().as_bool()
    }

    /// Get the grooming delay.
    pub fn grooming_delay(&self) -> f64 {
        self.get("grooming_delay").unwrap().as_float()
    }
}

impl Default for Defaults {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_defaults_have_correct_values() {
        let d = Defaults::new();
        assert!(!d.stderr());
        assert!(d.stdin());
        assert_eq!(d.fanout(), 64);
        assert_eq!(d.connect_timeout(), 10.0);
        assert_eq!(d.command_timeout(), 0.0);
        assert_eq!(d.engine(), "auto");
        assert_eq!(d.local_workername(), "exec");
        assert_eq!(d.distant_workername(), "ssh");
        assert_eq!(d.port_qlimit(), 100);
        assert!(d.fold_axis().is_empty());
        assert!(!d.debug());
        assert_eq!(d.grooming_delay(), 0.25);
    }

    #[test]
    fn test_config_paths_basic() {
        let paths = config_paths("defaults.conf");
        assert!(paths.len() >= 3);
        assert_eq!(paths[0], PathBuf::from("/etc/clustershell/defaults.conf"));
    }

    // These tests mutate environment variables which is inherently racy
    // in a multi-threaded test runner. They pass reliably with nextest
    // (one process per test) but can flake with cargo test.
    #[test]
    fn test_config_paths_with_cfgdir() {
        // SAFETY: env mutation — nextest isolates this in its own process
        unsafe { std::env::set_var("CLUSTERSHELL_CFGDIR", "/tmp/test_cfg") };
        let paths = config_paths("defaults.conf");
        let last = paths.last().unwrap();
        assert_eq!(*last, PathBuf::from("/tmp/test_cfg/defaults.conf"));
        unsafe { std::env::remove_var("CLUSTERSHELL_CFGDIR") };
    }

    #[test]
    fn test_config_paths_with_xdg() {
        // SAFETY: env mutation — nextest isolates this in its own process
        unsafe { std::env::set_var("XDG_CONFIG_HOME", "/tmp/xdg_test") };
        unsafe { std::env::remove_var("CLUSTERSHELL_CFGDIR") };
        let paths = config_paths("defaults.conf");
        assert!(paths.contains(&PathBuf::from("/tmp/xdg_test/clustershell/defaults.conf")));
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
    }

    #[test]
    fn test_from_config_empty_paths() {
        let d = Defaults::from_config(&[]);
        assert_eq!(d.fanout(), 64);
        assert_eq!(d.connect_timeout(), 10.0);
    }

    #[test]
    fn test_from_config_with_ini_file() {
        let dir = std::env::temp_dir().join("consortium_test_defaults");
        std::fs::create_dir_all(&dir).unwrap();
        let conf_path = dir.join("test_defaults.conf");

        let mut f = std::fs::File::create(&conf_path).unwrap();
        writeln!(f, "[task.info]").unwrap();
        writeln!(f, "fanout = 128").unwrap();
        writeln!(f, "connect_timeout = 30.0").unwrap();
        writeln!(f, "debug = true").unwrap();
        writeln!(f).unwrap();
        writeln!(f, "[task.default]").unwrap();
        writeln!(f, "stderr = true").unwrap();
        writeln!(f, "engine = select").unwrap();
        writeln!(f, "distant_workername = rsh").unwrap();
        writeln!(f).unwrap();
        writeln!(f, "[nodeset]").unwrap();
        writeln!(f, "fold_axis = 1,2,3").unwrap();
        writeln!(f).unwrap();
        writeln!(f, "[engine]").unwrap();
        writeln!(f, "port_qlimit = 200").unwrap();
        drop(f);

        let d = Defaults::from_config(std::slice::from_ref(&conf_path));
        assert_eq!(d.fanout(), 128);
        assert_eq!(d.connect_timeout(), 30.0);
        assert!(d.debug());
        assert!(d.stderr());
        assert_eq!(d.engine(), "select");
        assert_eq!(d.distant_workername(), "rsh");
        assert_eq!(d.fold_axis(), &[1, 2, 3]);
        assert_eq!(d.port_qlimit(), 200);

        // Cleanup
        std::fs::remove_file(&conf_path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn test_set_attribute() {
        let mut d = Defaults::new();
        assert_eq!(d.fanout(), 64);
        d.set("fanout", ConfigValue::Int(32));
        assert_eq!(d.fanout(), 32);
    }

    #[test]
    fn test_set_unknown_attribute() {
        let mut d = Defaults::new();
        assert!(!d.set("nonexistent", ConfigValue::Bool(true)));
    }

    #[test]
    fn test_get_unknown_attribute() {
        let d = Defaults::new();
        assert!(d.get("nonexistent").is_none());
    }

    #[test]
    fn test_config_value_accessors() {
        assert!(ConfigValue::Bool(true).as_bool());
        assert_eq!(ConfigValue::Int(42).as_int(), 42);
        assert_eq!(
            ConfigValue::Float(std::f64::consts::PI).as_float(),
            std::f64::consts::PI
        );
        assert_eq!(ConfigValue::Int(42).as_float(), 42.0);
        assert_eq!(ConfigValue::Str("hello".into()).as_str(), "hello");
        assert_eq!(
            ConfigValue::IntTuple(vec![1, 2, 3]).as_int_tuple(),
            &[1, 2, 3]
        );
    }

    #[test]
    fn test_port_qlimit_compat() {
        // When engine port_qlimit is at default (100), task_default value is used
        let mut d = Defaults::new();
        // Change task_default port_qlimit
        d.task_default
            .insert("port_qlimit".into(), ConfigValue::Int(50));
        // Engine is still at default 100, so task_default value should be returned
        assert_eq!(d.port_qlimit(), 50);

        // Now change engine port_qlimit to non-default
        d.engine.insert("port_qlimit".into(), ConfigValue::Int(200));
        assert_eq!(d.port_qlimit(), 200);
    }

    #[test]
    fn test_parse_bool_variants() {
        let check = |s, expected| {
            let v = Defaults::parse_value(s, ValueType::Bool);
            assert_eq!(v, Some(ConfigValue::Bool(expected)));
        };
        check("true", true);
        check("True", true);
        check("yes", true);
        check("1", true);
        check("on", true);
        check("false", false);
        check("False", false);
        check("no", false);
        check("0", false);
        check("off", false);

        assert_eq!(Defaults::parse_value("maybe", ValueType::Bool), None);
    }

    #[test]
    fn test_parse_int_tuple_empty() {
        let v = Defaults::parse_value("", ValueType::IntTuple);
        assert_eq!(v, Some(ConfigValue::IntTuple(vec![])));
    }

    #[test]
    fn test_multiple_config_files_override() {
        let dir = std::env::temp_dir().join("consortium_test_multi");
        std::fs::create_dir_all(&dir).unwrap();

        let conf1 = dir.join("base.conf");
        let conf2 = dir.join("override.conf");

        let mut f1 = std::fs::File::create(&conf1).unwrap();
        writeln!(f1, "[task.info]").unwrap();
        writeln!(f1, "fanout = 32").unwrap();
        writeln!(f1, "connect_timeout = 5.0").unwrap();
        drop(f1);

        let mut f2 = std::fs::File::create(&conf2).unwrap();
        writeln!(f2, "[task.info]").unwrap();
        writeln!(f2, "fanout = 256").unwrap();
        drop(f2);

        let d = Defaults::from_config(&[conf1.clone(), conf2.clone()]);
        // fanout overridden by second file
        assert_eq!(d.fanout(), 256);
        // connect_timeout from first file
        assert_eq!(d.connect_timeout(), 5.0);

        // Cleanup
        std::fs::remove_file(&conf1).ok();
        std::fs::remove_file(&conf2).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn test_defaults_default_trait() {
        let d = Defaults::default();
        assert_eq!(d.fanout(), 64);
    }
}
