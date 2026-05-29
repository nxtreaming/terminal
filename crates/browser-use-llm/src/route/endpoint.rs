//! `Endpoint` — where a request is sent. Pure value; the async client turns it
//! into a URL. `path` is a plain string here; protocols that embed the model id
//! or region in the path build it before constructing the route.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub base_url: String,
    pub path: String,
    pub query: Vec<(String, String)>,
}

impl Endpoint {
    pub fn new(base_url: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            path: path.into(),
            query: Vec::new(),
        }
    }

    pub fn with_query(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.query.push((key.into(), value.into()));
        self
    }

    /// Full URL: `base_url` + `path` (joined with exactly one `/`) + query.
    pub fn url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        let path = self.path.trim_start_matches('/');
        let mut url = if path.is_empty() {
            base.to_string()
        } else {
            format!("{base}/{path}")
        };
        if !self.query.is_empty() {
            url.push('?');
            let q: Vec<String> = self.query.iter().map(|(k, v)| format!("{k}={v}")).collect();
            url.push_str(&q.join("&"));
        }
        url
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_with_single_slash() {
        assert_eq!(
            Endpoint::new("https://api.example.com/v1", "/responses").url(),
            "https://api.example.com/v1/responses"
        );
        assert_eq!(
            Endpoint::new("https://api.example.com/v1/", "responses").url(),
            "https://api.example.com/v1/responses"
        );
    }

    #[test]
    fn appends_query() {
        let url = Endpoint::new("https://h", "/p")
            .with_query("a", "1")
            .with_query("b", "2")
            .url();
        assert_eq!(url, "https://h/p?a=1&b=2");
    }
}
