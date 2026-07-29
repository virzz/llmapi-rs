pub fn key(key: &str) -> String {
    let prefix: String = key.chars().take(6).collect();
    format!("{prefix}...")
}

pub fn query(query: &str) -> String {
    url::form_urlencoded::parse(query.as_bytes())
        .map(|(name, value)| {
            if is_sensitive_key(&name) {
                format!("{name}={}", key(&value))
            } else {
                format!("{name}={value}")
            }
        })
        .collect::<Vec<_>>()
        .join("&")
}

pub fn url(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else {
        return url.to_string();
    };

    format!("{base}?{}", self::query(query))
}

pub fn request(method: &str, uri: &axum::http::Uri) -> String {
    match uri.query() {
        Some(query) => format!("{method} {}?{}", uri.path(), self::query(query)),
        None => format!("{method} {}", uri.path()),
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    matches!(
        key.as_str(),
        "key" | "api_key" | "apikey" | "access_token" | "token"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_sensitive_query_values() {
        assert_eq!(
            query("alt=sse&key=sk-123456&api_key=token-abcdef"),
            "alt=sse&key=sk-123...&api_key=token-..."
        );
    }

    #[test]
    fn request_keeps_path_and_redacts_query() {
        let uri = "/v1/chat/completions?key=sk-query&mode=test"
            .parse()
            .unwrap();

        assert_eq!(
            request("POST", &uri),
            "POST /v1/chat/completions?key=sk-que...&mode=test"
        );
    }
}
