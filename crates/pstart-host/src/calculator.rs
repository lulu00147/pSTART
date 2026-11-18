//! Built-in calculator WASM plugin.

use pstart_plugins::{Plugin, PluginBudget, PluginError, PluginRuntime};

/// Compiled calculator plugin and its engine.
pub struct Calculator {
    runtime: PluginRuntime,
    plugin: Plugin,
}

impl Calculator {
    /// Compile the embedded WASM plugin.
    pub fn new() -> Result<Self, PluginError> {
        let runtime = PluginRuntime::new()?;
        let bytes = include_bytes!("../../../plugins/calculator.wasm");
        let plugin = runtime.compile(bytes)?;
        Ok(Self { runtime, plugin })
    }

    /// Evaluate `a op b` with `+`, `-`, or `*`.
    pub fn evaluate(&self, expression: &str) -> Result<String, PluginError> {
        self.plugin
            .call_text(&self.runtime, PluginBudget::mvp_default(), expression)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_decimal_integers() {
        let calculator = Calculator::new().unwrap();
        assert_eq!(calculator.evaluate("2+3").unwrap(), "5");
        assert_eq!(calculator.evaluate("12+34").unwrap(), "46");
        assert_eq!(calculator.evaluate("99999999+1").unwrap(), "100000000");
        assert_eq!(calculator.evaluate("0+0").unwrap(), "0");
    }

    #[test]
    fn subtracts_and_multiplies() {
        let calculator = Calculator::new().unwrap();
        assert_eq!(calculator.evaluate("50-8").unwrap(), "42");
        assert_eq!(calculator.evaluate("8-50").unwrap(), "-42");
        assert_eq!(calculator.evaluate("12*12").unwrap(), "144");
        assert_eq!(calculator.evaluate("9999*9999").unwrap(), "99980001");
        assert_eq!(calculator.evaluate("0-0").unwrap(), "0");
    }

    #[test]
    fn divides_integers() {
        let calculator = Calculator::new().unwrap();
        assert_eq!(calculator.evaluate("50/8").unwrap(), "6");
        assert_eq!(calculator.evaluate("8/50").unwrap(), "0");
        assert_eq!(calculator.evaluate("0/5").unwrap(), "0");
        assert_eq!(calculator.evaluate("5/0").unwrap(), "error");
    }

    #[test]
    fn leading_zeroes_are_accepted() {
        let calculator = Calculator::new().unwrap();
        assert_eq!(calculator.evaluate("0002+0003").unwrap(), "5");
    }

    #[test]
    fn empty_expression_yields_zero() {
        let calculator = Calculator::new().unwrap();
        assert_eq!(calculator.evaluate("").unwrap(), "0");
    }
}
