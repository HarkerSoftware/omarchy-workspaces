//! Fuzzy resolution of user queries to projects.
//!
//! Resolution policy (destructive operations use exact matching only):
//! 1. exact slug match wins outright;
//! 2. a unique slug prefix wins;
//! 3. otherwise fuzzy-match against slug and display name (nucleo); the best
//!    score wins only when it is decisive (clearly ahead of the runner-up).
//!
//! Anything else is ambiguous and reported with candidates rather than
//! guessed.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::model::Project;

/// Outcome of resolving a query against the project list.
#[derive(Debug, PartialEq)]
pub enum Resolution<'a> {
    /// One project clearly matches.
    Match(&'a Project),
    /// Several plausible candidates; slugs listed best-first.
    Ambiguous(Vec<&'a Project>),
    /// Nothing matches at all.
    NotFound,
}

/// Multiplier by which the best fuzzy score must beat the runner-up to be
/// considered decisive.
const DECISIVE_RATIO: f32 = 1.5;

/// Resolve `query` to a project. See the module docs for the policy.
pub fn resolve<'a>(query: &str, projects: &'a [Project]) -> Resolution<'a> {
    if projects.is_empty() {
        return Resolution::NotFound;
    }

    if let Some(project) = projects.iter().find(|p| p.slug.as_str() == query) {
        return Resolution::Match(project);
    }

    let prefixed: Vec<&Project> = projects
        .iter()
        .filter(|p| p.slug.as_str().starts_with(query))
        .collect();
    match prefixed.len() {
        1 => return Resolution::Match(prefixed[0]),
        n if n > 1 => return Resolution::Ambiguous(prefixed),
        _ => {}
    }

    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut buf = Vec::new();
    let mut scored: Vec<(u32, &Project)> = projects
        .iter()
        .filter_map(|project| {
            let slug_score =
                pattern.score(Utf32Str::new(project.slug.as_str(), &mut buf), &mut matcher);
            let name_score = pattern.score(Utf32Str::new(&project.name, &mut buf), &mut matcher);
            slug_score
                .into_iter()
                .chain(name_score)
                .max()
                .map(|score| (score, project))
        })
        .collect();
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));

    match scored.as_slice() {
        [] => Resolution::NotFound,
        [(_, only)] => Resolution::Match(only),
        [(best, project), (second, _), ..] => {
            if *best as f32 >= *second as f32 * DECISIVE_RATIO {
                Resolution::Match(project)
            } else {
                Resolution::Ambiguous(scored.iter().map(|(_, p)| *p).collect())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn projects() -> Vec<Project> {
        [
            "Web Development",
            "Machine Learning",
            "Gaming",
            "Web Design",
        ]
        .iter()
        .map(|name| Project::new(name).unwrap())
        .collect()
    }

    #[test]
    fn exact_slug_wins() {
        let projects = projects();
        match resolve("gaming", &projects) {
            Resolution::Match(p) => assert_eq!(p.slug.as_str(), "gaming"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unique_prefix_wins() {
        let projects = projects();
        match resolve("mach", &projects) {
            Resolution::Match(p) => assert_eq!(p.slug.as_str(), "machine-learning"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn shared_prefix_is_ambiguous() {
        let projects = projects();
        match resolve("web", &projects) {
            Resolution::Ambiguous(candidates) => assert_eq!(candidates.len(), 2),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn fuzzy_match_on_display_name() {
        let projects = projects();
        // "learning" is not a slug prefix but fuzzily identifies ML.
        match resolve("learning", &projects) {
            Resolution::Match(p) => assert_eq!(p.slug.as_str(), "machine-learning"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn garbage_is_not_found() {
        let projects = projects();
        assert_eq!(resolve("zzzqqq", &projects), Resolution::NotFound);
        assert_eq!(resolve("x", &[]), Resolution::NotFound);
    }
}
