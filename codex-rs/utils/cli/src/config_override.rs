//! Support for `-c key=value` overrides shared across Codex CLI tools.
//!
//! This module provides a [`CliConfigOverrides`] struct that can be embedded
//! into a `clap`-derived CLI struct using `#[clap(flatten)]`. Each occurrence
//! of `-c key=value` (or `--config key=value`) will be collected as a raw
//! string. A helper method converts the raw strings into key/value pairs.

use std::ffi::OsString;

use clap::ArgAction;
use clap::ArgMatches;
use clap::Args;
use clap::Command;
use clap::Parser;
use serde::de::Error as SerdeError;
use toml::Value;

/// Clap id of [`CliConfigOverrides::raw_overrides`].
const RAW_OVERRIDES_ID: &str = "raw_overrides";

/// CLI option that captures arbitrary configuration overrides specified as
/// `-c key=value`. It intentionally keeps both halves **unparsed** so that the
/// calling code can decide how to interpret the right-hand side.
#[derive(Parser, Debug, Default, Clone)]
pub struct CliConfigOverrides {
    /// Override a configuration value that would otherwise be loaded from
    /// `~/.codex/config.toml`. Use a dotted path (`foo.bar.baz`) to override
    /// nested values. The `value` portion is parsed as TOML. If it fails to
    /// parse as TOML, the raw string is used as a literal.
    ///
    /// Examples:
    ///   - `-c model="o3"`
    ///   - `-c 'sandbox_permissions=["disk-full-read-access"]'`
    ///   - `-c shell_environment_policy.inherit=all`
    #[arg(
        short = 'c',
        long = "config",
        value_name = "key=value",
        action = ArgAction::Append,
        global = true,
    )]
    pub raw_overrides: Vec<String>,
}

impl CliConfigOverrides {
    /// Prepend root-level config flags so they have lower precedence than
    /// command-specific flags parsed after a subcommand.
    pub fn prepend_root_overrides(&mut self, root_overrides: Self) {
        self.raw_overrides
            .splice(0..0, root_overrides.raw_overrides);
    }

    /// Collects `-c` values given at every command level of `args`, in
    /// command-line order.
    ///
    /// The flag is global, and clap copies the values of the deepest
    /// subcommand that received it into every ancestor, replacing values given
    /// before that subcommand. Parsing `args` against `command` with the flag
    /// scoped to each level recovers all of them. Returns `None` when `args`
    /// do not parse.
    pub fn from_every_command_level<I, T>(command: Command, args: I) -> Option<Self>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let matches = Self::scope_to_each_command_level(command)
            .try_get_matches_from(args)
            .ok()?;
        Some(Self::from_command_levels(&matches))
    }

    /// Declares `-c` as a non-global argument on `command` and on every
    /// subcommand, so each level's matches keep only that level's values.
    pub fn scope_to_each_command_level(command: Command) -> Command {
        let mut command = if command
            .get_arguments()
            .any(|arg| arg.get_id() == RAW_OVERRIDES_ID)
        {
            command
        } else {
            Self::augment_args(command)
        }
        .mut_arg(RAW_OVERRIDES_ID, |arg| arg.global(false));
        for subcommand in command.get_subcommands_mut() {
            *subcommand = Self::scope_to_each_command_level(std::mem::take(subcommand));
        }
        command
    }

    /// Concatenates `-c` values from `matches` and its subcommand chain,
    /// outermost level first. Expects matches from a command prepared by
    /// [`Self::scope_to_each_command_level`].
    pub fn from_command_levels(matches: &ArgMatches) -> Self {
        let mut raw_overrides = Vec::new();
        let mut level = Some(matches);
        while let Some(matches) = level {
            if let Ok(Some(values)) = matches.try_get_many::<String>(RAW_OVERRIDES_ID) {
                raw_overrides.extend(values.cloned());
            }
            level = matches.subcommand().map(|(_, matches)| matches);
        }
        Self { raw_overrides }
    }

    /// Parse the raw strings captured from the CLI into a list of `(path,
    /// value)` tuples where `value` is a `toml::Value`.
    pub fn parse_overrides(&self) -> Result<Vec<(String, Value)>, String> {
        self.raw_overrides
            .iter()
            .map(|s| {
                // Only split on the *first* '=' so values are free to contain
                // the character.
                let (key, value_str) = s
                    .split_once('=')
                    .ok_or_else(|| format!("Invalid override (missing '='): {s}"))?;
                let key = key.trim();
                let value_str = value_str.trim();

                if key.is_empty() {
                    return Err(format!("Empty key in override: {s}"));
                }

                // Attempt to parse as TOML. If that fails, treat it as a raw
                // string. This allows convenient usage such as
                // `-c model=o3` without the quotes.
                let value: Value = match parse_toml_value(value_str) {
                    Ok(v) => v,
                    Err(_) => Value::String(value_str.to_string()),
                };

                Ok((key.to_string(), value))
            })
            .collect()
    }
}

fn parse_toml_value(raw: &str) -> Result<Value, toml::de::Error> {
    let wrapped = format!("_x_ = {raw}");
    let table: toml::Table = toml::from_str(&wrapped)?;
    if table.len() != 1 {
        return Err(SerdeError::custom("expected a single TOML value"));
    }
    table
        .get("_x_")
        .cloned()
        .ok_or_else(|| SerdeError::custom("missing sentinel key"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::FromArgMatches;

    /// Mirrors the Codex CLIs: the root and `own` (like `codex mcp`) declare
    /// `-c`, while `inherited` (like `codex exec`) and `leaf` accept it only
    /// through global propagation.
    fn nested_command() -> Command {
        CliConfigOverrides::augment_args(Command::new("codex"))
            .subcommand(
                CliConfigOverrides::augment_args(Command::new("own"))
                    .subcommand(Command::new("leaf")),
            )
            .subcommand(Command::new("inherited"))
    }

    #[test]
    fn collects_overrides_from_every_command_level_in_order() {
        let root_and_child = ["codex", "-c", "a=1", "inherited", "-c", "a=2"];
        // Plain clap parsing keeps only the deepest level's values.
        let matches = nested_command()
            .try_get_matches_from(root_and_child)
            .expect("parse");
        assert_eq!(
            CliConfigOverrides::from_arg_matches(&matches)
                .expect("root overrides")
                .raw_overrides,
            vec!["a=2"]
        );

        for (args, expected) in [
            (root_and_child.to_vec(), vec!["a=1", "a=2"]),
            (
                vec!["codex", "-c", "a=1", "own", "-c", "a=2", "leaf", "-c", "a=3"],
                vec!["a=1", "a=2", "a=3"],
            ),
            (vec!["codex", "own", "leaf", "--config", "a=3"], vec!["a=3"]),
            (vec!["codex", "-c", "a=1", "own"], vec!["a=1"]),
        ] {
            assert_eq!(
                CliConfigOverrides::from_every_command_level(nested_command(), args.clone())
                    .expect("parse")
                    .raw_overrides,
                expected,
                "{args:?}"
            );
        }
    }

    #[test]
    fn parses_basic_scalar() {
        let v = parse_toml_value("42").expect("parse");
        assert_eq!(v.as_integer(), Some(42));
    }

    #[test]
    fn parses_bool() {
        let true_literal = parse_toml_value("true").expect("parse");
        assert_eq!(true_literal.as_bool(), Some(true));

        let false_literal = parse_toml_value("false").expect("parse");
        assert_eq!(false_literal.as_bool(), Some(false));
    }

    #[test]
    fn fails_on_unquoted_string() {
        assert!(parse_toml_value("hello").is_err());
    }

    #[test]
    fn parses_array() {
        let v = parse_toml_value("[1, 2, 3]").expect("parse");
        assert_eq!(v, Value::Array(vec![1.into(), 2.into(), 3.into()]));
    }

    #[test]
    fn cli_overrides_preserve_invalid_literals_and_decode_valid_strings() {
        for (raw, expected) in [
            ("'unfinished", "'unfinished"),
            ("\"unfinished", "\"unfinished"),
            ("42\nignored = 7", "42\nignored = 7"),
            ("42\n[ignored]\nx = 7", "42\n[ignored]\nx = 7"),
            ("'quoted'", "quoted"),
            ("\"\"\"first\nsecond\"\"\"", "first\nsecond"),
        ] {
            let overrides =
                CliConfigOverrides::try_parse_from(["codex", "-c", &format!("value={raw}")])
                    .expect("CLI override");
            assert_eq!(
                overrides.parse_overrides().expect("parse"),
                vec![("value".to_string(), Value::String(expected.to_string()))]
            );
        }
    }

    #[test]
    fn prepends_root_overrides() {
        let mut subcommand_overrides = CliConfigOverrides {
            raw_overrides: vec![r#"model="gpt-5.2""#.to_string()],
        };
        subcommand_overrides.prepend_root_overrides(CliConfigOverrides {
            raw_overrides: vec![r#"model="gpt-5.1""#.to_string()],
        });

        assert_eq!(
            subcommand_overrides.raw_overrides,
            vec![
                r#"model="gpt-5.1""#.to_string(),
                r#"model="gpt-5.2""#.to_string(),
            ]
        );
    }

    #[test]
    fn parses_inline_table() {
        let v = parse_toml_value("{a = 1, b = 2}").expect("parse");
        let tbl = v.as_table().expect("table");
        assert_eq!(tbl.get("a").unwrap().as_integer(), Some(1));
        assert_eq!(tbl.get("b").unwrap().as_integer(), Some(2));
    }
}
