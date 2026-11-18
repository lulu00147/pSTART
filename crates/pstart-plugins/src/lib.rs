//! WASM plugin host.
//!
//! M7 implementation: a bounded Wasmtime engine, per-call fuel and a linear
//! memory limit. WIT/Host API extensions remain separate work.

pub mod component;

pub use component::{ComponentError, ComponentRuntime, HostOutput, TextHostOutput};

use pstart_core::limits::{MAX_PLUGIN_INPUT_BYTES, MAX_PLUGIN_OUTPUT_BYTES, MAX_PLUGIN_RESULTS};
use wasmtime::{Config, Engine, Linker, Module, Store, StoreLimitsBuilder, Trap};

/// Resource budget for one plugin call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PluginBudget {
    /// Wasmtime fuel for one call.
    pub fuel: u64,
    /// Maximum linear memory bytes.
    pub max_memory_bytes: usize,
    /// Maximum returned results.
    pub max_results: u32,
    /// Maximum serialized input bytes.
    pub max_input_bytes: usize,
    /// Maximum serialized output bytes.
    pub max_output_bytes: usize,
}

impl PluginBudget {
    /// MVP default budget. Not a security boundary by itself.
    #[must_use]
    pub const fn mvp_default() -> Self {
        Self {
            fuel: 10_000_000,
            max_memory_bytes: 16 * 1024 * 1024,
            max_results: MAX_PLUGIN_RESULTS,
            max_input_bytes: MAX_PLUGIN_INPUT_BYTES,
            max_output_bytes: MAX_PLUGIN_OUTPUT_BYTES,
        }
    }
}

/// Plugin load or execution failure.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    /// Wasmtime engine setup failed.
    #[error("plugin engine setup failed: {0}")]
    Engine(String),
    /// Module compilation failed.
    #[error("plugin compile failed: {0}")]
    Compile(String),
    /// Instantiation or linking failed.
    #[error("plugin instantiation failed: {0}")]
    Instantiate(String),
    /// The plugin did not export a compatible `run` function.
    #[error("plugin is missing run() -> i32: {0}")]
    MissingEntry(String),
    /// Fuel was exhausted before the call completed.
    #[error("plugin exhausted its fuel budget")]
    OutOfFuel,
    /// The plugin trapped for another reason.
    #[error("plugin trapped: {0}")]
    Trap(String),
    /// Input exceeded the configured limit.
    #[error("plugin input exceeds {0} bytes")]
    InputTooLarge(usize),
    /// Output exceeded the configured limit.
    #[error("plugin output exceeds {0} bytes")]
    OutputTooLarge(usize),
    /// The plugin violated the text ABI.
    #[error("plugin ABI violation: {0}")]
    InvalidAbi(String),
    /// Output was not valid UTF-8.
    #[error("plugin output is not UTF-8")]
    InvalidUtf8,
}

/// Shared Wasmtime engine.
pub struct PluginRuntime {
    engine: Engine,
}

impl PluginRuntime {
    /// Create an engine with fuel metering enabled.
    pub fn new() -> Result<Self, PluginError> {
        let mut config = Config::new();
        config.consume_fuel(true);
        let engine =
            Engine::new(&config).map_err(|error| PluginError::Engine(error.to_string()))?;
        Ok(Self { engine })
    }

    /// Compile a binary or WAT module.
    pub fn compile(&self, bytes: impl AsRef<[u8]>) -> Result<Plugin, PluginError> {
        let module = Module::new(&self.engine, bytes)
            .map_err(|error| PluginError::Compile(error.to_string()))?;
        Ok(Plugin { module })
    }
}

impl Default for PluginRuntime {
    fn default() -> Self {
        Self::new().expect("wasmtime engine configuration is fixed")
    }
}

/// A compiled plugin module.
pub struct Plugin {
    module: Module,
}

impl Plugin {
    /// Invoke the exported `run() -> i32` under the given budget.
    pub fn call(&self, runtime: &PluginRuntime, budget: PluginBudget) -> Result<i32, PluginError> {
        let limits = StoreLimitsBuilder::new()
            .memory_size(budget.max_memory_bytes)
            .instances(1)
            .build();
        let mut store = Store::new(&runtime.engine, limits);
        store.limiter(|state| state);
        store
            .set_fuel(budget.fuel)
            .map_err(|error| PluginError::Engine(error.to_string()))?;

        let linker = Linker::new(&runtime.engine);
        let instance = linker
            .instantiate(&mut store, &self.module)
            .map_err(|error| PluginError::Instantiate(error.to_string()))?;
        let run = instance
            .get_typed_func::<(), i32>(&mut store, "run")
            .map_err(|error| PluginError::MissingEntry(error.to_string()))?;
        match run.call(&mut store, ()) {
            Ok(value) => Ok(value),
            Err(error) => Err(map_trap(error)),
        }
    }

    /// Invoke the bounded text ABI:
    /// `alloc(len: i32) -> i32` and `run(ptr: i32, len: i32) -> i64`,
    /// where the result packs `(output_ptr << 32) | output_len`.
    pub fn call_text(
        &self,
        runtime: &PluginRuntime,
        budget: PluginBudget,
        input: &str,
    ) -> Result<String, PluginError> {
        if input.len() > budget.max_input_bytes {
            return Err(PluginError::InputTooLarge(budget.max_input_bytes));
        }
        let limits = StoreLimitsBuilder::new()
            .memory_size(budget.max_memory_bytes)
            .instances(1)
            .build();
        let mut store = Store::new(&runtime.engine, limits);
        store.limiter(|state| state);
        store
            .set_fuel(budget.fuel)
            .map_err(|error| PluginError::Engine(error.to_string()))?;

        let linker = Linker::new(&runtime.engine);
        let instance = linker
            .instantiate(&mut store, &self.module)
            .map_err(|error| PluginError::Instantiate(error.to_string()))?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| PluginError::InvalidAbi("missing memory export".to_owned()))?;
        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, "alloc")
            .map_err(|error| PluginError::MissingEntry(error.to_string()))?;
        let run = instance
            .get_typed_func::<(i32, i32), i64>(&mut store, "run")
            .map_err(|error| PluginError::MissingEntry(error.to_string()))?;

        let input_ptr = alloc
            .call(&mut store, input.len() as i32)
            .map_err(map_trap)?;
        let start = usize::try_from(input_ptr)
            .map_err(|_| PluginError::InvalidAbi("negative input pointer".to_owned()))?;
        let end = start
            .checked_add(input.len())
            .ok_or_else(|| PluginError::InvalidAbi("input pointer overflow".to_owned()))?;
        let data = memory.data_mut(&mut store);
        if end > data.len() {
            return Err(PluginError::InvalidAbi(
                "input allocation exceeds memory".to_owned(),
            ));
        }
        data[start..end].copy_from_slice(input.as_bytes());

        let packed = run
            .call(&mut store, (input_ptr, input.len() as i32))
            .map_err(map_trap)? as u64;
        let output_len = (packed & 0xffff_ffff) as usize;
        let output_ptr = (packed >> 32) as usize;
        if output_len > budget.max_output_bytes {
            return Err(PluginError::OutputTooLarge(budget.max_output_bytes));
        }
        let end = output_ptr
            .checked_add(output_len)
            .ok_or_else(|| PluginError::InvalidAbi("output pointer overflow".to_owned()))?;
        let data = memory.data(&store);
        if end > data.len() {
            return Err(PluginError::InvalidAbi("output exceeds memory".to_owned()));
        }
        String::from_utf8(data[output_ptr..end].to_vec()).map_err(|_| PluginError::InvalidUtf8)
    }
}

fn map_trap(error: wasmtime::Error) -> PluginError {
    if error.downcast_ref::<Trap>() == Some(&Trap::OutOfFuel) {
        PluginError::OutOfFuel
    } else {
        PluginError::Trap(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> PluginRuntime {
        PluginRuntime::new().unwrap()
    }

    #[test]
    fn default_budget_is_bounded() {
        let budget = PluginBudget::mvp_default();
        assert!(budget.fuel > 0);
        assert_eq!(budget.max_results, MAX_PLUGIN_RESULTS);
        assert_eq!(budget.max_output_bytes, MAX_PLUGIN_OUTPUT_BYTES);
        assert!(budget.max_memory_bytes <= 64 * 1024 * 1024);
    }

    #[test]
    fn default_runtime_compiles_a_plugin() {
        let runtime = PluginRuntime::default();
        let plugin = runtime
            .compile(r#"(module (func (export "run") (result i32) i32.const 1))"#)
            .unwrap();
        assert_eq!(
            plugin.call(&runtime, PluginBudget::mvp_default()).unwrap(),
            1
        );
    }

    #[test]
    fn simple_plugin_returns_value() {
        let runtime = runtime();
        let plugin = runtime
            .compile(r#"(module (func (export "run") (result i32) i32.const 42))"#)
            .unwrap();
        assert_eq!(
            plugin.call(&runtime, PluginBudget::mvp_default()).unwrap(),
            42
        );
    }

    #[test]
    fn unreachable_trap_is_reported() {
        let runtime = runtime();
        let plugin = runtime
            .compile(r#"(module (func (export "run") (result i32) unreachable))"#)
            .unwrap();
        assert!(matches!(
            plugin.call(&runtime, PluginBudget::mvp_default()),
            Err(PluginError::Trap(_))
        ));
    }

    #[test]
    fn infinite_loop_exhausts_fuel() {
        let runtime = runtime();
        let plugin = runtime
            .compile(r#"(module (func (export "run") (result i32) (loop br 0) i32.const 0))"#)
            .unwrap();
        let budget = PluginBudget {
            fuel: 1_000,
            ..PluginBudget::mvp_default()
        };
        assert!(matches!(
            plugin.call(&runtime, budget),
            Err(PluginError::OutOfFuel)
        ));
    }

    #[test]
    fn memory_growth_is_bounded() {
        let runtime = runtime();
        let plugin = runtime
            .compile(
                r#"(module
                    (memory 1)
                    (func (export "run") (result i32)
                        i32.const 1000
                        memory.grow
                        drop
                        memory.size))"#,
            )
            .unwrap();
        let budget = PluginBudget {
            max_memory_bytes: 512 * 1024,
            ..PluginBudget::mvp_default()
        };
        let pages = plugin.call(&runtime, budget).unwrap();
        assert!(pages >= 0);
        assert!((pages as usize) * 65_536 <= 512 * 1024);
    }

    #[test]
    fn text_abi_echoes_input() {
        let runtime = runtime();
        let plugin = runtime
            .compile(
                r#"(module
                    (memory (export "memory") 1)
                    (func (export "alloc") (param i32) (result i32) i32.const 0)
                    (func (export "run") (param i32 i32) (result i64)
                        local.get 0
                        i64.extend_i32_u
                        i64.const 32
                        i64.shl
                        local.get 1
                        i64.extend_i32_u
                        i64.or))"#,
            )
            .unwrap();
        assert_eq!(
            plugin
                .call_text(&runtime, PluginBudget::mvp_default(), "hello")
                .unwrap(),
            "hello"
        );
    }

    #[test]
    fn text_input_limit_is_enforced() {
        let runtime = runtime();
        let plugin = runtime
            .compile(
                r#"(module
                    (memory (export "memory") 1)
                    (func (export "alloc") (param i32) (result i32) i32.const 0)
                    (func (export "run") (param i32 i32) (result i64) i64.const 0))"#,
            )
            .unwrap();
        let budget = PluginBudget {
            max_input_bytes: 4,
            ..PluginBudget::mvp_default()
        };
        assert!(matches!(
            plugin.call_text(&runtime, budget, "hello"),
            Err(PluginError::InputTooLarge(4))
        ));
    }

    #[test]
    fn text_output_limit_is_enforced() {
        let runtime = runtime();
        let plugin = runtime
            .compile(
                r#"(module
                    (memory (export "memory") 1)
                    (func (export "alloc") (param i32) (result i32) i32.const 0)
                    (func (export "run") (param i32 i32) (result i64) i64.const 100))"#,
            )
            .unwrap();
        let budget = PluginBudget {
            max_output_bytes: 10,
            ..PluginBudget::mvp_default()
        };
        assert!(matches!(
            plugin.call_text(&runtime, budget, "hello"),
            Err(PluginError::OutputTooLarge(10))
        ));
    }

    #[test]
    fn text_output_must_be_utf8() {
        let runtime = runtime();
        let plugin = runtime
            .compile(
                r#"(module
                    (memory (export "memory") 1)
                    (func (export "alloc") (param i32) (result i32) i32.const 0)
                    (func (export "run") (param i32 i32) (result i64)
                        i32.const 0
                        i32.const 255
                        i32.store8
                        i64.const 1))"#,
            )
            .unwrap();
        assert!(matches!(
            plugin.call_text(&runtime, PluginBudget::mvp_default(), "hello"),
            Err(PluginError::InvalidUtf8)
        ));
    }

    #[test]
    fn missing_text_abi_is_reported() {
        let runtime = runtime();
        let plugin = runtime.compile(r#"(module)"#).unwrap();
        assert!(matches!(
            plugin.call_text(&runtime, PluginBudget::mvp_default(), "x"),
            Err(PluginError::InvalidAbi(_))
        ));
    }

    #[test]
    fn text_alloc_out_of_bounds_is_rejected() {
        let runtime = runtime();
        let plugin = runtime
            .compile(
                r#"(module
                    (memory (export "memory") 1)
                    (func (export "alloc") (param i32) (result i32) i32.const -1)
                    (func (export "run") (param i32 i32) (result i64) i64.const 0))"#,
            )
            .unwrap();
        assert!(matches!(
            plugin.call_text(&runtime, PluginBudget::mvp_default(), "x"),
            Err(PluginError::InvalidAbi(_))
        ));
    }

    #[test]
    fn text_output_out_of_bounds_is_rejected() {
        let runtime = runtime();
        let plugin = runtime
            .compile(
                r#"(module
                    (memory (export "memory") 1)
                    (func (export "alloc") (param i32) (result i32) i32.const 0)
                    (func (export "run") (param i32 i32) (result i64) i64.const 100000))"#,
            )
            .unwrap();
        let budget = PluginBudget {
            max_output_bytes: 200_000,
            ..PluginBudget::mvp_default()
        };
        assert!(matches!(
            plugin.call_text(&runtime, budget, "x"),
            Err(PluginError::InvalidAbi(_))
        ));
    }

    #[test]
    fn text_run_trap_is_reported() {
        let runtime = runtime();
        let plugin = runtime
            .compile(
                r#"(module
                    (memory (export "memory") 1)
                    (func (export "alloc") (param i32) (result i32) i32.const 0)
                    (func (export "run") (param i32 i32) (result i64) unreachable))"#,
            )
            .unwrap();
        assert!(matches!(
            plugin.call_text(&runtime, PluginBudget::mvp_default(), "x"),
            Err(PluginError::Trap(_))
        ));
    }

    #[test]
    fn text_missing_alloc_is_reported() {
        let runtime = runtime();
        let plugin = runtime
            .compile(r#"(module (memory (export "memory") 1))"#)
            .unwrap();
        assert!(matches!(
            plugin.call_text(&runtime, PluginBudget::mvp_default(), "x"),
            Err(PluginError::MissingEntry(_))
        ));
    }

    #[test]
    fn text_input_allocation_beyond_memory_is_rejected() {
        let runtime = runtime();
        let plugin = runtime
            .compile(
                r#"(module
                    (memory (export "memory") 1)
                    (func (export "alloc") (param i32) (result i32) i32.const 65535)
                    (func (export "run") (param i32 i32) (result i64) i64.const 0))"#,
            )
            .unwrap();
        assert!(matches!(
            plugin.call_text(&runtime, PluginBudget::mvp_default(), "hello"),
            Err(PluginError::InvalidAbi(_))
        ));
    }

    #[test]
    fn text_alloc_trap_is_reported() {
        let runtime = runtime();
        let plugin = runtime
            .compile(
                r#"(module
                    (memory (export "memory") 1)
                    (func (export "alloc") (param i32) (result i32) unreachable)
                    (func (export "run") (param i32 i32) (result i64) i64.const 0))"#,
            )
            .unwrap();
        assert!(matches!(
            plugin.call_text(&runtime, PluginBudget::mvp_default(), "x"),
            Err(PluginError::Trap(_))
        ));
    }

    #[test]
    fn invalid_module_is_rejected() {
        let runtime = runtime();
        assert!(matches!(
            runtime.compile(b"not wasm"),
            Err(PluginError::Compile(_))
        ));
    }

    #[test]
    fn missing_run_is_reported() {
        let runtime = runtime();
        let plugin = runtime.compile(r#"(module)"#).unwrap();
        assert!(matches!(
            plugin.call(&runtime, PluginBudget::mvp_default()),
            Err(PluginError::MissingEntry(_))
        ));
    }
}
