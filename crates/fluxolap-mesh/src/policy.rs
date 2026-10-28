//! What a node does when a friend announces a database it doesn't have, or
//! one newer than its copy: import it by itself, only show it, or leave it.

pub use fluxolap_api_types::MeshPolicy as Mode;

/// `friend/database=mode` rules, first match wins; `*` in either place
/// matches anything. Without a match the mode is `notify`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rules(Vec<Rule>);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    friend: String,
    database: String,
    mode: Mode,
}

fn mode(word: &str) -> Result<Mode, String> {
    match word.trim() {
        "auto" => Ok(Mode::Auto),
        "notify" => Ok(Mode::Notify),
        "off" => Ok(Mode::Off),
        other => Err(format!("{other:?} is not auto, notify or off")),
    }
}

impl Rules {
    /// Parses `FLUXOLAP_MESH_IMPORT`: a comma-separated list such as
    /// `berlin/Sales=auto,*/Weather=auto,*/*=notify`, or just a mode, which
    /// stands for `*/*=<mode>`. A database name can't contain a comma.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(Self::default());
        }
        if let Ok(all) = mode(text) {
            return Ok(Self(vec![Rule { friend: "*".into(), database: "*".into(), mode: all }]));
        }
        let mut rules = Vec::new();
        for part in text.split(',') {
            let (pattern, word) = part.rsplit_once('=').ok_or_else(|| format!("{part:?}: expected friend/database=mode"))?;
            let (friend, database) = pattern.trim().split_once('/').ok_or_else(|| format!("{part:?}: expected friend/database=mode"))?;
            rules.push(Rule { friend: friend.to_string(), database: database.to_string(), mode: mode(word).map_err(|e| format!("{part:?}: {e}"))? });
        }
        Ok(Self(rules))
    }

    pub fn mode_for(&self, friend: &str, database: &str) -> Mode {
        let matches = |pattern: &str, name: &str| pattern == "*" || pattern == name;
        self.0.iter().find(|r| matches(&r.friend, friend) && matches(&r.database, database)).map_or(Mode::Notify, |r| r.mode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_matching_rule_wins_and_the_default_is_notify() {
        let rules = Rules::parse("berlin/Sales=auto, */Sales=off, */*=notify").unwrap();
        assert_eq!(rules.mode_for("berlin", "Sales"), Mode::Auto);
        assert_eq!(rules.mode_for("wien", "Sales"), Mode::Off);
        assert_eq!(rules.mode_for("wien", "Costs"), Mode::Notify);
        assert_eq!(Rules::default().mode_for("x", "y"), Mode::Notify);
    }

    #[test]
    fn a_bare_mode_is_for_everything() {
        let rules = Rules::parse("auto").unwrap();
        assert_eq!(rules.mode_for("any", "thing"), Mode::Auto);
        assert_eq!(Rules::parse("").unwrap(), Rules::default());
    }

    #[test]
    fn database_names_may_contain_slashes() {
        let rules = Rules::parse("a/x/y=auto").unwrap();
        assert_eq!(rules.mode_for("a", "x/y"), Mode::Auto);
        assert_eq!(rules.mode_for("a", "x"), Mode::Notify);
    }

    #[test]
    fn bad_rules_are_reported() {
        assert!(Rules::parse("berlin/Sales=sometimes").is_err());
        assert!(Rules::parse("berlin=auto").is_err());
        assert!(Rules::parse("berlin/Sales").is_err());
    }
}
