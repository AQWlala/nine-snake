use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AclRule {
    pub principal: String,
    pub resource: String,
    pub permission: AclPermission,
    pub effect: AclEffect,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AclPermission {
    Read,
    Write,
    Delete,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AclEffect {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Default)]
pub struct MemoryAcl {
    rules: Vec<AclRule>,
}

impl MemoryAcl {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_rule(&mut self, rule: AclRule) {
        self.rules.push(rule);
    }

    pub fn remove_rule(&mut self, principal: &str, resource: &str, permission: AclPermission) {
        self.rules.retain(|r| {
            !(r.principal == principal && r.resource == resource && r.permission == permission)
        });
    }

    pub fn check(&self, principal: &str, resource: &str, permission: AclPermission) -> bool {
        let mut has_deny = false;
        let mut has_allow = false;

        for rule in &self.rules {
            if !Self::matches(&rule.principal, principal) {
                continue;
            }
            if !Self::matches(&rule.resource, resource) {
                continue;
            }
            if rule.permission != permission {
                continue;
            }
            match rule.effect {
                AclEffect::Deny => has_deny = true,
                AclEffect::Allow => has_allow = true,
            }
        }

        if has_deny {
            return false;
        }
        if has_allow {
            return true;
        }
        true
    }

    pub fn filter_memories<'a>(
        &self,
        principal: &str,
        memories: Vec<(String, &'a str)>,
    ) -> Vec<(String, &'a str)> {
        memories
            .into_iter()
            .filter(|(id, _)| self.check(principal, id, AclPermission::Read))
            .collect()
    }

    fn matches(pattern: &str, value: &str) -> bool {
        if pattern == "*" {
            return true;
        }
        pattern == value
    }

    pub fn rules(&self) -> &[AclRule] {
        &self.rules
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_allows_everything() {
        let acl = MemoryAcl::new();
        assert!(acl.check("anyone", "any-resource", AclPermission::Read));
    }

    #[test]
    fn deny_overrides_allow() {
        let mut acl = MemoryAcl::new();
        acl.add_rule(AclRule {
            principal: "user-1".into(),
            resource: "mem-1".into(),
            permission: AclPermission::Read,
            effect: AclEffect::Deny,
        });
        assert!(!acl.check("user-1", "mem-1", AclPermission::Read));
    }

    #[test]
    fn wildcard_principal() {
        let mut acl = MemoryAcl::new();
        acl.add_rule(AclRule {
            principal: "*".into(),
            resource: "secret".into(),
            permission: AclPermission::Read,
            effect: AclEffect::Deny,
        });
        assert!(!acl.check("anyone", "secret", AclPermission::Read));
        assert!(acl.check("anyone", "public", AclPermission::Read));
    }

    #[test]
    fn filter_removes_denied() {
        let mut acl = MemoryAcl::new();
        acl.add_rule(AclRule {
            principal: "user-1".into(),
            resource: "mem-1".into(),
            permission: AclPermission::Read,
            effect: AclEffect::Deny,
        });
        let mems = vec![
            ("mem-1".to_string(), "content-1"),
            ("mem-2".to_string(), "content-2"),
        ];
        let filtered = acl.filter_memories("user-1", mems);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].0, "mem-2");
    }
}
