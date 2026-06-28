use std::collections::HashMap;

use anyhow::Context;
use reqwest::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedOAuthCallback {
    pub path: String,
    pub params: HashMap<String, String>,
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

impl ParsedOAuthCallback {
    pub fn error_message(&self) -> Option<String> {
        self.error_description
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                self.error
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
            })
            .map(str::to_string)
    }
}

pub fn target_from_input(input: &str) -> anyhow::Result<String> {
    let value = input.trim();
    if value.is_empty() {
        anyhow::bail!("回调 URL 不能为空");
    }
    if value.starts_with('/') {
        return Ok(value.to_string());
    }

    let url = Url::parse(value).context("无效的回调 URL")?;
    let mut target = url.path().to_string();
    if target.is_empty() {
        target.push('/');
    }
    if let Some(query) = url.query() {
        target.push('?');
        target.push_str(query);
    }
    Ok(target)
}

pub fn parse_input(input: &str) -> anyhow::Result<ParsedOAuthCallback> {
    let target = target_from_input(input)?;
    parse_target(&target)
}

pub fn parse_target(target: &str) -> anyhow::Result<ParsedOAuthCallback> {
    let (path, query) = split_path_query(target);
    let params = parse_query_string(query.unwrap_or(""));
    Ok(ParsedOAuthCallback {
        path: path.to_string(),
        code: params.get("code").cloned(),
        state: params.get("state").cloned(),
        error: params.get("error").cloned(),
        error_description: params.get("error_description").cloned(),
        params,
    })
}

pub fn split_path_query(value: &str) -> (&str, Option<&str>) {
    value
        .split_once('?')
        .map_or((value, None), |(path, query)| (path, Some(query)))
}

pub fn parse_query_string(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter_map(|pair| {
            let mut iter = pair.splitn(2, '=');
            let key = iter.next()?.to_string();
            let val = iter
                .next()
                .map(|v| {
                    let with_space = v.replace('+', " ");
                    urlencoding::decode(&with_space)
                        .map(|s| s.into_owned())
                        .unwrap_or_else(|_| with_space)
                })
                .unwrap_or_default();
            Some((key, val))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{parse_input, target_from_input};

    #[test]
    fn target_from_input_accepts_full_url_and_path() {
        assert_eq!(
            target_from_input("http://localhost:3128/oauth/callback?code=abc&state=xyz").unwrap(),
            "/oauth/callback?code=abc&state=xyz"
        );
        assert_eq!(
            target_from_input("/oauth/callback?code=abc").unwrap(),
            "/oauth/callback?code=abc"
        );
    }

    #[test]
    fn parse_input_decodes_common_oauth_params() {
        let callback =
            parse_input("/oauth/callback?code=abc%2Fdef&state=xyz&error_description=bad+thing")
                .unwrap();

        assert_eq!(callback.path, "/oauth/callback");
        assert_eq!(callback.code.as_deref(), Some("abc/def"));
        assert_eq!(callback.state.as_deref(), Some("xyz"));
        assert_eq!(callback.error_message().as_deref(), Some("bad thing"));
    }

    #[test]
    fn parse_input_rejects_empty_input() {
        assert!(target_from_input(" ").is_err());
    }
}
