//! Docker's `filters` query parameter: `{"label":["a=b"],"status":["running"]}`
//! (or the old `{"label":{"a=b":true}}` form). Values of one key are ORed,
//! keys are ANDed, as in Docker.

use regex::Regex;
use serde_json::Value;

#[derive(Debug, Default)]
pub struct Filters {
    values: Vec<(String, Vec<String>)>,
    /// Compile filter patterns once per request, not once per object.
    names: Vec<NamePattern>,
    ids: Vec<NamePattern>,
}

#[derive(Debug)]
struct NamePattern {
    literal: String,
    regex: Option<Regex>,
}

impl NamePattern {
    fn new(pattern: &str) -> Self {
        Self { literal: pattern.into(), regex: Regex::new(pattern).ok() }
    }

    fn matches(&self, name: &str) -> bool {
        // Moby tests exact strings before regex, including invalid patterns.
        self.literal == name || self.regex.as_ref().is_some_and(|re| re.is_match(name))
    }
}

impl Filters {
    pub fn parse(raw: Option<&str>) -> Result<Filters, String> {
        let Some(raw) = raw.filter(|r| !r.is_empty()) else { return Ok(Filters::default()) };
        let v: Value = serde_json::from_str(raw).map_err(|e| format!("invalid filters: {e}"))?;
        let Value::Object(map) = v else { return Err("invalid filters: not an object".into()) };
        let mut out: Vec<(String, Vec<String>)> = Vec::with_capacity(map.len());
        for (k, v) in map {
            let values = match v {
                Value::Array(a) => a.into_iter().filter_map(|s| s.as_str().map(str::to_owned)).collect(),
                Value::Object(o) => o.into_iter().filter(|(_, on)| on.as_bool().unwrap_or(false)).map(|(s, _)| s).collect(),
                Value::String(s) => vec![s],
                _ => Vec::new(),
            };
            out.push((k, values));
        }
        let compile = |key: &str| out.iter().filter(|(k, _)| k == key).flat_map(|(_, v)| v).map(|p| NamePattern::new(p)).collect();
        let (names, ids) = (compile("name"), compile("id"));
        Ok(Filters { values: out, names, ids })
    }

    /// Rejects keys this list does not understand, as Docker does.
    pub fn only(&self, known: &[&str]) -> Result<(), String> {
        match self.values.iter().find(|(k, _)| !known.contains(&k.as_str())) {
            Some((k, _)) => Err(format!("invalid filter '{k}'")),
            None => Ok(()),
        }
    }

    pub fn values(&self, key: &str) -> Option<&[String]> {
        self.values.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_slice()).filter(|v| !v.is_empty())
    }

    /// True when `key` is absent or any of its values satisfies `f`.
    pub fn any(&self, key: &str, f: impl Fn(&str) -> bool) -> bool {
        self.values(key).is_none_or(|v| v.iter().any(|s| f(s)))
    }

    /// `label=key` or `label=key=value` against a labels object; every label
    /// filter must match (Docker ANDs labels). `label!=` must not match.
    pub fn labels(&self, labels: &Value) -> bool {
        let has = |want: &str| match want.split_once('=') {
            Some((k, v)) => labels.get(k).and_then(Value::as_str) == Some(v),
            None => labels.get(want).is_some(),
        };
        self.values("label").is_none_or(|v| v.iter().all(|w| has(w))) && self.values("label!").is_none_or(|v| !v.iter().any(|w| has(w)))
    }

    /// Volumes and networks match their literal name. Patterns are compiled
    /// once per request rather than once per listed resource.
    pub fn name(&self, name: &str) -> bool {
        self.names.is_empty() || self.names.iter().any(|p| p.matches(name))
    }

    /// Only containers also have Docker's slash-prefixed name alias.
    pub fn container_name(&self, name: &str) -> bool {
        let bare = name.strip_prefix('/').unwrap_or(name);
        self.name(bare) || self.name(&format!("/{bare}"))
    }

    /// A network's `id` filter, which Docker matches like `name`: exactly,
    /// or as a regular expression (k3d looks its network up with
    /// `^/?<id>$`). An ID prefix is a regular expression that matches too.
    /// Containers' `id` filter is a prefix in Docker, not this.
    pub fn network_id(&self, id: &str) -> bool {
        self.ids.is_empty() || self.ids.iter().any(|p| p.matches(id))
    }

    /// `true`/`1` or `false`/`0` for a boolean filter, if given.
    pub fn bool(&self, key: &str) -> Option<bool> {
        self.values(key).and_then(|v| v.first()).map(|s| matches!(s.as_str(), "true" | "1"))
    }
}

#[cfg(test)]
fn name_matches(pattern: &str, name: &str) -> bool {
    Filters { names: vec![NamePattern::new(pattern)], ..Filters::default() }.container_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_both_forms() {
        let f = Filters::parse(Some(r#"{"label":["a=b"],"status":{"running":true}}"#)).unwrap();
        assert_eq!(f.values("label").unwrap(), ["a=b"]);
        assert_eq!(f.values("status").unwrap(), ["running"]);
        assert!(f.values("name").is_none());
    }

    #[test]
    fn labels_and_together() {
        let f = Filters::parse(Some(r#"{"label":["p=x","s"]}"#)).unwrap();
        assert!(f.labels(&json!({"p":"x","s":"web"})));
        assert!(!f.labels(&json!({"p":"x"})));
        assert!(!f.labels(&json!({"p":"y","s":"web"})));
        assert!(Filters::default().labels(&json!({})));
    }

    #[test]
    fn values_or_together() {
        let f = Filters::parse(Some(r#"{"name":["a","b"]}"#)).unwrap();
        assert!(f.any("name", |n| n == "b"));
        assert!(!f.any("name", |n| n == "c"));
    }

    #[test]
    fn network_id_filter_is_a_regex_like_docker() {
        let id = "72007c172e7879167e7f562d3b53d3fb15874b9702bc4ab1bb19a28e1b4c676c";
        let f = |v: &str| Filters::parse(Some(&json!({ "id": [v] }).to_string())).unwrap().network_id(id);
        // k3d finds its cluster network with `^/?<id>$`.
        assert!(f(&format!("^/?{id}$")));
        assert!(f(id), "the full id");
        assert!(f("72007c17"), "a short id, as `docker network ls -f id=` takes");
        assert!(!f(&format!("^/?{}$", &id[..12])), "anchored to the whole id");
        assert!(!f("deadbeef"));
        assert!(Filters::default().network_id(id));
    }

    #[test]
    fn name_filter_is_anchored_like_docker() {
        assert!(name_matches("^kind$", "kind"));
        assert!(!name_matches("^kind$", "kind-2"));
        assert!(!name_matches("^kind$", "mykind"));
        assert!(name_matches("^kind", "kind-2"));
        assert!(name_matches("plane$", "dev-control-plane"));
        assert!(name_matches("ind", "kind"));
        assert!(name_matches("^/web$", "web"));
        assert!(name_matches("^web$", "/web"));
        let f = Filters::parse(Some(r#"{"name":["^kind$"]}"#)).unwrap();
        assert!(f.name("kind") && !f.name("kind-old"));
        assert!(f.any("status", |_| false));
    }

    #[test]
    fn name_filter_supports_regex_and_container_slash_alias() {
        let f = Filters::parse(Some(r#"{"name":["^/aoscp-(floci|localstack)-demo$"]}"#)).unwrap();
        assert!(f.container_name("aoscp-floci-demo"));
        assert!(f.container_name("/aoscp-localstack-demo"));
        assert!(!f.container_name("aoscp-floci-demo-old"));
        assert!(!f.container_name("aoscp-other-demo"));
        for (pattern, yes, no) in [
            (r"^worker-[0-9]{2}$", "worker-42", "worker-x2"),
            (r"^app\.v[12]$", "app.v2", "appXv2"),
            (r"^web(-old)?$", "web-old", "web-older"),
            (r"api|worker", "my-worker-1", "database"),
            (r"^/", "web", ""),
        ] {
            assert!(name_matches(pattern, yes), "{pattern} should match {yes}");
            if !no.is_empty() {
                assert!(!name_matches(pattern, no), "{pattern} should not match {no}");
            }
        }
        assert!(name_matches(r"^(/web|worker)$", "web"));
        assert!(name_matches(r"^(/web|worker)$", "/worker"));
    }

    #[test]
    fn invalid_regex_is_ignored_but_exact_match_precedes_regex() {
        assert!(!name_matches("[", "web"));
        assert!(name_matches("[", "["));
        let f = Filters::parse(Some(r#"{"name":["[","^worker-[0-9]+$"]}"#)).unwrap();
        assert!(f.name("worker-12"));
        assert!(!f.name("web"));
        assert!(Filters::parse(Some(r#"{"name":[]}"#)).unwrap().name("anything"));
    }

    #[test]
    fn volume_and_network_name_filters_do_not_add_a_slash_alias() {
        let bare = Filters::parse(Some(r#"{"name":["^data-(one|two)$"]}"#)).unwrap();
        assert!(bare.name("data-one"));
        assert!(bare.name("data-two"));
        assert!(!bare.name("data-three"));
        let slash = Filters::parse(Some(r#"{"name":["^/data-(one|two)$"]}"#)).unwrap();
        assert!(!slash.name("data-one"));
        assert!(slash.container_name("data-one"));
        assert!(!bare.name("/data-one"));
    }

    #[test]
    fn rejects_unknown() {
        let f = Filters::parse(Some(r#"{"bogus":["x"]}"#)).unwrap();
        assert!(f.only(&["label"]).is_err());
        assert!(Filters::parse(Some("[1]")).is_err());
    }
}
