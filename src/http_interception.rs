// Copyright 2020-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

//! Helpers for [`crate::WebViewBuilder::with_http_interceptor`].
//!
//! Interceptors are keyed by a normalized origin: `<scheme>://<host>[:<port>]`, with a
//! lowercase scheme and host, and without the port when it is the default for the scheme.
//! The wildcard origins `http://*` and `https://*` match every origin of their scheme.
//!
//! Precedence, from highest: an exact origin, a custom protocol (including its `http(s)`
//! workaround URI on Windows and Android), a wildcard origin.

use std::collections::HashMap;

use http::Uri;

const WILDCARD_HOST: &str = "*";

/// Normalizes an origin given to [`crate::WebViewBuilder::with_http_interceptor`].
///
/// Accepts `http` and `https` origins with an optional trailing `/`, including the wildcards
/// `http://*` and `https://*`.
/// Returns `None` for other schemes, user info, paths, queries or fragments.
pub(crate) fn normalize_origin(origin: &str) -> Option<String> {
  if let Some(wildcard) = normalize_wildcard(origin) {
    return Some(wildcard);
  }
  let uri: Uri = origin.parse().ok()?;
  let path_is_empty = matches!(uri.path(), "" | "/");
  if !path_is_empty || uri.query().is_some() || origin.contains('#') {
    return None;
  }
  origin_of(&uri)
}

fn normalize_wildcard(origin: &str) -> Option<String> {
  let (scheme, host) = origin.split_once("://")?;
  let scheme = scheme.to_ascii_lowercase();
  let is_wildcard = matches!(host, "*" | "*/");
  (is_wildcard && matches!(scheme.as_str(), "http" | "https"))
    .then(|| format!("{scheme}://{WILDCARD_HOST}"))
}

/// The interceptor registered for exactly the origin of `uri`.
#[cfg_attr(
  not(any(
    target_os = "android",
    target_os = "windows",
    target_os = "macos",
    target_os = "ios"
  )),
  allow(dead_code)
)]
pub(crate) fn find_exact<'a, H>(interceptors: &'a HashMap<String, H>, uri: &str) -> Option<&'a H> {
  interceptors.get(&request_origin(uri)?)
}

/// The wildcard interceptor (`http://*` or `https://*`) matching the scheme of `uri`.
#[cfg_attr(
  not(any(
    target_os = "android",
    target_os = "windows",
    target_os = "macos",
    target_os = "ios"
  )),
  allow(dead_code)
)]
pub(crate) fn find_wildcard<'a, H>(
  interceptors: &'a HashMap<String, H>,
  uri: &str,
) -> Option<&'a H> {
  let origin = request_origin(uri)?;
  let (scheme, _) = origin.split_once("://")?;
  interceptors.get(&format!("{scheme}://{WILDCARD_HOST}"))
}

/// Returns the normalized origin of a request URI, if it is `http` or `https`.
#[cfg_attr(
  not(any(
    target_os = "android",
    target_os = "windows",
    target_os = "macos",
    target_os = "ios"
  )),
  allow(dead_code)
)]
pub(crate) fn request_origin(uri: &str) -> Option<String> {
  origin_of(&uri.parse().ok()?)
}

fn origin_of(uri: &Uri) -> Option<String> {
  let scheme = uri.scheme_str()?.to_ascii_lowercase();
  let default_port = match scheme.as_str() {
    "http" => 80,
    "https" => 443,
    _ => return None,
  };

  let authority = uri.authority()?;
  if authority.as_str().contains('@') {
    return None;
  }

  let host = authority.host().to_ascii_lowercase();
  // `*` is only valid as the whole host (wildcard origin), handled by `normalize_wildcard`.
  if host.is_empty() || host.contains('*') {
    return None;
  }

  match authority.port_u16() {
    Some(port) if port != default_port => Some(format!("{scheme}://{host}:{port}")),
    _ => Some(format!("{scheme}://{host}")),
  }
}

/// Query parameter the app's Android body script appends to an intercepted request that has a
/// body, naming the body it handed to `HttpBodies` (see `WebViewBuilder::with_http_interceptor`).
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) const BODY_ID_PARAM: &str = "__wry_body";

/// Splits the [`BODY_ID_PARAM`] off `uri`: returns the URI without it, and the body id if any.
///
/// Other query parameters are kept byte for byte, in order.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) fn take_body_id(uri: &str) -> (String, Option<String>) {
  let (without_fragment, fragment) = match uri.find('#') {
    Some(i) => uri.split_at(i),
    None => (uri, ""),
  };
  let Some((base, query)) = without_fragment.split_once('?') else {
    return (uri.to_string(), None);
  };

  let mut body_id = None;
  let kept: Vec<&str> = query
    .split('&')
    .filter(|pair| match pair.split_once('=') {
      Some((name, value)) if name == BODY_ID_PARAM => {
        body_id = Some(value.to_string());
        false
      }
      _ => true,
    })
    .collect();

  if body_id.is_none() {
    return (uri.to_string(), None);
  }
  let uri = if kept.is_empty() {
    format!("{base}{fragment}")
  } else {
    format!("{base}?{}{fragment}", kept.join("&"))
  };
  (uri, body_id)
}

/// Whether [`crate::WebViewBuilder::with_http_interceptor`] works on the current platform.
pub(crate) const SUPPORTED: bool = cfg!(any(
  target_os = "android",
  target_os = "windows",
  target_os = "macos",
  target_os = "ios"
));

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn normalizes_origins() {
    assert_eq!(
      normalize_origin("https://app.com").as_deref(),
      Some("https://app.com")
    );
    assert_eq!(
      normalize_origin("https://app.com/").as_deref(),
      Some("https://app.com")
    );
    assert_eq!(
      normalize_origin("HTTPS://App.COM").as_deref(),
      Some("https://app.com")
    );
    assert_eq!(
      normalize_origin("https://app.com:443").as_deref(),
      Some("https://app.com")
    );
    assert_eq!(
      normalize_origin("http://app.com:80").as_deref(),
      Some("http://app.com")
    );
    assert_eq!(
      normalize_origin("https://app.com:8443").as_deref(),
      Some("https://app.com:8443")
    );
    assert_eq!(
      normalize_origin("http://[::1]:8080").as_deref(),
      Some("http://[::1]:8080")
    );
  }

  #[test]
  fn normalizes_wildcards() {
    assert_eq!(normalize_origin("https://*").as_deref(), Some("https://*"));
    assert_eq!(normalize_origin("HTTP://*/").as_deref(), Some("http://*"));
    assert_eq!(normalize_origin("wry://*"), None);
    assert_eq!(normalize_origin("https://*.app.com"), None);
  }

  #[test]
  fn finds_interceptors() {
    let map: HashMap<String, u8> = [
      ("https://app.com".to_string(), 1),
      ("https://*".to_string(), 2),
    ]
    .into();
    assert_eq!(find_exact(&map, "https://app.com/a"), Some(&1));
    assert_eq!(find_exact(&map, "https://cdn.app.com/a"), None);
    assert_eq!(find_wildcard(&map, "https://cdn.app.com/a"), Some(&2));
    assert_eq!(find_wildcard(&map, "http://cdn.app.com/a"), None);
    assert_eq!(find_wildcard(&map, "data:text/html,hi"), None);
  }

  #[test]
  fn takes_body_ids() {
    assert_eq!(
      take_body_id("https://app.com/a?__wry_body=k1"),
      ("https://app.com/a".to_string(), Some("k1".to_string()))
    );
    assert_eq!(
      take_body_id("https://app.com/a?x=a%20b&y=1&__wry_body=k1"),
      (
        "https://app.com/a?x=a%20b&y=1".to_string(),
        Some("k1".to_string())
      )
    );
    assert_eq!(
      take_body_id("https://app.com/a?x=1&__wry_body=k1#frag"),
      (
        "https://app.com/a?x=1#frag".to_string(),
        Some("k1".to_string())
      )
    );
    assert_eq!(
      take_body_id("https://app.com/a?x=1"),
      ("https://app.com/a?x=1".to_string(), None)
    );
    assert_eq!(
      take_body_id("https://app.com/a?__wry_bodyx=1"),
      ("https://app.com/a?__wry_bodyx=1".to_string(), None)
    );
  }

  #[test]
  fn rejects_invalid_origins() {
    assert_eq!(normalize_origin("app.com"), None);
    assert_eq!(normalize_origin("wry://localhost"), None);
    assert_eq!(normalize_origin("https://app.com/index.html"), None);
    assert_eq!(normalize_origin("https://app.com/?a=b"), None);
    assert_eq!(normalize_origin("https://app.com/#a"), None);
    assert_eq!(normalize_origin("https://user@app.com"), None);
    assert_eq!(normalize_origin(""), None);
  }

  #[test]
  fn matches_request_origins() {
    assert_eq!(
      request_origin("https://app.com/a/b?c=d").as_deref(),
      Some("https://app.com")
    );
    assert_eq!(
      request_origin("https://APP.com:443/").as_deref(),
      Some("https://app.com")
    );
    assert_eq!(
      request_origin("http://app.com/").as_deref(),
      Some("http://app.com")
    );
    assert_eq!(
      request_origin("https://app.com.evil.net/").as_deref(),
      Some("https://app.com.evil.net")
    );
    assert_eq!(request_origin("data:text/html,hi"), None);
    assert_eq!(request_origin("about:blank"), None);
  }
}
