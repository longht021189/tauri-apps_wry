// Copyright 2020-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

//! [`crate::WebViewBuilder::with_http_interceptor`] on WKWebView.
//!
//! WKWebView refuses `setURLSchemeHandler:forURLScheme:` for schemes it handles natively
//! (`http`, `https`, …), checking `+[WKWebView handlesURLScheme:]`. We swizzle that class method
//! to return `NO` for `http` and `https`, then register one URL scheme handler per scheme.
//!
//! Once a scheme handler owns `https`, WebKit routes **every** `https` request of the webview to
//! it, not only those of intercepted origins. Requests matched by neither an exact nor a wildcard
//! (`https://*`) interceptor are therefore fetched with `NSURLSession` ("passthrough").

use std::{
  cell::Cell,
  collections::HashMap,
  sync::{Arc, OnceLock},
};

use block2::RcBlock;
use http::{HeaderName, Request, Response, StatusCode, header};
use objc2::{
  rc::Retained,
  runtime::{AnyClass, Bool, Imp, Sel},
  sel,
};
use objc2_foundation::{
  NSData, NSDictionary, NSError, NSHTTPURLResponse, NSMutableURLRequest, NSString, NSURL,
  NSURLResponse, NSURLSession, NSURLSessionConfiguration,
};

use crate::{
  RequestAsyncResponder, WebViewId,
  http_interception::{find_exact, find_wildcard},
};

type Handler = Box<dyn Fn(WebViewId, Request<Vec<u8>>, RequestAsyncResponder) + Send + Sync>;

/// Schemes taken over from WebKit. Must stay lowercase.
const SCHEMES: [&str; 2] = ["http", "https"];

/// Response headers that no longer describe the body once `NSURLSession` has decoded it.
const STALE_RESPONSE_HEADERS: [HeaderName; 3] = [
  header::CONTENT_ENCODING,
  header::CONTENT_LENGTH,
  header::TRANSFER_ENCODING,
];

/// Turns the webview's interceptors (keyed by normalized origin) into one handler per scheme,
/// ready to be registered as custom protocols.
///
/// Swizzles `+[WKWebView handlesURLScheme:]` the first time it is called with interceptors.
pub(crate) fn scheme_handlers(
  interceptors: HashMap<String, Handler>,
  incognito: bool,
) -> HashMap<String, Handler> {
  if interceptors.is_empty() {
    return HashMap::new();
  }
  allow_http_scheme_handlers();

  let mut by_scheme: HashMap<&'static str, HashMap<String, Handler>> = HashMap::new();
  for (origin, handler) in interceptors {
    // Origins are normalized, so they start with exactly `http://` or `https://`.
    let scheme = if origin.starts_with("https://") {
      "https"
    } else {
      "http"
    };
    by_scheme.entry(scheme).or_default().insert(origin, handler);
  }

  let session = Arc::new(Session::new(incognito));
  by_scheme
    .into_iter()
    .map(|(scheme, interceptors)| {
      let session = session.clone();
      let dispatch: Handler = Box::new(move |webview_id, request, responder| {
        let uri = request.uri().to_string();
        let interceptor =
          find_exact(&interceptors, &uri).or_else(|| find_wildcard(&interceptors, &uri));
        match interceptor {
          Some(handler) => handler(webview_id, request, responder),
          None => session.fetch(request, responder),
        }
      });
      (scheme.to_string(), dispatch)
    })
    .collect()
}

// --- Swizzling ---------------------------------------------------------------------------------

type HandlesUrlScheme = unsafe extern "C-unwind" fn(&AnyClass, Sel, *const NSString) -> Bool;

static ORIGINAL_HANDLES_URL_SCHEME: OnceLock<HandlesUrlScheme> = OnceLock::new();

unsafe extern "C-unwind" fn handles_url_scheme(
  cls: &AnyClass,
  sel: Sel,
  scheme: *const NSString,
) -> Bool {
  if let Some(scheme) = unsafe { scheme.as_ref() } {
    let scheme = scheme.to_string().to_ascii_lowercase();
    if SCHEMES.contains(&scheme.as_str()) {
      return Bool::NO;
    }
  }
  let original = ORIGINAL_HANDLES_URL_SCHEME
    .get()
    .expect("swizzled without original implementation");
  unsafe { original(cls, sel, scheme) }
}

/// Makes `+[WKWebView handlesURLScheme:]` return `NO` for `http` and `https`, process-wide.
fn allow_http_scheme_handlers() {
  ORIGINAL_HANDLES_URL_SCHEME.get_or_init(|| {
    let cls = AnyClass::get(c"WKWebView").expect("WKWebView class not found");
    let method = cls
      .class_method(sel!(handlesURLScheme:))
      .expect("+[WKWebView handlesURLScheme:] not found");
    unsafe {
      let replacement: HandlesUrlScheme = handles_url_scheme;
      let original =
        method.set_implementation(std::mem::transmute::<HandlesUrlScheme, Imp>(replacement));
      std::mem::transmute::<Imp, HandlesUrlScheme>(original)
    }
  });
}

// --- Passthrough -------------------------------------------------------------------------------

/// `NSURLSession` used for requests that no interceptor claims.
struct Session(Retained<NSURLSession>);

// SAFETY: `NSURLSession` is thread-safe.
unsafe impl Send for Session {}
unsafe impl Sync for Session {}

impl Session {
  fn new(incognito: bool) -> Self {
    let session = if incognito {
      NSURLSession::sessionWithConfiguration(
        &NSURLSessionConfiguration::ephemeralSessionConfiguration(),
      )
    } else {
      NSURLSession::sharedSession()
    };
    Self(session)
  }

  /// Fetches `request` from the network and resolves `responder` with the result.
  ///
  /// Redirects are followed by `NSURLSession`, so the webview receives the final response
  /// under the original URL.
  fn fetch(&self, request: Request<Vec<u8>>, responder: RequestAsyncResponder) {
    let Some(ns_request) = to_ns_request(&request) else {
      responder.respond(error_response(format!("invalid URL: {}", request.uri())));
      return;
    };

    let responder = Cell::new(Some(responder));
    let completion = RcBlock::new(
      move |data: *mut NSData, response: *mut NSURLResponse, error: *mut NSError| {
        let Some(responder) = responder.take() else {
          return;
        };
        // SAFETY: NSURLSession passes valid (or null) pointers for the duration of the call.
        let response =
          unsafe { to_http_response(data.as_ref(), response.as_ref(), error.as_ref()) };
        responder.respond(response);
      },
    );

    // SAFETY: the request and the completion block are valid; the block is copied by the session.
    let task = unsafe {
      self
        .0
        .dataTaskWithRequest_completionHandler(&ns_request, &completion)
    };
    task.resume();
  }
}

fn to_ns_request(request: &Request<Vec<u8>>) -> Option<Retained<NSMutableURLRequest>> {
  let url = NSURL::URLWithString(&NSString::from_str(&request.uri().to_string()))?;
  let ns_request = NSMutableURLRequest::requestWithURL(&url);
  ns_request.setHTTPMethod(&NSString::from_str(request.method().as_str()));
  for (name, value) in request.headers() {
    if let Ok(value) = value.to_str() {
      ns_request.addValue_forHTTPHeaderField(
        &NSString::from_str(value),
        &NSString::from_str(name.as_str()),
      );
    }
  }
  if !request.body().is_empty() {
    ns_request.setHTTPBody(Some(&NSData::with_bytes(request.body())));
  }
  Some(ns_request)
}

fn to_http_response(
  data: Option<&NSData>,
  response: Option<&NSURLResponse>,
  error: Option<&NSError>,
) -> Response<Vec<u8>> {
  if let Some(error) = error {
    return error_response(error.localizedDescription().to_string());
  }
  let Some(response) = response.and_then(|r| r.downcast_ref::<NSHTTPURLResponse>()) else {
    return error_response("no HTTP response".to_string());
  };

  let mut builder = Response::builder().status(
    u16::try_from(response.statusCode())
      .ok()
      .and_then(|code| StatusCode::from_u16(code).ok())
      .unwrap_or(StatusCode::BAD_GATEWAY),
  );

  // SAFETY: `allHeaderFields` maps header names to values, both `NSString`.
  let headers = unsafe {
    Retained::cast_unchecked::<NSDictionary<NSString, NSString>>(response.allHeaderFields())
  };
  for name in headers.allKeys() {
    let Some(value) = headers.objectForKey(&name) else {
      continue;
    };
    let name = name.to_string();
    if STALE_RESPONSE_HEADERS
      .iter()
      .any(|stale| stale.as_str().eq_ignore_ascii_case(&name))
    {
      continue;
    }
    builder = builder.header(name, value.to_string());
  }

  let body = data.map(NSData::to_vec).unwrap_or_default();
  builder
    .body(body)
    .unwrap_or_else(|e| error_response(e.to_string()))
}

fn error_response(message: String) -> Response<Vec<u8>> {
  Response::builder()
    .status(StatusCode::BAD_GATEWAY)
    .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
    .body(message.into_bytes())
    .unwrap()
}
