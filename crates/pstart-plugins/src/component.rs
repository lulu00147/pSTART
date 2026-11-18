//! Minimal WIT component contract and Wasmtime component runtime.

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder, Trap};

use pstart_policy::{CapabilitySet, APP_SEARCH, CONFIG, COPY, OPEN, RECORD, SEARCH};

use crate::PluginBudget;

mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/plugin.wit",
        world: "plugin",
    });
}

/// Component load or execution failure.
#[derive(Debug, thiserror::Error)]
pub enum ComponentError {
    /// Component compilation failed.
    #[error("component compile failed: {0}")]
    Compile(String),
    /// Component instantiation failed.
    #[error("component instantiate failed: {0}")]
    Instantiate(String),
    /// Exported `run` returned an error.
    #[error("component run failed: {0}")]
    Run(String),
    /// The wall-clock deadline expired.
    #[error("component timed out")]
    Timeout,
}

/// Store data exposed to component host imports.
struct HostState {
    limits: StoreLimits,
    capabilities: CapabilitySet,
    records: Vec<u32>,
    opens: Vec<String>,
    copies: Vec<String>,
    config: HashMap<u32, u32>,
    search: Arc<dyn Fn(u32) -> u32 + Send + Sync>,
    app_search: Arc<dyn Fn(&str) -> String + Send + Sync>,
}

impl bindings::PluginImports for HostState {
    fn host_record(&mut self, value: u32) {
        if self.capabilities.allows(RECORD) {
            self.records.push(value);
        }
    }

    fn host_config(&mut self, key: u32) -> u32 {
        if !self.capabilities.allows(CONFIG) {
            return 0;
        }
        self.config.get(&key).copied().unwrap_or(0)
    }

    fn host_search(&mut self, query: u32) -> u32 {
        if !self.capabilities.allows(SEARCH) {
            return 0;
        }
        (self.search)(query)
    }

    fn host_app_search(&mut self, query: String) -> String {
        if !self.capabilities.allows(APP_SEARCH) {
            return String::new();
        }
        (self.app_search)(&query)
    }

    fn host_open(&mut self, path: String) -> u32 {
        if !self.capabilities.allows(OPEN) {
            return 0;
        }
        self.opens.push(path);
        1
    }

    fn host_copy(&mut self, text: String) -> u32 {
        if !self.capabilities.allows(COPY) {
            return 0;
        }
        self.copies.push(text);
        1
    }
}

/// Result of a component `run-string` call plus Host API observations.
#[derive(Debug, PartialEq, Eq)]
pub struct TextHostOutput {
    /// String returned by `run-string`.
    pub text: String,
    /// Values recorded through `host-record`.
    pub records: Vec<u32>,
    /// Paths requested through `host-open`.
    pub opens: Vec<String>,
    /// Text requested through `host-copy`.
    pub copies: Vec<String>,
}

/// Result of a component `run` call plus Host API observations.
#[derive(Debug, PartialEq, Eq)]
pub struct HostOutput {
    /// Value returned by `run`.
    pub value: u32,
    /// Values recorded through `host-record`.
    pub records: Vec<u32>,
    /// Paths requested through `host-open`.
    pub opens: Vec<String>,
    /// Text requested through `host-copy`.
    pub copies: Vec<String>,
}

/// Shared component-model engine.
pub struct ComponentRuntime {
    engine: Engine,
    capabilities: u8,
    config: HashMap<u32, u32>,
    search: Arc<dyn Fn(u32) -> u32 + Send + Sync>,
    app_search: Arc<dyn Fn(&str) -> String + Send + Sync>,
}

impl ComponentRuntime {
    /// Create an engine with component support.
    #[must_use]
    pub fn new() -> Self {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.consume_fuel(true);
        config.epoch_interruption(true);
        let engine = Engine::new(&config).expect("fixed component engine configuration");
        Self {
            engine,
            capabilities: CapabilitySet::ALL.bits(),
            config: HashMap::new(),
            search: Arc::new(|_| 0),
            app_search: Arc::new(|_| String::new()),
        }
    }

    /// Set the capability set granted to this component.
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: CapabilitySet) -> Self {
        self.capabilities = capabilities.bits();
        self
    }

    /// Create an engine with values returned by `host-config`.
    #[must_use]
    pub fn with_config(config: impl IntoIterator<Item = (u32, u32)>) -> Self {
        let mut runtime = Self::new();
        runtime.config = config.into_iter().collect();
        runtime
    }

    /// Set the callback used by `host-search`.
    #[must_use]
    pub fn with_search(mut self, search: impl Fn(u32) -> u32 + Send + Sync + 'static) -> Self {
        self.search = Arc::new(search);
        self
    }

    /// Set the callback used by `host-app-search`.
    #[must_use]
    pub fn with_app_search(
        mut self,
        search: impl Fn(&str) -> String + Send + Sync + 'static,
    ) -> Self {
        self.app_search = Arc::new(search);
        self
    }

    /// Compile and read the packed manifest `(id << 32) | version`.
    pub fn manifest(&self, bytes: impl AsRef<[u8]>) -> Result<u64, ComponentError> {
        let (mut store, plugin) = self.instantiate(bytes)?;
        let manifest = plugin
            .call_manifest(&mut store)
            .map_err(|error| ComponentError::Run(error.to_string()))?;
        Ok(manifest)
    }

    /// Compile and call the exported `run(input: u32) -> u32`.
    pub fn run(&self, bytes: impl AsRef<[u8]>, input: u32) -> Result<u32, ComponentError> {
        let (mut store, plugin) = self.instantiate(bytes)?;
        plugin
            .call_run(&mut store, input)
            .map_err(|error| ComponentError::Run(error.to_string()))
    }

    /// Compile and echo `input` through the text export.
    pub fn run_text(&self, bytes: impl AsRef<[u8]>, input: &str) -> Result<String, ComponentError> {
        let (mut store, plugin) = self.instantiate(bytes)?;
        plugin
            .call_run_string(&mut store, input)
            .map_err(|error| ComponentError::Run(error.to_string()))
    }

    /// Compile and call `run-string`, returning host records and actions.
    pub fn run_text_host(
        &self,
        bytes: impl AsRef<[u8]>,
        input: &str,
    ) -> Result<TextHostOutput, ComponentError> {
        let (mut store, plugin) = self.instantiate(bytes)?;
        let text = plugin
            .call_run_string(&mut store, input)
            .map_err(|error| ComponentError::Run(error.to_string()))?;
        let state = store.data();
        Ok(TextHostOutput {
            text,
            records: state.records.clone(),
            opens: state.opens.clone(),
            copies: state.copies.clone(),
        })
    }

    /// Compile and call `run-string` with a wall-clock timeout.
    pub fn run_text_host_timeout(
        &self,
        bytes: impl AsRef<[u8]>,
        input: &str,
        timeout: Duration,
    ) -> Result<TextHostOutput, ComponentError> {
        let (mut store, plugin) = self.instantiate(bytes)?;
        store.set_epoch_deadline(1);
        store
            .set_fuel(u64::MAX)
            .expect("fuel is enabled for the component engine");
        let (cancel, cancelled) = mpsc::channel();
        let engine = self.engine.clone();
        let timer = std::thread::spawn(move || {
            if cancelled.recv_timeout(timeout).is_err() {
                engine.increment_epoch();
            }
        });
        let result = plugin.call_run_string(&mut store, input);
        let _ = cancel.send(());
        let _ = timer.join();
        let text = result.map_err(|error| {
            if error.downcast_ref::<Trap>() == Some(&Trap::Interrupt) {
                ComponentError::Timeout
            } else {
                ComponentError::Run(error.to_string())
            }
        })?;
        let state = store.data();
        Ok(TextHostOutput {
            text,
            records: state.records.clone(),
            opens: state.opens.clone(),
            copies: state.copies.clone(),
        })
    }

    /// Compile and call `run`, returning host records made during the call.
    pub fn run_host(
        &self,
        bytes: impl AsRef<[u8]>,
        input: u32,
    ) -> Result<HostOutput, ComponentError> {
        let (mut store, plugin) = self.instantiate(bytes)?;
        let value = plugin
            .call_run(&mut store, input)
            .map_err(|error| ComponentError::Run(error.to_string()))?;
        let state = store.data();
        Ok(HostOutput {
            value,
            records: state.records.clone(),
            opens: state.opens.clone(),
            copies: state.copies.clone(),
        })
    }

    /// Compile and call `run`, returning host records made during the call.
    pub fn run_with_records(
        &self,
        bytes: impl AsRef<[u8]>,
        input: u32,
    ) -> Result<(u32, Vec<u32>), ComponentError> {
        let output = self.run_host(bytes, input)?;
        Ok((output.value, output.records))
    }

    fn instantiate(
        &self,
        bytes: impl AsRef<[u8]>,
    ) -> Result<(Store<HostState>, bindings::Plugin), ComponentError> {
        let component = Component::new(&self.engine, bytes)
            .map_err(|error| ComponentError::Compile(error.to_string()))?;
        let mut linker = Linker::new(&self.engine);
        bindings::Plugin::add_to_linker::<HostState, wasmtime::component::HasSelf<HostState>>(
            &mut linker,
            |state| state,
        )
        .expect("static host imports are always linkable");
        let mut store = self.store();
        let plugin = bindings::Plugin::instantiate(&mut store, &component, &linker)
            .map_err(|error| ComponentError::Instantiate(error.to_string()))?;
        Ok((store, plugin))
    }

    fn store(&self) -> Store<HostState> {
        let budget = PluginBudget::mvp_default();
        let limits = StoreLimitsBuilder::new()
            .memory_size(budget.max_memory_bytes)
            .instances(2)
            .build();
        let mut store: Store<HostState> = Store::new(
            &self.engine,
            HostState {
                limits,
                capabilities: CapabilitySet::from_bits(self.capabilities),
                records: Vec::new(),
                opens: Vec::new(),
                copies: Vec::new(),
                config: self.config.clone(),
                search: Arc::clone(&self.search),
                app_search: Arc::clone(&self.app_search),
            },
        );
        store.limiter(|state| &mut state.limits);
        store.set_epoch_deadline(u64::MAX);
        store
            .set_fuel(budget.fuel)
            .expect("fuel is enabled for the component engine");
        store
    }
}

impl Default for ComponentRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUN_42: &str = r#"
        (component
          (core module $m
            (memory (export "memory") 1)
            (global $next (mut i32) (i32.const 16384))
            (func (export "cabi_realloc")
              (param $old i32) (param $old_size i32)
              (param $align i32) (param $new_size i32)
              (result i32)
              (local $ptr i32)
              (local.set $ptr (global.get $next))
              (global.set $next
                (i32.add (global.get $next) (local.get $new_size)))
              (local.get $ptr))
            (func (export "manifest") (result i64)
              i64.const 30064771075)
            (func (export "run") (param i32) (result i32)
              local.get 0
              i32.const 2
              i32.mul)
            (func (export "run-string")
              (param $ptr i32) (param $len i32) (result i32)
              (local $i i32)
              (block $done
                (loop $loop
                  (br_if $done
                    (i32.ge_u (local.get $i) (local.get $len)))
                  (i32.store8
                    (i32.add (i32.const 8192) (local.get $i))
                    (i32.load8_u
                      (i32.add (local.get $ptr) (local.get $i))))
                  (local.set $i
                    (i32.add (local.get $i) (i32.const 1)))
                  (br $loop)))
              (i32.store (i32.const 32768) (i32.const 8192))
              (i32.store (i32.const 32772) (local.get $len))
              (i32.const 32768)))
          (core instance $i (instantiate $m))
          (func (export "manifest") (result u64)
            (canon lift (core func $i "manifest")))
          (func (export "run") (param "input" u32) (result u32)
            (canon lift (core func $i "run")))
          (func (export "run-string")
            (param "input" string) (result string)
            (canon lift (core func $i "run-string")
              (memory (core memory $i "memory"))
              (realloc (core func $i "cabi_realloc")))))
    "#;

    #[test]
    fn runs_component_export() {
        let runtime = ComponentRuntime::default();
        assert_eq!(runtime.run(RUN_42, 21).unwrap(), 42);
        assert_eq!(runtime.run(RUN_42, 0).unwrap(), 0);
        assert_eq!(runtime.manifest(RUN_42).unwrap(), (7u64 << 32) | 3);
    }

    #[test]
    fn echoes_text() {
        let runtime = ComponentRuntime::new();
        assert_eq!(runtime.run_text(RUN_42, "hello").unwrap(), "hello");
        assert_eq!(runtime.run_text(RUN_42, "").unwrap(), "");
    }

    #[test]
    fn invalid_component_is_rejected() {
        let runtime = ComponentRuntime::new();
        assert!(matches!(
            runtime.run(b"not a component", 1),
            Err(ComponentError::Compile(_))
        ));
    }

    #[test]
    fn missing_export_is_rejected() {
        let runtime = ComponentRuntime::new();
        assert!(matches!(
            runtime.run("(component)", 1),
            Err(ComponentError::Instantiate(_))
        ));
    }

    #[test]
    fn unknown_host_import_is_rejected() {
        // A component cannot acquire an undeclared host capability.
        let component = r#"
            (component
              (import "host-delete" (func (param "path" string)))
              (core module $m
                (func (export "manifest") (result i64) i64.const 1))
              (core instance $i (instantiate $m))
              (func (export "manifest") (result u64)
                (canon lift (core func $i "manifest"))))
        "#;
        let runtime = ComponentRuntime::new();
        assert!(matches!(
            runtime.manifest(component),
            Err(ComponentError::Instantiate(_))
        ));
    }

    #[test]
    fn calls_host_import() {
        let component = r#"
            (component
              (import "host-record"
                (func $host_record (param "value" u32)))
              (import "host-config"
                (func $host_config (param "key" u32) (result u32)))
              (import "host-search"
                (func $host_search (param "query" u32) (result u32)))
              (core func $host_record_lower
                (canon lower (func $host_record)))
              (core func $host_config_lower
                (canon lower (func $host_config)))
              (core func $host_search_lower
                (canon lower (func $host_search)))
              (core module $m
                (import "" "host-record"
                  (func $record (param i32)))
                (import "" "host-config"
                  (func $config (param i32) (result i32)))
                (import "" "host-search"
                  (func $search (param i32) (result i32)))
                (memory (export "memory") 1)
                (global $next (mut i32) (i32.const 16384))
                (func (export "cabi_realloc")
                  (param i32 i32 i32 i32) (result i32)
                  (local $ptr i32)
                  (local.set $ptr (global.get $next))
                  (global.set $next
                    (i32.add (global.get $next) (local.get 3)))
                  (local.get $ptr))
                (func (export "manifest") (result i64)
                  i64.const 0)
                (func (export "run") (param i32) (result i32)
                  local.get 0
                  call $config
                  call $record
                  local.get 0
                  call $search)
                (func (export "run-string")
                  (param i32 i32) (result i32)
                  (i32.store (i32.const 32768) (i32.const 8192))
                  (i32.store (i32.const 32772) (i32.const 0))
                  (i32.const 32768)))
              (core instance $i
                (instantiate $m
                  (with "" (instance
                    (export "host-record" (func $host_record_lower))
                    (export "host-config" (func $host_config_lower))
                    (export "host-search" (func $host_search_lower))))))
              (func (export "manifest") (result u64)
                (canon lift (core func $i "manifest")))
              (func (export "run") (param "input" u32) (result u32)
                (canon lift (core func $i "run")))
              (func (export "run-string")
                (param "input" string) (result string)
                (canon lift (core func $i "run-string")
                  (memory (core memory $i "memory"))
                  (realloc (core func $i "cabi_realloc")))))
        "#;
        let runtime = ComponentRuntime::with_config([(7, 42)]).with_search(|query| query + 2);
        let (value, records) = runtime.run_with_records(component, 7).unwrap();
        assert_eq!(value, 9);
        assert_eq!(records, vec![42]);

        let runtime = ComponentRuntime::new();
        let (value, records) = runtime.run_with_records(component, 7).unwrap();
        assert_eq!(value, 0);
        assert_eq!(records, vec![0]);
    }

    #[test]
    fn opens_path_through_host() {
        let component = r#"
            (component
              (import "host-open"
                (func $host_open (param "path" string) (result u32)))
              (import "host-copy"
                (func $host_copy (param "text" string) (result u32)))
              (core module $mem
                (memory (export "memory") 1)
                (global $next (mut i32) (i32.const 16384))
                (func (export "cabi_realloc")
                  (param i32 i32 i32 i32) (result i32)
                  (local $ptr i32)
                  (local.set $ptr (global.get $next))
                  (global.set $next
                    (i32.add (global.get $next) (local.get 3)))
                  (local.get $ptr)))
              (core instance $mem_i (instantiate $mem))
              (core func $host_open_lower
                (canon lower (func $host_open)
                  (memory (core memory $mem_i "memory"))))
              (core func $host_copy_lower
                (canon lower (func $host_copy)
                  (memory (core memory $mem_i "memory"))))
              (core module $m
                (import "env" "memory" (memory 1))
                (import "env" "host-open"
                  (func $open (param i32 i32) (result i32)))
                (import "env" "host-copy"
                  (func $copy (param i32 i32) (result i32)))
                (func (export "manifest") (result i64)
                  i64.const 0)
                (func (export "run") (param i32) (result i32)
                  (i32.store8 (i32.const 1024) (i32.const 97))
                  (i32.const 1024)
                  (i32.const 1)
                  call $open
                  drop
                  (i32.store8 (i32.const 1025) (i32.const 98))
                  (i32.const 1025)
                  (i32.const 1)
                  call $copy
                  drop
                  local.get 0
                  i32.const 1
                  i32.add)
                (func (export "run-string")
                  (param i32 i32) (result i32)
                  (i32.store (i32.const 32768) (i32.const 8192))
                  (i32.store (i32.const 32772) (i32.const 0))
                  (i32.const 32768)))
              (core instance $i
                (instantiate $m
                  (with "env" (instance
                    (export "memory" (memory $mem_i "memory"))
                    (export "host-open" (func $host_open_lower))
                    (export "host-copy" (func $host_copy_lower))))))
              (func (export "manifest") (result u64)
                (canon lift (core func $i "manifest")))
              (func (export "run") (param "input" u32) (result u32)
                (canon lift (core func $i "run")))
              (func (export "run-string")
                (param "input" string) (result string)
                (canon lift (core func $i "run-string")
                  (memory (core memory $mem_i "memory"))
                  (realloc (core func $mem_i "cabi_realloc")))))
        "#;
        let output = ComponentRuntime::new().run_host(component, 7).unwrap();
        assert_eq!(output.value, 8);
        assert_eq!(output.opens, vec!["a"]);
        assert_eq!(output.copies, vec!["b"]);
    }

    #[test]
    fn action_component_requests_open_and_copy() {
        let output = ComponentRuntime::new()
            .run_text_host(include_bytes!("../../../plugins/action.wasm"), "hello")
            .unwrap();
        assert_eq!(output.text, "");
        assert_eq!(output.opens, vec!["hello"]);
        assert_eq!(output.copies, vec!["hello"]);
    }

    #[test]
    fn default_runtime_grants_all_capabilities() {
        let output = ComponentRuntime::default()
            .run_text_host(include_bytes!("../../../plugins/action.wasm"), "hello")
            .unwrap();
        assert_eq!(output.opens, vec!["hello"]);
        assert_eq!(output.copies, vec!["hello"]);
    }

    #[test]
    fn restricted_capabilities_deny_open_and_copy() {
        let output = ComponentRuntime::new()
            .with_capabilities(CapabilitySet::SEARCH_ONLY)
            .run_text_host(include_bytes!("../../../plugins/action.wasm"), "hello")
            .unwrap();
        assert_eq!(output.text, "");
        assert!(output.opens.is_empty());
        assert!(output.copies.is_empty());
    }

    #[test]
    fn empty_capabilities_suppress_all_host_calls() {
        use super::bindings::PluginImports;

        let mut state = HostState {
            limits: StoreLimitsBuilder::new().build(),
            capabilities: CapabilitySet::EMPTY,
            records: Vec::new(),
            opens: Vec::new(),
            copies: Vec::new(),
            config: HashMap::new(),
            search: Arc::new(|_| 7),
            app_search: Arc::new(|_| "found".to_owned()),
        };
        state.host_record(1);
        assert_eq!(state.host_config(1), 0);
        assert_eq!(state.host_search(1), 0);
        assert_eq!(state.host_app_search("q".to_owned()), "");
        assert_eq!(state.host_open("p".to_owned()), 0);
        assert_eq!(state.host_copy("c".to_owned()), 0);
        assert!(state.records.is_empty());
        assert!(state.opens.is_empty());
        assert!(state.copies.is_empty());
    }

    #[test]
    fn all_capability_masks_are_enforced() {
        use super::bindings::PluginImports;

        for mask in 0u8..64 {
            let mut state = HostState {
                limits: StoreLimitsBuilder::new().build(),
                capabilities: CapabilitySet::from_bits(mask),
                records: Vec::new(),
                opens: Vec::new(),
                copies: Vec::new(),
                config: HashMap::from([(1, 9)]),
                search: Arc::new(|query| query + 1),
                app_search: Arc::new(|_| "found".to_owned()),
            };
            state.host_record(7);
            assert_eq!(state.host_config(1), if mask & CONFIG != 0 { 9 } else { 0 });
            assert_eq!(
                state.host_search(41),
                if mask & SEARCH != 0 { 42 } else { 0 }
            );
            assert_eq!(
                state.host_app_search("q".to_owned()),
                if mask & APP_SEARCH != 0 { "found" } else { "" }
            );
            assert_eq!(state.host_open("p".to_owned()), u32::from(mask & OPEN != 0));
            assert_eq!(state.host_copy("c".to_owned()), u32::from(mask & COPY != 0));
            assert_eq!(state.records.len(), usize::from(mask & RECORD != 0));
            assert_eq!(state.opens.len(), usize::from(mask & OPEN != 0));
            assert_eq!(state.copies.len(), usize::from(mask & COPY != 0));
        }
    }

    #[test]
    fn all_capabilities_allow_host_calls() {
        use super::bindings::PluginImports;

        let mut state = HostState {
            limits: StoreLimitsBuilder::new().build(),
            capabilities: CapabilitySet::ALL,
            records: Vec::new(),
            opens: Vec::new(),
            copies: Vec::new(),
            config: HashMap::from([(1, 9)]),
            search: Arc::new(|query| query + 1),
            app_search: Arc::new(|_| "found".to_owned()),
        };
        state.host_record(7);
        assert_eq!(state.host_config(1), 9);
        assert_eq!(state.host_search(41), 42);
        assert_eq!(state.host_app_search("q".to_owned()), "found");
        assert_eq!(state.host_open("p".to_owned()), 1);
        assert_eq!(state.host_copy("c".to_owned()), 1);
        assert_eq!(state.records, vec![7]);
        assert_eq!(state.opens, vec!["p".to_owned()]);
        assert_eq!(state.copies, vec!["c".to_owned()]);
    }

    #[test]
    fn host_app_search_bridges_component() {
        let component = r#"
            (component
              (import "host-app-search"
                (func $host_app (param "query" string) (result string)))
              (core module $mem
                (memory (export "memory") 1)
                (global $next (mut i32) (i32.const 16384))
                (func (export "cabi_realloc")
                  (param i32 i32 i32 i32) (result i32)
                  (local $ptr i32)
                  (local.set $ptr (global.get $next))
                  (global.set $next (i32.add (global.get $next) (local.get 3)))
                  (local.get $ptr)))
              (core instance $mem_i (instantiate $mem))
              (core func $host_app_lower
                (canon lower (func $host_app)
                  (memory (core memory $mem_i "memory"))
                  (realloc (core func $mem_i "cabi_realloc"))))
              (core module $m
                (import "env" "memory" (memory 1))
                (import "env" "host-app-search" (func $app (param i32 i32 i32)))
                (func (export "manifest") (result i64) i64.const 1)
                (func (export "run") (param i32) (result i32) local.get 0)
                (func (export "run-string") (param i32 i32) (result i32)
                  local.get 0
                  local.get 1
                  i32.const 32768
                  call $app
                  i32.const 32768))
              (core instance $i
                (instantiate $m
                  (with "env" (instance
                    (export "memory" (memory $mem_i "memory"))
                    (export "host-app-search" (func $host_app_lower))))))
              (func (export "manifest") (result u64)
                (canon lift (core func $i "manifest")))
              (func (export "run") (param "input" u32) (result u32)
                (canon lift (core func $i "run")))
              (func (export "run-string") (param "input" string) (result string)
                (canon lift (core func $i "run-string")
                  (memory (core memory $mem_i "memory"))
                  (realloc (core func $mem_i "cabi_realloc")))))
        "#;
        let runtime =
            ComponentRuntime::new().with_app_search(|query| format!("C:\\Apps\\{query}.lnk"));
        let output = runtime.run_text(component, "Notepad").unwrap();
        assert_eq!(output, "C:\\Apps\\Notepad.lnk");
    }

    #[test]
    fn times_out_an_infinite_component() {
        let component = r#"
            (component
              (core module $m
                (memory (export "memory") 1)
                (global $next (mut i32) (i32.const 16384))
                (func (export "cabi_realloc")
                  (param i32 i32 i32 i32) (result i32)
                  (local $ptr i32)
                  (local.set $ptr (global.get $next))
                  (global.set $next
                    (i32.add (global.get $next) (local.get 3)))
                  (local.get $ptr))
                (func (export "manifest") (result i64)
                  i64.const 0)
                (func (export "run") (param i32) (result i32)
                  i32.const 0)
                (func (export "run-string")
                  (param i32 i32) (result i32)
                  (loop $spin
                    br $spin)
                  unreachable))
              (core instance $i (instantiate $m))
              (func (export "manifest") (result u64)
                (canon lift (core func $i "manifest")))
              (func (export "run") (param "input" u32) (result u32)
                (canon lift (core func $i "run")))
              (func (export "run-string")
                (param "input" string) (result string)
                (canon lift (core func $i "run-string")
                  (memory (core memory $i "memory"))
                  (realloc (core func $i "cabi_realloc")))))
        "#;
        let error = ComponentRuntime::new()
            .run_text_host_timeout(component, "x", Duration::from_millis(1))
            .unwrap_err();
        assert!(matches!(error, ComponentError::Timeout), "{error}");
    }

    #[test]
    fn completes_before_timeout() {
        let output = ComponentRuntime::new()
            .run_text_host_timeout(
                include_bytes!("../../../plugins/action.wasm"),
                "hello",
                Duration::from_secs(1),
            )
            .unwrap();
        assert_eq!(output.opens, vec!["hello"]);
    }

    #[test]
    fn trapping_manifest_is_reported() {
        let runtime = ComponentRuntime::new();
        let component = r#"
            (component
              (core module $m
                (memory (export "memory") 1)
                (global $next (mut i32) (i32.const 16384))
                (func (export "cabi_realloc")
                  (param i32 i32 i32 i32) (result i32)
                  (local $ptr i32)
                  (local.set $ptr (global.get $next))
                  (global.set $next
                    (i32.add (global.get $next) (local.get 3)))
                  (local.get $ptr))
                (func (export "manifest") (result i64)
                  unreachable)
                (func (export "run") (param i32) (result i32)
                  i32.const 0)
                (func (export "run-string")
                  (param i32 i32) (result i32)
                  (i32.store (i32.const 32768) (i32.const 8192))
                  (i32.store (i32.const 32772) (i32.const 0))
                  (i32.const 32768)))
              (core instance $i (instantiate $m))
              (func (export "manifest") (result u64)
                (canon lift (core func $i "manifest")))
              (func (export "run") (param "input" u32) (result u32)
                (canon lift (core func $i "run")))
              (func (export "run-string")
                (param "input" string) (result string)
                (canon lift (core func $i "run-string")
                  (memory (core memory $i "memory"))
                  (realloc (core func $i "cabi_realloc")))))
        "#;
        assert!(matches!(
            runtime.manifest(component),
            Err(ComponentError::Run(_))
        ));
        assert_eq!(runtime.run(component, 1).unwrap(), 0);
    }

    #[test]
    fn timeout_wrapper_reports_non_timeout_run_error() {
        let runtime = ComponentRuntime::new();
        let component = r#"
            (component
              (core module $m
                (memory (export "memory") 1)
                (global $next (mut i32) (i32.const 16384))
                (func (export "cabi_realloc")
                  (param i32 i32 i32 i32) (result i32)
                  (local $ptr i32)
                  (local.set $ptr (global.get $next))
                  (global.set $next
                    (i32.add (global.get $next) (local.get 3)))
                  (local.get $ptr))
                (func (export "manifest") (result i64)
                  i64.const 0)
                (func (export "run") (param i32) (result i32)
                  i32.const 0)
                (func (export "run-string")
                  (param i32 i32) (result i32)
                  unreachable))
              (core instance $i (instantiate $m))
              (func (export "manifest") (result u64)
                (canon lift (core func $i "manifest")))
              (func (export "run") (param "input" u32) (result u32)
                (canon lift (core func $i "run")))
              (func (export "run-string")
                (param "input" string) (result string)
                (canon lift (core func $i "run-string")
                  (memory (core memory $i "memory"))
                  (realloc (core func $i "cabi_realloc")))))
        "#;
        assert!(matches!(
            runtime.run_text_host_timeout(component, "x", Duration::from_secs(1)),
            Err(ComponentError::Run(_))
        ));
    }

    #[test]
    fn trapping_run_is_reported() {
        let runtime = ComponentRuntime::new();
        let component = r#"
            (component
              (core module $m
                (memory (export "memory") 1)
                (global $next (mut i32) (i32.const 16384))
                (func (export "cabi_realloc")
                  (param i32 i32 i32 i32) (result i32)
                  (local $ptr i32)
                  (local.set $ptr (global.get $next))
                  (global.set $next
                    (i32.add (global.get $next) (local.get 3)))
                  (local.get $ptr))
                (func (export "manifest") (result i64)
                  i64.const 0)
                (func (export "run") (param i32) (result i32)
                  unreachable)
                (func (export "run-string")
                  (param i32 i32) (result i32)
                  (i32.store (i32.const 32768) (i32.const 8192))
                  (i32.store (i32.const 32772) (i32.const 0))
                  (i32.const 32768)))
              (core instance $i (instantiate $m))
              (func (export "manifest") (result u64)
                (canon lift (core func $i "manifest")))
              (func (export "run") (param "input" u32) (result u32)
                (canon lift (core func $i "run")))
              (func (export "run-string")
                (param "input" string) (result string)
                (canon lift (core func $i "run-string")
                  (memory (core memory $i "memory"))
                  (realloc (core func $i "cabi_realloc")))))
        "#;
        assert!(matches!(
            runtime.run(component, 1),
            Err(ComponentError::Run(_))
        ));
    }
}
