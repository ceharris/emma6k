//! Reading the CPU variant from a configuration source: `cpu-variant`, or its
//! synonym `cpu`. A source may use either spelling but not both.

use figment::Figment;
use figment::providers::Env;
use serde::{Deserialize, Deserializer};

/// The prefix of environment variables that set configuration keys.
pub const ENV_PREFIX: &str = "EMMA6K_";

/// The `EMMA6K_*` environment variables as a configuration source, with
/// `_` in a key read as `-` (`EMMA6K_CPU_VARIANT` sets `cpu-variant`).
pub fn env_source() -> Env {
    Env::prefixed(ENV_PREFIX).map(|k| k.as_str().replace('_', "-").into())
}

/// The `cpu` and `cpu-variant` keys of one source.
#[derive(Default, Deserialize)]
struct CpuKeys {
    #[serde(default, deserialize_with = "variant_value")]
    cpu: Option<String>,
    #[serde(rename = "cpu-variant", default, deserialize_with = "variant_value")]
    cpu_variant: Option<String>,
}

/// Reads a CPU variant as text. Accepts a number too, since `6809` in an
/// environment variable or unquoted in TOML arrives as one.
pub fn variant_value<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Variant {
        Text(String),
        Number(u64),
    }
    Ok(
        Option::<Variant>::deserialize(deserializer)?.map(|v| match v {
            Variant::Text(s) => s,
            Variant::Number(n) => n.to_string(),
        }),
    )
}

/// Returns the CPU variant one source sets, or an error if it sets both
/// spellings. `name` names the source in error messages.
pub fn variant_in(source: Figment, name: &str) -> Result<Option<String>, String> {
    let keys: CpuKeys = source.extract().map_err(|e| format!("{name}: {e}"))?;
    match keys {
        CpuKeys {
            cpu: Some(_),
            cpu_variant: Some(_),
        } => Err(format!(
            "{name} sets both cpu and cpu-variant; set only one"
        )),
        CpuKeys { cpu, cpu_variant } => Ok(cpu.or(cpu_variant)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use figment::providers::{Format, Toml};

    fn toml(text: &str) -> Result<Option<String>, String> {
        variant_in(Figment::from(Toml::string(text)), "test.toml")
    }

    #[test]
    fn either_spelling_names_the_variant() {
        assert_eq!(toml("cpu = \"6809\"").unwrap().as_deref(), Some("6809"));
        assert_eq!(toml("cpu-variant = 6809").unwrap().as_deref(), Some("6809"));
        assert_eq!(toml("clock-speed-hz = 1").unwrap(), None);
    }

    #[test]
    fn both_spellings_in_one_source_is_an_error() {
        let err = toml("cpu = \"6809\"\ncpu-variant = \"6809\"").unwrap_err();
        assert!(err.contains("test.toml sets both"), "{err}");
    }
}
