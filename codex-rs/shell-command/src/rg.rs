pub(crate) const UNSAFE_OPTIONS_WITH_VALUES: &[&str] = &["--pre", "--hostname-bin"];
pub(crate) const UNSAFE_OPTIONS_WITHOUT_VALUES: &[&str] = &["--search-zip", "-z"];

pub(crate) const PATTERN_VALUE_OPTIONS: &[&str] = &["-e", "--regexp"];

pub(crate) const VALUE_OPTIONS: &[&str] = &[
    "-A",
    "-B",
    "-C",
    "-E",
    "-M",
    "-e",
    "-f",
    "-g",
    "-m",
    "-t",
    "-T",
    "--after-context",
    "--before-context",
    "--color",
    "--colors",
    "--context",
    "--context-separator",
    "--dfa-size-limit",
    "--encoding",
    "--engine",
    "--field-context-separator",
    "--field-match-separator",
    "--glob",
    "--iglob",
    "--ignore-file",
    "--max-columns",
    "--max-count",
    "--max-depth",
    "--max-filesize",
    "--path-separator",
    "--pre-glob",
    "--regexp",
    "--regex-size-limit",
    "--sort",
    "--sortr",
    "--threads",
    "--type",
    "--type-add",
    "--type-clear",
    "--type-not",
];

pub(crate) fn is_unsafe_option(arg: &str) -> bool {
    let arg_lc = arg.to_ascii_lowercase();
    UNSAFE_OPTIONS_WITHOUT_VALUES.contains(&arg_lc.as_str())
        || UNSAFE_OPTIONS_WITH_VALUES
            .iter()
            .any(|opt| arg_lc == *opt || arg_lc.starts_with(&format!("{opt}=")))
}

pub(crate) fn is_pattern_value_option(arg: &str) -> bool {
    PATTERN_VALUE_OPTIONS.contains(&arg)
}

pub(crate) fn is_value_option(arg: &str) -> bool {
    VALUE_OPTIONS.contains(&arg)
}

pub(crate) fn split_long_equals(arg: &str) -> Option<(&str, &str)> {
    let (flag, value) = arg.split_once('=')?;
    flag.starts_with("--").then_some((flag, value))
}
