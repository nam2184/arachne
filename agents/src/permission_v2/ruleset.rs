use serde::{Deserialize, Serialize};

use super::rule::{PermissionAction, PermissionRule};
use super::wildcard::wildcard_match;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRuleset {
    pub rules: Vec<PermissionRule>,
}

impl PermissionRuleset {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_rules(mut self, rules: Vec<PermissionRule>) -> Self {
        self.rules = rules;
        self
    }

    pub fn push(mut self, rule: PermissionRule) -> Self {
        self.rules.push(rule);
        self
    }

    /// Evaluate a (permission, pattern) pair against the ruleset.
    /// Last matching rule wins.
    pub fn evaluate(&self, permission: &str, pattern: &str) -> PermissionRule {
        self.rules
            .iter()
            .rev()
            .find(|rule| {
                wildcard_match(&rule.permission, permission)
                    && wildcard_match(&rule.pattern, pattern)
            })
            .cloned()
            .unwrap_or_else(|| PermissionRule {
                permission: permission.to_string(),
                pattern: "*".to_string(),
                action: PermissionAction::Ask,
            })
    }

    /// Evaluate across multiple rulesets. Rules are merged into a single list
    /// in the order given (so later rulesets' rules take precedence), and the
    /// standard last-matching-wins evaluation runs against the merged list.
    pub fn evaluate_merged(
        rulesets: &[&PermissionRuleset],
        permission: &str,
        pattern: &str,
    ) -> PermissionRule {
        let merged: Vec<PermissionRule> = rulesets
            .iter()
            .flat_map(|rs| rs.rules.iter().cloned())
            .collect();
        PermissionRuleset { rules: merged }.evaluate(permission, pattern)
    }

    /// Compute the set of tools that are globally denied (rule has pattern `*`
    /// and action `deny` for the tool's permission category).
    pub fn disabled_tools(&self) -> std::collections::HashSet<String> {
        let edits = ["edit", "write", "apply_patch"];
        let mut disabled = std::collections::HashSet::new();
        for tool in [
            "read",
            "glob",
            "grep",
            "edit",
            "write",
            "apply_patch",
            "bash",
            "webfetch",
            "websearch",
            "lsp",
            "skill",
            "todowrite",
            "question",
        ] {
            let permission = if edits.contains(&tool) { "edit" } else { tool };
            if let Some(rule) = self
                .rules
                .iter()
                .rev()
                .find(|r| wildcard_match(&r.permission, permission) && r.pattern == "*")
            {
                if rule.action == PermissionAction::Deny {
                    disabled.insert(tool.to_string());
                }
            }
        }
        disabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rs(rules: Vec<PermissionRule>) -> PermissionRuleset {
        PermissionRuleset { rules }
    }

    #[test]
    fn empty_ruleset_defaults_to_ask() {
        let ruleset = rs(vec![]);
        let result = ruleset.evaluate("bash", "git status");
        assert_eq!(result.action, PermissionAction::Ask);
        assert_eq!(result.permission, "bash");
        assert_eq!(result.pattern, "*");
    }

    #[test]
    fn last_matching_rule_wins() {
        let ruleset = rs(vec![
            PermissionRule::ask("bash", "*"),
            PermissionRule::allow("bash", "git *"),
        ]);
        // "git status" matches both, but the later rule wins.
        assert_eq!(
            ruleset.evaluate("bash", "git status").action,
            PermissionAction::Allow
        );
        // "rm -rf" only matches the first rule.
        assert_eq!(
            ruleset.evaluate("bash", "rm -rf /").action,
            PermissionAction::Ask
        );
    }

    #[test]
    fn deny_overrides_earlier_allow() {
        let ruleset = rs(vec![
            PermissionRule::allow("bash", "git *"),
            PermissionRule::deny("bash", "git push *"),
        ]);
        assert_eq!(
            ruleset.evaluate("bash", "git status").action,
            PermissionAction::Allow
        );
        assert_eq!(
            ruleset.evaluate("bash", "git push origin main").action,
            PermissionAction::Deny
        );
    }

    #[test]
    fn pattern_must_match_both_permission_and_pattern() {
        let ruleset = rs(vec![PermissionRule::allow("read", "src/**")]);
        // Pattern "edit" doesn't match permission "read".
        assert_eq!(
            ruleset.evaluate("edit", "src/main.rs").action,
            PermissionAction::Ask
        );
        // Path matches but tool doesn't.
        assert_eq!(
            ruleset.evaluate("write", "src/main.rs").action,
            PermissionAction::Ask
        );
    }

    #[test]
    fn wildcard_permission_matches_multiple_tools() {
        let ruleset = rs(vec![PermissionRule::deny("*", "*.env")]);
        assert_eq!(
            ruleset.evaluate("read", "production.env").action,
            PermissionAction::Deny
        );
        assert_eq!(
            ruleset.evaluate("write", "production.env").action,
            PermissionAction::Deny
        );
        assert_eq!(
            ruleset.evaluate("edit", "production.env").action,
            PermissionAction::Deny
        );
    }

    #[test]
    fn evaluate_merged_later_ruleset_takes_precedence() {
        // Later rulesets' rules are appended to the merged list, so their
        // matching rules win under last-match semantics.
        let global = rs(vec![PermissionRule::ask("bash", "*")]);
        let agent = rs(vec![PermissionRule::allow("bash", "git *")]);
        let result = PermissionRuleset::evaluate_merged(&[&global, &agent], "bash", "git status");
        assert_eq!(result.action, PermissionAction::Allow);
    }

    #[test]
    fn evaluate_merged_first_ruleset_wins_when_both_match() {
        // When both have a matching rule, the first ruleset is at the
        // beginning of the merged list — but findLast picks the second one
        // (the agent's), so agent wins.
        let global = rs(vec![PermissionRule::deny("bash", "*")]);
        let agent = rs(vec![PermissionRule::allow("bash", "*")]);
        let result = PermissionRuleset::evaluate_merged(&[&global, &agent], "bash", "anything");
        assert_eq!(result.action, PermissionAction::Allow);
    }

    #[test]
    fn evaluate_merged_falls_through_when_no_match() {
        // No rules in either ruleset match the call → default to Ask.
        let global = rs(vec![PermissionRule::allow("bash", "git *")]);
        let agent = rs(vec![]);
        let result = PermissionRuleset::evaluate_merged(&[&agent, &global], "bash", "npm test");
        assert_eq!(result.action, PermissionAction::Ask);
    }

    #[test]
    fn evaluate_merged_different_tools() {
        // Different tools are independent; the agent rule for npm doesn't
        // affect git's resolution.
        let global = rs(vec![PermissionRule::ask("bash", "*")]);
        let agent = rs(vec![PermissionRule::allow("bash", "npm *")]);
        // npm matches the agent rule.
        let npm = PermissionRuleset::evaluate_merged(&[&global, &agent], "bash", "npm test");
        assert_eq!(npm.action, PermissionAction::Allow);
        // git falls through to global.
        let git = PermissionRuleset::evaluate_merged(&[&global, &agent], "bash", "git status");
        assert_eq!(git.action, PermissionAction::Ask);
    }

    #[test]
    fn disabled_tools_finds_globally_denied() {
        let ruleset = rs(vec![
            PermissionRule::deny("bash", "*"),
            PermissionRule::deny("webfetch", "*"),
        ]);
        let disabled = ruleset.disabled_tools();
        assert!(disabled.contains("bash"));
        assert!(disabled.contains("webfetch"));
        assert!(!disabled.contains("read"));
    }

    #[test]
    fn disabled_tools_treats_edits_as_one_category() {
        // A deny on "edit" should disable write/apply_patch too.
        let ruleset = rs(vec![PermissionRule::deny("edit", "*")]);
        let disabled = ruleset.disabled_tools();
        assert!(disabled.contains("edit"));
        assert!(disabled.contains("write"));
        assert!(disabled.contains("apply_patch"));
    }
}
