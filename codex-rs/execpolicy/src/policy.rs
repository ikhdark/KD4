use crate::decision::Decision;
use crate::error::Error;
use crate::error::Result;
use crate::executable_name::executable_lookup_key;
use crate::executable_name::executable_path_lookup_key;
use crate::rule::NetworkRule;
use crate::rule::NetworkRuleProtocol;
use crate::rule::PatternToken;
use crate::rule::PrefixPattern;
use crate::rule::PrefixRule;
use crate::rule::RuleMatch;
use crate::rule::RuleRef;
use crate::rule::normalize_network_rule_host;
use codex_utils_absolute_path::AbsolutePathBuf;
use multimap::MultiMap;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

type HeuristicsFallback<'a> = Option<&'a dyn Fn(&[String]) -> Decision>;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MatchOptions {
    pub resolve_host_executables: bool,
}

#[derive(Clone, Debug)]
pub struct Policy {
    rules_by_program: MultiMap<String, RuleRef>,
    network_rules: Vec<NetworkRule>,
    basename_rules: MultiMap<String, RuleRef>,
    host_executables_by_name: Arc<HashMap<String, Arc<[AbsolutePathBuf]>>>,
}

impl Policy {
    pub fn new(rules_by_program: MultiMap<String, RuleRef>) -> Self {
        Self::from_parts(rules_by_program, Vec::new(), HashMap::new())
    }

    /// `host_executables_by_name` keys must use native executable identity: on Windows,
    /// lowercase names without .exe/.cmd/.bat/.com; elsewhere, exact basenames.
    pub fn from_parts(
        rules_by_program: MultiMap<String, RuleRef>,
        network_rules: Vec<NetworkRule>,
        host_executables_by_name: HashMap<String, Arc<[AbsolutePathBuf]>>,
    ) -> Self {
        Self::from_shared_parts(
            rules_by_program,
            network_rules,
            Arc::new(host_executables_by_name),
        )
    }

    pub(crate) fn from_shared_parts(
        rules_by_program: MultiMap<String, RuleRef>,
        network_rules: Vec<NetworkRule>,
        host_executables_by_name: Arc<HashMap<String, Arc<[AbsolutePathBuf]>>>,
    ) -> Self {
        let mut basename_rules = MultiMap::new();
        for (program, rules) in rules_by_program.iter_all() {
            if !program.contains(['/', '\\']) {
                for rule in rules {
                    basename_rules.insert(executable_lookup_key(program), Arc::clone(rule));
                }
            }
        }
        Self {
            rules_by_program,
            network_rules,
            basename_rules,
            host_executables_by_name,
        }
    }

    pub fn empty() -> Self {
        Self::new(MultiMap::new())
    }

    pub fn rules(&self) -> &MultiMap<String, RuleRef> {
        &self.rules_by_program
    }

    pub fn network_rules(&self) -> &[NetworkRule] {
        &self.network_rules
    }

    pub fn host_executables(&self) -> &HashMap<String, Arc<[AbsolutePathBuf]>> {
        &self.host_executables_by_name
    }

    pub fn get_allowed_prefixes(&self) -> Vec<Vec<String>> {
        let mut prefixes = Vec::new();

        for (_program, rules) in self.rules_by_program.iter_all() {
            for rule in rules {
                if rule.decision != Decision::Allow {
                    continue;
                }

                let mut prefix = Vec::with_capacity(rule.pattern.rest.len() + 1);
                prefix.push(rule.pattern.first.as_ref().to_string());
                prefix.extend(rule.pattern.rest.iter().map(render_pattern_token));
                prefixes.push(prefix);
            }
        }

        prefixes.sort();
        prefixes.dedup();
        prefixes
    }

    pub fn add_prefix_rule(&mut self, prefix: &[String], decision: Decision) -> Result<()> {
        let (first_token, rest) = prefix
            .split_first()
            .ok_or_else(|| Error::InvalidPattern("prefix cannot be empty".to_string()))?;

        let rest = rest
            .iter()
            .map(|token| PatternToken::single(token.clone()))
            .collect::<Result<Vec<_>>>()?;
        PatternToken::single(first_token.clone())?.validate_program()?;
        let first_token = first_token.clone();
        let rule: RuleRef = Arc::new(PrefixRule {
            pattern: PrefixPattern {
                first: Arc::from(first_token.as_str()),
                rest: rest.into(),
            },
            decision,
            justification: None,
        });

        if !first_token.contains(['/', '\\']) {
            self.basename_rules
                .insert(executable_lookup_key(&first_token), Arc::clone(&rule));
        }
        self.rules_by_program.insert(first_token, rule);
        Ok(())
    }

    pub fn add_network_rule(
        &mut self,
        host: &str,
        protocol: NetworkRuleProtocol,
        decision: Decision,
        justification: Option<String>,
    ) -> Result<()> {
        let host = normalize_network_rule_host(host)?;
        if let Some(raw) = justification.as_deref()
            && raw.trim().is_empty()
        {
            return Err(Error::InvalidRule(
                "justification cannot be empty".to_string(),
            ));
        }
        self.network_rules.push(NetworkRule {
            host,
            protocol,
            decision,
            justification,
        });
        Ok(())
    }

    pub fn set_host_executable_paths(&mut self, name: String, paths: Vec<AbsolutePathBuf>) {
        Arc::make_mut(&mut self.host_executables_by_name)
            .insert(executable_lookup_key(&name), paths.into());
    }

    pub fn merge_overlay(&self, overlay: &Policy) -> Policy {
        let mut combined_rules = self.rules_by_program.clone();
        for (program, rules) in overlay.rules_by_program.iter_all() {
            for rule in rules {
                combined_rules.insert(program.clone(), rule.clone());
            }
        }

        let mut combined_network_rules = self.network_rules.clone();
        combined_network_rules.extend(overlay.network_rules.iter().cloned());

        let mut host_executables_by_name = self.host_executables_by_name.clone();
        Arc::make_mut(&mut host_executables_by_name).extend(
            overlay
                .host_executables_by_name
                .iter()
                .map(|(name, paths)| (name.clone(), paths.clone())),
        );

        Policy::from_shared_parts(
            combined_rules,
            combined_network_rules,
            host_executables_by_name,
        )
    }

    /// Projects rules into the proxy's host-wide permissions. Protocol records the
    /// originating request; the last non-prompt decision applies to the whole host.
    pub fn compiled_network_domains(&self) -> (Vec<String>, Vec<String>) {
        let mut allowed = Vec::new();
        let mut denied = Vec::new();
        let mut seen = HashSet::new();

        for rule in self.network_rules.iter().rev() {
            if rule.decision == Decision::Prompt || !seen.insert(&rule.host) {
                continue;
            }
            match rule.decision {
                Decision::Allow => allowed.push(rule.host.clone()),
                Decision::Forbidden => denied.push(rule.host.clone()),
                Decision::Prompt => {}
            }
        }
        allowed.reverse();
        denied.reverse();
        (allowed, denied)
    }

    pub fn check<F>(&self, cmd: &[String], heuristics_fallback: &F) -> Evaluation
    where
        F: Fn(&[String]) -> Decision,
    {
        let matched_rules = self.matches_for_command_with_options(
            cmd,
            Some(heuristics_fallback),
            &MatchOptions::default(),
        );
        Evaluation::from_matches(matched_rules)
    }

    pub fn check_with_options<F>(
        &self,
        cmd: &[String],
        heuristics_fallback: &F,
        options: &MatchOptions,
    ) -> Evaluation
    where
        F: Fn(&[String]) -> Decision,
    {
        let matched_rules =
            self.matches_for_command_with_options(cmd, Some(heuristics_fallback), options);
        Evaluation::from_matches(matched_rules)
    }

    /// Checks multiple commands and aggregates the results.
    ///
    /// # Panics
    /// Panics if `commands` is empty.
    pub fn check_multiple<Commands, F>(
        &self,
        commands: Commands,
        heuristics_fallback: &F,
    ) -> Evaluation
    where
        Commands: IntoIterator,
        Commands::Item: AsRef<[String]>,
        F: Fn(&[String]) -> Decision,
    {
        self.check_multiple_with_options(commands, heuristics_fallback, &MatchOptions::default())
    }

    /// Checks a nonempty command collection with executable resolution options.
    ///
    /// # Panics
    /// Panics if `commands` is empty.
    pub fn check_multiple_with_options<Commands, F>(
        &self,
        commands: Commands,
        heuristics_fallback: &F,
        options: &MatchOptions,
    ) -> Evaluation
    where
        Commands: IntoIterator,
        Commands::Item: AsRef<[String]>,
        F: Fn(&[String]) -> Decision,
    {
        let matched_rules: Vec<RuleMatch> = commands
            .into_iter()
            .flat_map(|command| {
                self.matches_for_command_with_options(
                    command.as_ref(),
                    Some(heuristics_fallback),
                    options,
                )
            })
            .collect();

        Evaluation::from_matches(matched_rules)
    }

    /// Returns matching rules for the given command. If no rules match and
    /// `heuristics_fallback` is provided, returns a single
    /// `HeuristicsRuleMatch` with the decision rendered by
    /// `heuristics_fallback`.
    ///
    /// If `heuristics_fallback.is_some()`, then the returned vector is
    /// guaranteed to be non-empty.
    pub fn matches_for_command(
        &self,
        cmd: &[String],
        heuristics_fallback: HeuristicsFallback<'_>,
    ) -> Vec<RuleMatch> {
        self.matches_for_command_with_options(cmd, heuristics_fallback, &MatchOptions::default())
    }

    pub fn matches_for_command_with_options(
        &self,
        cmd: &[String],
        heuristics_fallback: HeuristicsFallback<'_>,
        options: &MatchOptions,
    ) -> Vec<RuleMatch> {
        let mut matched_rules = Vec::new();
        self.visit_matches(cmd, options, |rule, resolved| {
            matched_rules.push(rule.materialize_match(cmd, resolved));
            false
        });
        if matched_rules.is_empty()
            && let Some(fallback) = heuristics_fallback
        {
            matched_rules.push(RuleMatch::HeuristicsRuleMatch {
                command: cmd.to_vec(),
                decision: fallback(cmd),
            });
        }
        matched_rules
    }

    /// Visits borrowed matches; returning true stops observation, never authorization aggregation.
    pub(crate) fn visit_matches(
        &self,
        cmd: &[String],
        options: &MatchOptions,
        mut visit: impl FnMut(&PrefixRule, Option<&AbsolutePathBuf>) -> bool,
    ) -> bool {
        let Some(first) = cmd.first() else {
            return false;
        };
        let mut matched = false;
        if let Some(rules) = self.rules_by_program.get_vec(first) {
            for rule in rules {
                if rule.pattern.matches_args(&cmd[1..]) {
                    matched = true;
                    if visit(rule, None) {
                        return true;
                    }
                }
            }
        }
        if matched || !options.resolve_host_executables {
            return matched;
        }
        let Ok(program) = AbsolutePathBuf::try_from(first.as_str()) else {
            return false;
        };
        let Some(basename) = executable_path_lookup_key(program.as_path()) else {
            return false;
        };
        if let Some(paths) = self.host_executables_by_name.get(&basename)
            && !paths.iter().any(|path| path == &program)
        {
            return false;
        }
        let Some(rules) = self.basename_rules.get_vec(&basename) else {
            return false;
        };
        let Some(filename) = program.as_path().file_name().and_then(|name| name.to_str()) else {
            return false;
        };
        for rule in rules {
            // Extensionless aliases keep their existing behavior. Explicit suffixes
            // only match that suffix, even though the host-path gate shares an alias.
            let name = rule.program();
            let applicable = if cfg!(windows) {
                name.eq_ignore_ascii_case(filename) || name.eq_ignore_ascii_case(&basename)
            } else {
                name == filename
            };
            if applicable && rule.pattern.matches_args(&cmd[1..]) {
                matched = true;
                if visit(rule, Some(&program)) {
                    return true;
                }
            }
        }
        matched
    }
}

fn render_pattern_token(token: &PatternToken) -> String {
    match token {
        PatternToken::Single(value) => value.clone(),
        PatternToken::Alts(alternatives) => format!("[{}]", alternatives.join("|")),
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Evaluation {
    pub decision: Decision,
    #[serde(rename = "matchedRules")]
    pub matched_rules: Vec<RuleMatch>,
}

impl Evaluation {
    pub fn is_match(&self) -> bool {
        self.matched_rules
            .iter()
            .any(|rule_match| !matches!(rule_match, RuleMatch::HeuristicsRuleMatch { .. }))
    }

    /// Caller is responsible for ensuring that `matched_rules` is non-empty.
    fn from_matches(matched_rules: Vec<RuleMatch>) -> Self {
        let decision = matched_rules.iter().map(RuleMatch::decision).max();
        #[expect(clippy::expect_used)]
        let decision = decision.expect("invariant failed: matched_rules must be non-empty");

        Self {
            decision,
            matched_rules,
        }
    }
}
