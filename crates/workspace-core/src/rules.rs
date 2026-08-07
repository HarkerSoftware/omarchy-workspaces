//! The rules engine: declarative auto-assignment of windows to projects.
//!
//! Rules live in `rules.toml`:
//!
//! ```toml
//! [[rules]]
//! name = "firefox-research"     # required, unique
//! priority = 50                 # higher evaluates first; ties keep file order
//! project = "research"          # target project slug
//! group = "browsing"            # optional group slug
//! stop = true                   # default: first match wins
//!
//! [rules.match]                 # all present matchers are ANDed
//! class = { equals = "firefox" }
//! title = { contains = "YouTube" }
//! executable = { regex = ".*/firefox$" }
//! ```
//!
//! Operators are `equals` (case-insensitive unless `case_sensitive = true`),
//! `contains` (same), and `regex`. Matcher keys are provided by a
//! [`MatcherRegistry`]; the built-ins are `class`, `initial_class`, `title`,
//! and `executable`, and plugins can add more — a rule referencing an
//! unregistered key fails validation loudly rather than silently never
//! matching.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::model::Slug;
use crate::world::WindowFacts;

/// Errors from parsing or compiling rules. Each names the offending rule.
#[derive(Debug, thiserror::Error)]
pub enum RuleError {
    /// The TOML failed to parse.
    #[error("invalid rules file: {0}")]
    Toml(#[from] toml::de::Error),
    /// Two rules share a name.
    #[error("duplicate rule name {name:?}")]
    DuplicateName {
        /// The duplicated name.
        name: String,
    },
    /// A rule has no matchers at all.
    #[error("rule {rule:?} has an empty [rules.match] section")]
    NoMatchers {
        /// The rule name.
        rule: String,
    },
    /// A rule references a matcher key no registered matcher provides.
    #[error("rule {rule:?} uses unknown matcher {key:?} (registered: {registered})")]
    UnknownMatcher {
        /// The rule name.
        rule: String,
        /// The unknown key.
        key: String,
        /// Comma-separated registered keys.
        registered: String,
    },
    /// A matcher spec must use exactly one operator.
    #[error("rule {rule:?}, matcher {key:?}: use exactly one of equals/contains/regex")]
    BadOperator {
        /// The rule name.
        rule: String,
        /// The matcher key.
        key: String,
    },
    /// A regex failed to compile.
    #[error("rule {rule:?}, matcher {key:?}: invalid regex: {source}")]
    BadRegex {
        /// The rule name.
        rule: String,
        /// The matcher key.
        key: String,
        /// The regex error.
        #[source]
        source: regex::Error,
    },
    /// A project or group slug in the rule is invalid.
    #[error("rule {rule:?}: {source}")]
    BadSlug {
        /// The rule name.
        rule: String,
        /// The slug error.
        #[source]
        source: crate::model::ModelError,
    },
}

/// The parsed shape of `rules.toml`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RulesFile {
    /// Schema version (reserved).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    /// The rules, in file order.
    #[serde(default)]
    pub rules: Vec<RuleSpec>,
}

/// One `[[rules]]` entry.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RuleSpec {
    /// Unique rule name, used in logs and `rules test`.
    pub name: String,
    /// Higher priorities evaluate first; ties keep file order.
    #[serde(default)]
    pub priority: i32,
    /// Target project slug.
    pub project: String,
    /// Optional target group slug.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Whether evaluation stops after this rule matches (default true).
    #[serde(default = "default_true")]
    pub stop: bool,
    /// Matcher specs keyed by matcher name; ANDed together.
    #[serde(rename = "match", default)]
    pub matchers: BTreeMap<String, MatcherSpec>,
}

fn default_true() -> bool {
    true
}

/// One matcher spec: exactly one operator plus flags.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct MatcherSpec {
    /// Exact match (case-insensitive unless `case_sensitive`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equals: Option<String>,
    /// Substring match (case-insensitive unless `case_sensitive`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,
    /// Regex match (use `(?i)` for case-insensitive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regex: Option<String>,
    /// Make `equals`/`contains` case-sensitive.
    #[serde(default)]
    pub case_sensitive: bool,
}

/// A compiled predicate over [`WindowFacts`]. Plugin extension point.
pub trait CompiledMatcher: Send + Sync {
    /// Whether the window matches.
    fn matches(&self, facts: &WindowFacts) -> bool;
}

/// A matcher factory registered under a key. Plugin extension point.
pub trait RuleMatcher: Send + Sync {
    /// The key used in `[rules.match]`.
    fn key(&self) -> &'static str;
    /// Compile a spec into an executable matcher. `rule` is for diagnostics.
    fn compile(
        &self,
        rule: &str,
        spec: &MatcherSpec,
    ) -> Result<Box<dyn CompiledMatcher>, RuleError>;
}

/// Registry of matcher factories, keyed by `[rules.match]` key.
pub struct MatcherRegistry {
    matchers: HashMap<&'static str, Box<dyn RuleMatcher>>,
}

impl std::fmt::Debug for MatcherRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MatcherRegistry")
            .field("keys", &self.matchers.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl MatcherRegistry {
    /// Registry with the built-in matchers: `class`, `initial_class`,
    /// `title`, `executable`.
    pub fn builtin() -> Self {
        let mut registry = Self {
            matchers: HashMap::new(),
        };
        registry.register(Box::new(FieldMatcher {
            key: "class",
            extract: |facts| Some(facts.class.clone()),
        }));
        registry.register(Box::new(FieldMatcher {
            key: "initial_class",
            extract: |facts| Some(facts.initial_class.clone()),
        }));
        registry.register(Box::new(FieldMatcher {
            key: "title",
            extract: |facts| Some(facts.title.clone()),
        }));
        registry.register(Box::new(FieldMatcher {
            key: "executable",
            extract: |facts| {
                facts
                    .executable
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned())
            },
        }));
        registry
    }

    /// Register a matcher factory (later registrations override earlier ones).
    pub fn register(&mut self, matcher: Box<dyn RuleMatcher>) {
        self.matchers.insert(matcher.key(), matcher);
    }

    fn keys(&self) -> String {
        let mut keys: Vec<_> = self.matchers.keys().copied().collect();
        keys.sort_unstable();
        keys.join(", ")
    }
}

/// Built-in matcher over a string field of [`WindowFacts`].
struct FieldMatcher {
    key: &'static str,
    extract: fn(&WindowFacts) -> Option<String>,
}

impl RuleMatcher for FieldMatcher {
    fn key(&self) -> &'static str {
        self.key
    }

    fn compile(
        &self,
        rule: &str,
        spec: &MatcherSpec,
    ) -> Result<Box<dyn CompiledMatcher>, RuleError> {
        let op = Operator::from_spec(rule, self.key, spec)?;
        let extract = self.extract;
        Ok(Box::new(FieldPredicate { extract, op }))
    }
}

struct FieldPredicate {
    extract: fn(&WindowFacts) -> Option<String>,
    op: Operator,
}

impl CompiledMatcher for FieldPredicate {
    fn matches(&self, facts: &WindowFacts) -> bool {
        (self.extract)(facts).is_some_and(|value| self.op.matches(&value))
    }
}

enum Operator {
    Equals {
        needle: String,
        case_sensitive: bool,
    },
    Contains {
        needle: String,
        case_sensitive: bool,
    },
    Regex(regex::Regex),
}

impl Operator {
    fn from_spec(rule: &str, key: &str, spec: &MatcherSpec) -> Result<Self, RuleError> {
        let count = [
            spec.equals.is_some(),
            spec.contains.is_some(),
            spec.regex.is_some(),
        ]
        .into_iter()
        .filter(|present| *present)
        .count();
        if count != 1 {
            return Err(RuleError::BadOperator {
                rule: rule.to_owned(),
                key: key.to_owned(),
            });
        }
        Ok(if let Some(needle) = &spec.equals {
            Self::Equals {
                needle: needle.clone(),
                case_sensitive: spec.case_sensitive,
            }
        } else if let Some(needle) = &spec.contains {
            Self::Contains {
                needle: needle.clone(),
                case_sensitive: spec.case_sensitive,
            }
        } else {
            let pattern = spec.regex.as_ref().expect("counted above");
            Self::Regex(
                regex::Regex::new(pattern).map_err(|source| RuleError::BadRegex {
                    rule: rule.to_owned(),
                    key: key.to_owned(),
                    source,
                })?,
            )
        })
    }

    fn matches(&self, value: &str) -> bool {
        match self {
            Self::Equals {
                needle,
                case_sensitive: true,
            } => value == needle,
            Self::Equals {
                needle,
                case_sensitive: false,
            } => value.eq_ignore_ascii_case(needle),
            Self::Contains {
                needle,
                case_sensitive: true,
            } => value.contains(needle.as_str()),
            Self::Contains {
                needle,
                case_sensitive: false,
            } => value.to_lowercase().contains(&needle.to_lowercase()),
            Self::Regex(regex) => regex.is_match(value),
        }
    }
}

/// One rule, compiled and ready to evaluate.
pub struct CompiledRule {
    /// Rule name.
    pub name: String,
    /// Rule priority.
    pub priority: i32,
    /// Target project slug.
    pub project: Slug,
    /// Target group slug, if any.
    pub group: Option<Slug>,
    /// Whether evaluation stops when this rule matches.
    pub stop: bool,
    matchers: Vec<Box<dyn CompiledMatcher>>,
}

impl std::fmt::Debug for CompiledRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledRule")
            .field("name", &self.name)
            .field("priority", &self.priority)
            .field("project", &self.project)
            .field("matchers", &self.matchers.len())
            .finish()
    }
}

impl CompiledRule {
    /// Whether all matchers accept the window (matchers are ANDed).
    pub fn matches(&self, facts: &WindowFacts) -> bool {
        self.matchers.iter().all(|matcher| matcher.matches(facts))
    }
}

/// All rules, compiled and sorted for evaluation.
#[derive(Debug, Default)]
pub struct RuleSet {
    rules: Vec<CompiledRule>,
}

impl RuleSet {
    /// Parse and compile a `rules.toml` string. Collects *all* problems
    /// instead of stopping at the first.
    pub fn parse(text: &str, registry: &MatcherRegistry) -> Result<Self, Vec<RuleError>> {
        let file: RulesFile = toml::from_str(text).map_err(|e| vec![RuleError::from(e)])?;
        Self::compile(&file, registry)
    }

    /// Compile a parsed [`RulesFile`].
    pub fn compile(file: &RulesFile, registry: &MatcherRegistry) -> Result<Self, Vec<RuleError>> {
        let mut errors = Vec::new();
        let mut seen = HashSet::new();
        let mut rules = Vec::new();

        for spec in &file.rules {
            if !seen.insert(spec.name.clone()) {
                errors.push(RuleError::DuplicateName {
                    name: spec.name.clone(),
                });
                continue;
            }
            if spec.matchers.is_empty() {
                errors.push(RuleError::NoMatchers {
                    rule: spec.name.clone(),
                });
                continue;
            }
            let project = match Slug::parse(&spec.project) {
                Ok(slug) => slug,
                Err(source) => {
                    errors.push(RuleError::BadSlug {
                        rule: spec.name.clone(),
                        source,
                    });
                    continue;
                }
            };
            let group = match &spec.group {
                None => None,
                Some(raw) => match Slug::parse(raw) {
                    Ok(slug) => Some(slug),
                    Err(source) => {
                        errors.push(RuleError::BadSlug {
                            rule: spec.name.clone(),
                            source,
                        });
                        continue;
                    }
                },
            };
            let mut matchers = Vec::new();
            let mut rule_ok = true;
            for (key, matcher_spec) in &spec.matchers {
                let Some(factory) = registry.matchers.get(key.as_str()) else {
                    errors.push(RuleError::UnknownMatcher {
                        rule: spec.name.clone(),
                        key: key.clone(),
                        registered: registry.keys(),
                    });
                    rule_ok = false;
                    continue;
                };
                match factory.compile(&spec.name, matcher_spec) {
                    Ok(compiled) => matchers.push(compiled),
                    Err(error) => {
                        errors.push(error);
                        rule_ok = false;
                    }
                }
            }
            if rule_ok {
                rules.push(CompiledRule {
                    name: spec.name.clone(),
                    priority: spec.priority,
                    project,
                    group,
                    stop: spec.stop,
                    matchers,
                });
            }
        }

        if !errors.is_empty() {
            return Err(errors);
        }
        // Stable sort keeps file order within equal priorities.
        rules.sort_by_key(|rule| std::cmp::Reverse(rule.priority));
        Ok(Self { rules })
    }

    /// Rules matching the window, in evaluation order, up to and including
    /// the first `stop = true` match. The first element is the winning rule.
    pub fn matches<'a>(&'a self, facts: &WindowFacts) -> Vec<&'a CompiledRule> {
        let mut matched = Vec::new();
        for rule in &self.rules {
            if rule.matches(facts) {
                matched.push(rule);
                if rule.stop {
                    break;
                }
            }
        }
        matched
    }

    /// Number of compiled rules.
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// Whether no rules are loaded.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(class: &str, title: &str) -> WindowFacts {
        WindowFacts {
            class: class.to_owned(),
            initial_class: class.to_owned(),
            title: title.to_owned(),
            executable: Some(format!("/usr/bin/{}", class.to_lowercase()).into()),
            ..Default::default()
        }
    }

    fn parse(text: &str) -> RuleSet {
        RuleSet::parse(text, &MatcherRegistry::builtin()).unwrap()
    }

    #[test]
    fn matches_by_class_case_insensitive_by_default() {
        let rules = parse(
            r#"
            [[rules]]
            name = "ff"
            project = "research"
            [rules.match]
            class = { equals = "Firefox" }
            "#,
        );
        assert_eq!(rules.matches(&facts("firefox", "x"))[0].name, "ff");
        assert!(rules.matches(&facts("chromium", "x")).is_empty());
    }

    #[test]
    fn case_sensitive_and_contains_and_regex() {
        let rules = parse(
            r#"
            [[rules]]
            name = "exact"
            project = "a"
            [rules.match]
            class = { equals = "Code", case_sensitive = true }

            [[rules]]
            name = "yt"
            project = "fun"
            [rules.match]
            title = { contains = "youtube" }

            [[rules]]
            name = "exe"
            project = "dev"
            [rules.match]
            executable = { regex = ".*/code$" }
            "#,
        );
        assert!(
            rules
                .matches(&facts("code", "x"))
                .iter()
                .all(|r| r.name != "exact")
        );
        assert_eq!(rules.matches(&facts("Code", "x"))[0].name, "exact");
        assert_eq!(rules.matches(&facts("mpv", "Cats — YouTube"))[0].name, "yt");
        let mut dev = facts("x", "y");
        dev.executable = Some("/usr/lib/code".into());
        assert_eq!(rules.matches(&dev)[0].name, "exe");
    }

    #[test]
    fn matchers_are_anded() {
        let rules = parse(
            r#"
            [[rules]]
            name = "both"
            project = "p"
            [rules.match]
            class = { equals = "firefox" }
            title = { contains = "GitHub" }
            "#,
        );
        assert!(rules.matches(&facts("firefox", "GitHub — PR")).len() == 1);
        assert!(rules.matches(&facts("firefox", "YouTube")).is_empty());
    }

    #[test]
    fn priority_and_stop_semantics() {
        let rules = parse(
            r#"
            [[rules]]
            name = "low"
            priority = 1
            project = "a"
            [rules.match]
            class = { equals = "kitty" }

            [[rules]]
            name = "tag"
            priority = 10
            project = "b"
            stop = false
            [rules.match]
            class = { equals = "kitty" }

            [[rules]]
            name = "high"
            priority = 5
            project = "c"
            [rules.match]
            class = { equals = "kitty" }
            "#,
        );
        // tag (10, non-stop) matches first, then high (5, stop) ends it;
        // low (1) is never reached.
        let matched: Vec<_> = rules
            .matches(&facts("kitty", "x"))
            .iter()
            .map(|r| r.name.clone())
            .collect();
        assert_eq!(matched, ["tag", "high"]);
    }

    #[test]
    fn all_errors_are_collected_and_named() {
        let err = RuleSet::parse(
            r#"
            [[rules]]
            name = "dup"
            project = "p"
            [rules.match]
            class = { equals = "a" }

            [[rules]]
            name = "dup"
            project = "p"
            [rules.match]
            class = { equals = "a" }

            [[rules]]
            name = "unknown-key"
            project = "p"
            [rules.match]
            wm_role = { equals = "x" }

            [[rules]]
            name = "two-ops"
            project = "p"
            [rules.match]
            class = { equals = "a", contains = "b" }

            [[rules]]
            name = "bad-regex"
            project = "p"
            [rules.match]
            title = { regex = "(" }

            [[rules]]
            name = "empty"
            project = "p"

            [[rules]]
            name = "bad-slug"
            project = "Not A Slug"
            [rules.match]
            class = { equals = "a" }
            "#,
            &MatcherRegistry::builtin(),
        )
        .unwrap_err();
        let messages: Vec<String> = err.iter().map(|e| e.to_string()).collect();
        assert_eq!(messages.len(), 6, "{messages:#?}");
        assert!(messages.iter().any(|m| m.contains("duplicate rule name")));
        assert!(
            messages
                .iter()
                .any(|m| m.contains("unknown matcher \"wm_role\""))
        );
        assert!(messages.iter().any(|m| m.contains("exactly one of")));
        assert!(messages.iter().any(|m| m.contains("invalid regex")));
        assert!(messages.iter().any(|m| m.contains("empty [rules.match]")));
        assert!(messages.iter().any(|m| m.contains("invalid slug")));
    }

    #[test]
    fn empty_file_is_valid() {
        let rules = parse("");
        assert!(rules.is_empty());
        assert!(rules.matches(&facts("a", "b")).is_empty());
    }
}
