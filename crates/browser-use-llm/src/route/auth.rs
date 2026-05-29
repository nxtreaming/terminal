//! `Auth` — a small composable credential/header builder. Pure and sync; it
//! produces header `(name, value)` pairs that the async client applies. Kept
//! free of any HTTP-client type so protocols/tests don't pull in `reqwest`.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Auth {
    None,
    Bearer(String),
    Header(String, String),
    Chain(Vec<Auth>),
}

impl Auth {
    pub fn bearer(token: impl Into<String>) -> Self {
        Auth::Bearer(token.into())
    }

    pub fn header(name: impl Into<String>, value: impl Into<String>) -> Self {
        Auth::Header(name.into(), value.into())
    }

    /// Compose two auth steps (both applied, in order).
    pub fn and_then(self, other: Auth) -> Auth {
        match self {
            Auth::Chain(mut v) => {
                v.push(other);
                Auth::Chain(v)
            }
            first => Auth::Chain(vec![first, other]),
        }
    }

    /// Append this auth's header pairs to `headers`.
    pub fn apply(&self, headers: &mut Vec<(String, String)>) {
        match self {
            Auth::None => {}
            Auth::Bearer(token) => {
                headers.push(("authorization".to_string(), format!("Bearer {token}")));
            }
            Auth::Header(name, value) => {
                headers.push((name.clone(), value.clone()));
            }
            Auth::Chain(steps) => {
                for step in steps {
                    step.apply(headers);
                }
            }
        }
    }

    /// Convenience: the header pairs this auth produces.
    pub fn headers(&self) -> Vec<(String, String)> {
        let mut h = Vec::new();
        self.apply(&mut h);
        h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_sets_authorization() {
        assert_eq!(
            Auth::bearer("abc").headers(),
            vec![("authorization".to_string(), "Bearer abc".to_string())]
        );
    }

    #[test]
    fn chain_applies_all_in_order() {
        let auth = Auth::bearer("t")
            .and_then(Auth::header("x-api", "k"))
            .and_then(Auth::header("anthropic-version", "2023-06-01"));
        assert_eq!(
            auth.headers(),
            vec![
                ("authorization".to_string(), "Bearer t".to_string()),
                ("x-api".to_string(), "k".to_string()),
                ("anthropic-version".to_string(), "2023-06-01".to_string()),
            ]
        );
    }

    #[test]
    fn none_is_empty() {
        assert!(Auth::None.headers().is_empty());
    }
}
