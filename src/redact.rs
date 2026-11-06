pub fn key(_key: &str) -> String {
    "***".to_string()
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
    let redacted_base = redact_userinfo(url.split_once('?').map_or(url, |(base, _)| base));
    let Some((_, query)) = url.split_once('?') else {
        return redacted_base;
    };

    format!("{redacted_base}?{}", self::query(query))
}

/// Hide `user:password@` credentials embedded in a URL authority.
///
/// Uses the same URL parser as the HTTP client so partially normalized forms
/// such as `https:///user:pass@host/` are handled like a real request would.
fn redact_userinfo(url: &str) -> String {
    if let Ok(mut parsed) = url::Url::parse(url) {
        let has_username = !parsed.username().is_empty();
        let has_password = parsed.password().is_some();
        if has_username || has_password {
            if has_username {
                let _ = parsed.set_username("***");
            }
            if has_password {
                let _ = parsed.set_password(None);
            }
            return parsed.to_string();
        }
    }
    // Fallback heuristic for URL forms the parser rejects or treats as opaque.
    let (scheme, rest) = unparsed_authority(url);
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let Some((_, host)) = rest[..authority_end].rsplit_once('@') else {
        return url.to_string();
    };
    let prefix = scheme
        .map(|scheme| format!("{scheme}://"))
        .unwrap_or_default();
    format!("{prefix}***@{host}{}", &rest[authority_end..])
}

/// Credential values embedded in a configured URL: userinfo (raw and
/// percent-decoded) plus values of sensitive query parameters.
pub fn url_secrets(url: &str) -> Vec<String> {
    let mut secrets = Vec::new();
    if let Ok(parsed) = url::Url::parse(url) {
        if !parsed.username().is_empty() {
            secrets.push(parsed.username().to_string());
            secrets.push(percent_decode(parsed.username()));
        }
        if let Some(password) = parsed.password() {
            secrets.push(password.to_string());
            secrets.push(percent_decode(password));
        }
        for pair in parsed.query().unwrap_or_default().split('&') {
            let (raw_name, raw_value) = pair.split_once('=').unwrap_or((pair, ""));
            if is_sensitive_key(&decode_form_component(raw_name)) {
                // Keep both the wire form and the decoded form: upstream
                // errors may echo either one.
                secrets.push(raw_value.to_string());
                secrets.push(decode_form_component(raw_value));
            }
        }
    }
    if !secrets.is_empty() {
        return secrets;
    }

    // Fallback heuristic for URL forms the parser rejects or treats as opaque.
    let (_, rest) = unparsed_authority(url);
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let Some((userinfo, _)) = rest[..authority_end].rsplit_once('@') else {
        return secrets;
    };
    let (username, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
    [username, password]
        .into_iter()
        .filter(|value| !value.is_empty())
        .flat_map(|value| [value.to_string(), percent_decode(value)])
        .collect()
}

/// Decoded userinfo that the HTTP client turns into an `Authorization: Basic`
/// header, matching `reqwest`'s URL authority extraction.
pub fn url_basic_auth(url: &str) -> Option<(String, Option<String>)> {
    let parsed = url::Url::parse(url).ok()?;
    if !is_http_scheme(parsed.scheme()) || !parsed.has_authority() {
        return None;
    }
    // `reqwest` decodes strictly: an invalid username drops authentication
    // entirely, while an invalid password is treated as absent.
    let username = percent_decode_strict(parsed.username())?;
    let password = parsed.password().and_then(percent_decode_strict);
    if username.is_empty() && password.is_none() {
        return None;
    }
    Some((username, password))
}

fn is_http_scheme(scheme: &str) -> bool {
    matches!(
        scheme.to_ascii_lowercase().as_str(),
        "http" | "https" | "ws" | "wss"
    )
}

/// Locate the authority candidate in input the URL parser rejected.
///
/// Handles HTTP-like scheme prefixes with any number of slashes (including
/// `https:/host` and invalid ports) and network-path references (`//host`).
fn unparsed_authority(url: &str) -> (Option<&str>, &str) {
    // HTTP-like schemes tolerate nonstandard slash counts (`https:/host`);
    // this mirrors how the URL parser would handle the request target.
    if let Some((scheme, after)) = url.split_once(':') {
        if is_http_scheme(scheme) {
            return (Some(scheme), after.trim_start_matches(['/', '\\']));
        }
    }
    // Other `scheme://authority` forms are not requested by the proxy, but a
    // configured URL must still not display embedded credentials.
    if let Some((scheme, after)) = url.split_once("://") {
        if is_valid_scheme(scheme) {
            return (Some(scheme), after.trim_start_matches(['/', '\\']));
        }
    }
    if url.starts_with(['/', '\\']) {
        return (None, url.trim_start_matches(['/', '\\']));
    }
    (None, url)
}

/// RFC 3986 scheme syntax, used only to recognize `scheme://authority` input.
fn is_valid_scheme(scheme: &str) -> bool {
    let mut bytes = scheme.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    first.is_ascii_alphabetic()
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
}

fn decode_form_component(input: &str) -> String {
    percent_decode(&input.replace('+', " "))
}

fn percent_decode(input: &str) -> String {
    String::from_utf8_lossy(&percent_decode_bytes(input)).into_owned()
}

fn percent_decode_strict(input: &str) -> Option<String> {
    String::from_utf8(percent_decode_bytes(input)).ok()
}

fn percent_decode_bytes(input: &str) -> Vec<u8> {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                output.push(high << 4 | low);
                index += 3;
                continue;
            }
        }
        output.push(bytes[index]);
        index += 1;
    }
    output
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Describe a transport error without its URL.
///
/// `reqwest::Error` embeds the full request URL, including query parameters such
/// as `api_key`; that value must never reach logs or client error bodies.
pub fn transport_error(err: reqwest::Error) -> String {
    let kind = if err.is_timeout() {
        "timeout"
    } else if err.is_connect() {
        "connection failed"
    } else if err.is_body() || err.is_decode() {
        "response body error"
    } else if err.is_request() {
        "request failed"
    } else {
        "transport error"
    };
    let detail = err.without_url().to_string();
    format!("{kind}: {detail}")
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
        "key"
            | "api_key"
            | "apikey"
            | "api-key"
            | "x-api-key"
            | "access_token"
            | "refresh_token"
            | "token"
            | "password"
            | "secret"
            | "authorization"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_sensitive_query_values() {
        assert_eq!(
            query("alt=sse&key=sk-123456&api_key=token-abcdef"),
            "alt=sse&key=***&api_key=***"
        );
        assert_eq!(
            query("api-key=sk-short&x-api-key=short&mode=test"),
            "api-key=***&x-api-key=***&mode=test"
        );
    }

    #[test]
    fn redacts_url_userinfo_and_query_secrets() {
        assert_eq!(
            url("https://user:password@example.test/v1?key=sk-secret"),
            "https://***@example.test/v1?key=***"
        );
        assert_eq!(
            url("https://api.example.test/v1"),
            "https://api.example.test/v1"
        );
        assert_eq!(
            url("http://user@127.0.0.1:9000/v1"),
            "http://***@127.0.0.1:9000/v1"
        );
        // Partially normalized forms accepted by the client URL parser.
        assert_eq!(
            url("https:///alice:s3cr3t@example.test/v1"),
            "https://***@example.test/v1"
        );
        assert_eq!(
            url("https:/alice:s3cr3t@example.test/v1"),
            "https://***@example.test/v1"
        );
        // Percent-encoded userinfo.
        assert_eq!(
            url("https://alice:p%40ss%23word@example.test/v1"),
            "https://***@example.test/v1"
        );
        // Parsing errors fall back to hiding the authority credentials.
        assert_eq!(
            url("https://alice:s3cr3t@example.test:bad/v1"),
            "https://***@example.test:bad/v1"
        );
        assert_eq!(url("alice:s3cr3t@example.test/v1"), "***@example.test/v1");
        // Network-path references and single-slash forms.
        assert_eq!(url("//alice:s3cr3t@example.test/v1"), "***@example.test/v1");
        assert_eq!(
            url("https:/alice:s3cr3t@example.test:bad/v1"),
            "https://***@example.test:bad/v1"
        );
        // Non-HTTP schemes with an authority still hide userinfo on display.
        assert_eq!(
            url("ftp://alice:s3cr3t@example.test/v1"),
            "ftp://***@example.test/v1"
        );
        // Invalid ports make the URL parser fail; the fallback still hides it.
        assert_eq!(
            url("ftp://alice:s3cr3t@example.test:bad/v1"),
            "ftp://***@example.test:bad/v1"
        );
    }

    #[test]
    fn extracts_basic_auth_userinfo() {
        assert_eq!(
            url_basic_auth("http://alice:p%40ss@example.test/v1"),
            Some(("alice".to_string(), Some("p@ss".to_string())))
        );
        assert_eq!(
            url_basic_auth("http://alice:%FF@example.test/v1"),
            Some(("alice".to_string(), None))
        );
        assert_eq!(url_basic_auth("http://%FF:pass@example.test/v1"), None);
        assert_eq!(
            url_basic_auth("http://%E7%94%A8%E6%88%B7:pass@example.test/v1"),
            Some(("用户".to_string(), Some("pass".to_string())))
        );
        assert_eq!(url_basic_auth("https://example.test/v1"), None);
    }

    #[test]
    fn collects_url_credentials_for_display_redaction() {
        let secrets = url_secrets("https://alice:p%40ss%23word@example.test/v1?api_key=sk%2Dquery");
        for expected in [
            "alice",
            "p@ss#word",
            "p%40ss%23word",
            "sk%2Dquery",
            "sk-query",
        ] {
            assert!(
                secrets.contains(&expected.to_string()),
                "{expected}: {secrets:?}"
            );
        }
        assert_eq!(url_secrets("https://example.test/v1"), Vec::<String>::new());

        for (input, user, password) in [
            ("//alice:s3cr3t@example.test/v1", "alice", "s3cr3t"),
            ("https:/alice:s3cr3t@example.test:bad/v1", "alice", "s3cr3t"),
            ("ftp://alice:s3cr3t@example.test/v1", "alice", "s3cr3t"),
            ("ftp://alice:s3cr3t@example.test:bad/v1", "alice", "s3cr3t"),
        ] {
            let secrets = url_secrets(input);
            assert!(secrets.contains(&user.to_string()), "{input}: {secrets:?}");
            assert!(
                secrets.contains(&password.to_string()),
                "{input}: {secrets:?}"
            );
        }
    }

    #[tokio::test]
    async fn transport_error_omits_url_and_query_secrets() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);

        let error = reqwest::get(format!(
            "http://{address}/v1/models?api_key=sk-secret-value"
        ))
        .await
        .unwrap_err();
        let message = transport_error(error);

        assert!(!message.contains("sk-secret-value"), "{message}");
        assert!(!message.contains("for url"), "{message}");
        assert!(!message.contains("?api_key"), "{message}");
    }

    #[test]
    fn request_keeps_path_and_redacts_query() {
        let uri = "/v1/chat/completions?key=sk-query&mode=test"
            .parse()
            .unwrap();

        assert_eq!(
            request("POST", &uri),
            "POST /v1/chat/completions?key=***&mode=test"
        );
        let uri = "/pool/models?api-key=sk-query-secret".parse().unwrap();
        assert_eq!(request("GET", &uri), "GET /pool/models?api-key=***");
    }
}
