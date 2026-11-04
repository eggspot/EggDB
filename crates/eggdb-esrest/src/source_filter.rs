use eggdb_sql::json::{Json, Node};

/// Which parts of a document's `_source` a response returns: all of it, none of it, or the
/// fields its `includes` patterns name minus those its `excludes` patterns name (`*` matches
/// any run of characters, dots included).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SourceFilter {
    Hidden,
    Fields {
        includes: Vec<String>,
        excludes: Vec<String>,
    },
}

impl Default for SourceFilter {
    fn default() -> Self {
        Self::Fields {
            includes: Vec::new(),
            excludes: Vec::new(),
        }
    }
}

fn patterns(node: &Node) -> Option<Vec<String>> {
    match &node.value {
        Json::String(text) => Some(split(text)),
        Json::Array(items) => items
            .iter()
            .map(|item| match &item.value {
                Json::String(text) => Some(text.clone()),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

fn split(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

impl SourceFilter {
    /// The filter a request's `_source` member asks for: `true`, `false`, a pattern, an array
    /// of patterns, or an object with `includes` and `excludes` (or `include` and `exclude`).
    pub(crate) fn from_node(node: &Node) -> Result<Self, String> {
        let invalid = || {
            "[_source] must be a boolean, a string, an array of strings or an object with includes and excludes".to_owned()
        };
        match &node.value {
            Json::Boolean(false) => Ok(Self::Hidden),
            Json::Boolean(true) => Ok(Self::default()),
            Json::String(_) | Json::Array(_) => Ok(Self::Fields {
                includes: patterns(node).ok_or_else(invalid)?,
                excludes: Vec::new(),
            }),
            Json::Object(members) => {
                let mut includes = Vec::new();
                let mut excludes = Vec::new();
                for (key, value) in members {
                    let found = patterns(value).ok_or_else(invalid)?;
                    match key.as_str() {
                        "includes" | "include" => includes = found,
                        "excludes" | "exclude" => excludes = found,
                        other => return Err(format!("unknown key [{other}] in [_source]")),
                    }
                }
                Ok(Self::Fields { includes, excludes })
            }
            _ => Err(invalid()),
        }
    }

    /// The filter a request's URL asks for with `_source`, `_source_includes` and
    /// `_source_excludes`, given each one's value when present.
    pub(crate) fn from_flags(
        source: Option<&str>,
        includes: Option<&str>,
        excludes: Option<&str>,
    ) -> Self {
        let mut include_patterns = match source {
            Some("false") => return Self::Hidden,
            Some("true") | None => Vec::new(),
            Some(text) => split(text),
        };
        if let Some(text) = includes {
            include_patterns = split(text);
        }
        Self::Fields {
            includes: include_patterns,
            excludes: excludes.map(split).unwrap_or_default(),
        }
    }

    /// The document's source as this filter returns it, or `None` when it is hidden.
    pub(crate) fn apply(&self, source: &str) -> Option<String> {
        let Self::Fields { includes, excludes } = self else {
            return None;
        };
        if includes.is_empty() && excludes.is_empty() {
            return Some(source.to_owned());
        }
        let Ok(parsed) = eggdb_sql::json::parse_document(source) else {
            return Some(source.to_owned());
        };
        let Json::Object(members) = &parsed.value else {
            return Some(source.to_owned());
        };
        let kept = Walk { includes, excludes }.members(members, "", includes.is_empty());
        Some(crate::json_out::written(&Node {
            value: Json::Object(kept),
            start: 0,
            end: 0,
        }))
    }
}

struct Walk<'p> {
    includes: &'p [String],
    excludes: &'p [String],
}

impl Walk<'_> {
    fn members(
        &self,
        members: &[(String, Node)],
        prefix: &str,
        included: bool,
    ) -> Vec<(String, Node)> {
        let mut kept = Vec::new();
        for (key, node) in members {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            if let Some(value) = self.value(&node.value, &path, included) {
                kept.push((
                    key.clone(),
                    Node {
                        value,
                        start: node.start,
                        end: node.end,
                    },
                ));
            }
        }
        kept
    }

    fn value(&self, value: &Json, path: &str, included: bool) -> Option<Json> {
        if self.excludes.iter().any(|pattern| matches(pattern, path)) {
            return None;
        }
        let whole = included || self.includes.iter().any(|pattern| matches(pattern, path));
        let below = whole
            || self
                .includes
                .iter()
                .any(|pattern| may_match_below(pattern, path));
        match value {
            Json::Object(members) if below => {
                let kept = self.members(members, path, whole);
                (whole || !kept.is_empty()).then_some(Json::Object(kept))
            }
            Json::Array(items) if below => {
                let kept: Vec<Node> = items
                    .iter()
                    .filter_map(|item| {
                        let value = match &item.value {
                            Json::Object(_) | Json::Array(_) => {
                                self.value(&item.value, path, whole)?
                            }
                            other if whole => other.clone(),
                            _ => return None,
                        };
                        Some(Node {
                            value,
                            start: item.start,
                            end: item.end,
                        })
                    })
                    .collect();
                (whole || !kept.is_empty()).then_some(Json::Array(kept))
            }
            other if whole => Some(other.clone()),
            _ => None,
        }
    }
}

fn may_match_below(pattern: &str, path: &str) -> bool {
    let nested = format!("{path}.");
    let head = pattern.split('*').next().unwrap_or_default();
    if pattern.contains('*') {
        nested.starts_with(head) || head.starts_with(&nested)
    } else {
        pattern.starts_with(&nested)
    }
}

/// Whether `text` matches `pattern`, where `*` matches any run of characters.
pub(crate) fn matches(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, t));
                p += 1;
            }
            Some(&character) if text.get(t) == Some(&character) => {
                p += 1;
                t += 1;
            }
            _ => match star {
                Some((star_at, matched)) => {
                    p = star_at + 1;
                    t = matched + 1;
                    star = Some((star_at, matched + 1));
                }
                None => return false,
            },
        }
    }
    pattern
        .get(p..)
        .is_some_and(|rest| rest.iter().all(|&character| character == '*'))
}

#[cfg(test)]
mod tests {
    use super::{SourceFilter, matches};
    use eggdb_sql::json::parse_document;

    fn filter(spec: &str) -> SourceFilter {
        SourceFilter::from_node(&parse_document(spec).expect("JSON")).expect("a filter")
    }

    const SOURCE: &str = "{\"user\":{\"id\":\"kimchy\",\"name\":\"Shay\"},\"message\":\"hi\",\"tags\":[{\"k\":1,\"v\":2}],\"n\":3}";

    #[test]
    fn patterns_match_with_wildcards() {
        assert!(matches("user.*", "user.id"));
        assert!(matches("*.id", "user.id"));
        assert!(matches("*", "anything.at.all"));
        assert!(matches("us*er", "user"));
        assert!(!matches("user.*", "user"));
        assert!(!matches("message", "messages"));
        assert!(matches("a*b*c", "axxbyyc"));
        assert!(!matches("a*b*c", "axxbyy"));
    }

    #[test]
    fn includes_and_excludes_follow_the_reference() {
        assert_eq!(filter("false").apply(SOURCE), None);
        assert_eq!(filter("true").apply(SOURCE).as_deref(), Some(SOURCE));
        assert_eq!(
            filter("\"user.id\"").apply(SOURCE).as_deref(),
            Some("{\"user\":{\"id\":\"kimchy\"}}")
        );
        assert_eq!(
            filter("[\"user\",\"n\"]").apply(SOURCE).as_deref(),
            Some("{\"user\":{\"id\":\"kimchy\",\"name\":\"Shay\"},\"n\":3}")
        );
        assert_eq!(
            filter("{\"includes\":[\"*\"],\"excludes\":[\"user.name\",\"tags.v\"]}")
                .apply(SOURCE)
                .as_deref(),
            Some("{\"user\":{\"id\":\"kimchy\"},\"message\":\"hi\",\"tags\":[{\"k\":1}],\"n\":3}")
        );
        assert_eq!(
            filter("{\"include\":\"tags.k\"}").apply(SOURCE).as_deref(),
            Some("{\"tags\":[{\"k\":1}]}")
        );
        assert_eq!(
            SourceFilter::from_flags(Some("message,n"), None, None)
                .apply(SOURCE)
                .as_deref(),
            Some("{\"message\":\"hi\",\"n\":3}")
        );
        assert_eq!(
            SourceFilter::from_flags(None, None, Some("user,tags"))
                .apply(SOURCE)
                .as_deref(),
            Some("{\"message\":\"hi\",\"n\":3}")
        );
        assert_eq!(
            SourceFilter::from_flags(Some("false"), None, None).apply(SOURCE),
            None
        );
    }

    #[test]
    fn a_bad_source_member_is_an_error() {
        for spec in ["1", "[1]", "{\"includes\":2}", "{\"other\":[]}", "null"] {
            assert!(
                SourceFilter::from_node(&parse_document(spec).expect("JSON")).is_err(),
                "{spec}"
            );
        }
    }
}
