//! The single source of truth for how projects and groups map to Hyprland
//! named workspaces.
//!
//! Scheme: a project's primary workspace is `<prefix><slug>`; a group's
//! parking workspace (where hidden groups' windows live) is
//! `<prefix><slug><SEP><group>`. The separator is isolated here so it can be
//! swapped in one place if Hyprland's dispatcher grammar ever misbehaves with
//! `:` inside `name:` targets. `@` is reserved for future per-monitor
//! secondary workspaces and rejected in slugs by [`crate::model::Slug`].

use crate::model::Slug;

/// Separator between project slug and group slug in workspace names.
const SEP: char = ':';

/// Workspace name for a project's primary workspace (without the `name:`
/// dispatcher prefix), e.g. `web-dev` or `ws:web-dev` with a prefix.
pub fn project_workspace(prefix: &str, project: &Slug) -> String {
    format!("{prefix}{project}")
}

/// Workspace name for a group's parking workspace, e.g. `web-dev:backend`.
pub fn group_workspace(prefix: &str, project: &Slug, group: &Slug) -> String {
    format!("{prefix}{project}{SEP}{group}")
}

/// Dispatcher target for a workspace name, e.g. `name:web-dev`.
pub fn dispatch_target(workspace_name: &str) -> String {
    format!("name:{workspace_name}")
}

/// What a live Hyprland workspace name means to us, per [`parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedName {
    /// A project primary workspace.
    Project(Slug),
    /// A group parking workspace.
    Group {
        /// The owning project's slug.
        project: Slug,
        /// The group's slug.
        group: Slug,
    },
    /// Not one of ours (numeric, special, user-named, or wrong prefix).
    Foreign,
}

/// Classify a live workspace name against our naming scheme.
///
/// This only checks shape; the daemon must additionally verify the slug
/// belongs to a known project before claiming the workspace.
pub fn parse(prefix: &str, workspace_name: &str) -> ParsedName {
    let Some(rest) = workspace_name.strip_prefix(prefix) else {
        return ParsedName::Foreign;
    };
    match rest.split_once(SEP) {
        None => match Slug::parse(rest) {
            Ok(slug) => ParsedName::Project(slug),
            Err(_) => ParsedName::Foreign,
        },
        Some((proj, group)) => match (Slug::parse(proj), Slug::parse(group)) {
            (Ok(project), Ok(group)) => ParsedName::Group { project, group },
            _ => ParsedName::Foreign,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slug(s: &str) -> Slug {
        Slug::parse(s).unwrap()
    }

    #[test]
    fn names_without_prefix() {
        assert_eq!(project_workspace("", &slug("web-dev")), "web-dev");
        assert_eq!(
            group_workspace("", &slug("web-dev"), &slug("backend")),
            "web-dev:backend"
        );
        assert_eq!(dispatch_target("web-dev"), "name:web-dev");
    }

    #[test]
    fn names_with_prefix() {
        assert_eq!(project_workspace("ws:", &slug("ai")), "ws:ai");
        assert_eq!(group_workspace("ws:", &slug("ai"), &slug("nb")), "ws:ai:nb");
    }

    #[test]
    fn parse_round_trips() {
        assert_eq!(parse("", "web-dev"), ParsedName::Project(slug("web-dev")));
        assert_eq!(
            parse("", "web-dev:backend"),
            ParsedName::Group {
                project: slug("web-dev"),
                group: slug("backend")
            }
        );
        assert_eq!(
            parse("ws:", "ws:web-dev"),
            ParsedName::Project(slug("web-dev"))
        );
    }

    #[test]
    fn parse_rejects_foreign() {
        // Wrong prefix, invalid slugs, user-style names.
        assert_eq!(parse("ws:", "web-dev"), ParsedName::Foreign);
        assert_eq!(parse("", "Web Dev"), ParsedName::Foreign);
        assert_eq!(parse("", "special:magic:extra:parts"), ParsedName::Foreign);
        assert_eq!(parse("", ""), ParsedName::Foreign);
    }
}
