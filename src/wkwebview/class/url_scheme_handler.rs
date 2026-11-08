// Copyright 2020-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::{
  borrow::Cow,
  cell::Cell,
  ffi::{CStr, c_char},
  panic::AssertUnwindSafe,
  ptr::NonNull,
  rc::Rc,
  sync::Arc,
};

use block2::RcBlock;
use http::{
  HeaderMap, Method, Request, Response as HttpResponse, StatusCode,
  header::{CONTENT_LENGTH, CONTENT_TYPE, LOCATION},
};
use objc2::{
  AllocAnyThread, ClassType, Message, msg_send,
  rc::Retained,
  runtime::{AnyClass, AnyObject, Bool, ClassBuilder, ProtocolObject, Sel},
  sel,
};
use objc2_foundation::{
  NSData, NSHTTPURLResponse, NSMutableDictionary, NSMutableURLRequest, NSObject, NSObjectProtocol,
  NSString, NSURL, NSURLRequest, NSUUID,
};
use objc2_web_kit::{WKURLSchemeHandler, WKURLSchemeTask};

use crate::{RequestAsyncResponder, WryWebView, wkwebview::WEBVIEW_STATE};

type ProtocolFn =
  Rc<dyn Fn(crate::WebViewId, Request<Vec<u8>>, RequestAsyncResponder) + Send + Sync>;

/// Redirects followed within one task. Past this, the `3xx` response is delivered as-is.
const MAX_REDIRECTS: u8 = 20;

pub fn create(name: &str) -> &AnyClass {
  unsafe {
    // Include the address of WEBVIEW_STATE in the class name so that each dylib in the process
    // gets its own ObjC class with method pointers into its own code and data segments.
    let unique_id = std::ptr::addr_of!(WEBVIEW_STATE) as usize;
    let scheme_name = format!("{name}URLSchemeHandler_{unique_id:x}\0");
    let scheme_name = CStr::from_bytes_with_nul(scheme_name.as_bytes()).unwrap();
    let cls = ClassBuilder::new(scheme_name, NSObject::class());
    match cls {
      Some(mut cls) => {
        cls.add_ivar::<*mut c_char>(c"webview_id");
        cls.add_ivar::<usize>(c"protocol_index");
        cls.add_method(
          objc2::sel!(webView:startURLSchemeTask:),
          start_task as extern "C" fn(_, _, _, _),
        );
        cls.add_method(
          objc2::sel!(webView:stopURLSchemeTask:),
          stop_task as extern "C" fn(_, _, _, _),
        );
        cls.register()
      }
      None => AnyClass::get(scheme_name).expect("Failed to get the class definition"),
    }
  }
}

// Task handler for custom protocol
extern "C" fn start_task(
  this: &AnyObject,
  _sel: objc2::runtime::Sel,
  webview: &WryWebView,
  task: &ProtocolObject<dyn WKURLSchemeTask>,
) {
  #[cfg(feature = "tracing")]
  let span =
    tracing::info_span!(parent: None, "wry::custom_protocol::handle", uri = tracing::field::Empty)
      .entered();

  let task_key = task.hash(); // hash by task object address
  let task_uuid = webview.add_custom_task_key(task_key);

  let ivar = this.class().instance_variable(c"webview_id").unwrap();
  let webview_id = unsafe {
    let webview_id_ptr: *mut c_char = *ivar.load(this);
    CStr::from_ptr(webview_id_ptr)
      .to_str()
      .ok()
      .unwrap_or_default()
      .to_string()
  };

  let ivar = this.class().instance_variable(c"protocol_index").unwrap();
  let protocol_index: usize = unsafe { *ivar.load(this) };

  let function = WEBVIEW_STATE
    .read()
    .unwrap()
    .get(&webview_id)
    .and_then(|v| v.protocol_ptrs.get(protocol_index))
    .cloned();

  let Some(function) = function else {
    #[cfg(feature = "tracing")]
    tracing::warn!(
      "Either WebView or WebContext instance is dropped! This handler shouldn't be called."
    );
    return;
  };

  let ns_request = unsafe { task.request() };
  let Some(url) = ns_request.URL() else {
    return;
  };

  #[cfg(feature = "tracing")]
  if let Some(uri) = url.absoluteString() {
    span.record("uri", uri.to_string());
  }

  // `Arc` rather than `Rc`: handlers may resolve (and drop) the responder from another thread,
  // so the reference count must be atomic. The task methods themselves are validated before use.
  #[allow(clippy::arc_with_non_send_sync)]
  let ctx = Arc::new(TaskContext {
    webview: webview.retain(),
    task: task.retain(),
    task_key,
    task_uuid,
    webview_id,
    function,
  });

  let body = read_body(&ns_request);
  match to_http_request(&ns_request, body) {
    Some(request) => dispatch(ctx, request, url, MAX_REDIRECTS),
    None => {
      let _ = ctx.send(&url, not_found());
    }
  }
}

/// A running `WKURLSchemeTask` and the handler serving it.
struct TaskContext {
  // FIXME: though we give it a static lifetime, it's not guaranteed to be valid.
  webview: Retained<WryWebView>,
  // FIXME: though we give it a static lifetime, it's not guaranteed to be valid.
  task: Retained<ProtocolObject<dyn WKURLSchemeTask>>,
  task_key: usize,
  task_uuid: Retained<NSUUID>,
  webview_id: String,
  function: ProtocolFn,
}

impl TaskContext {
  /// Task may not live longer than async custom protocol handler.
  ///
  /// There are roughly 2 ways to cause segfault:
  /// 1. Task has stopped. pointer of the task not valid anymore.
  /// 2. Task had stopped, but the pointer of the task has allocated to a new task.
  ///    Outdated custom handler may call to the new task instance and cause segfault.
  fn validate(&self) -> crate::Result<()> {
    if !WEBVIEW_STATE.read().unwrap().contains_key(&self.webview_id) {
      return Err(crate::Error::CustomProtocolTaskInvalid);
    }
    match self.webview.get_custom_task_uuid(self.task_key) {
      Some(latest_uuid) if latest_uuid == self.task_uuid => Ok(()),
      _ => Err(crate::Error::CustomProtocolTaskInvalid),
    }
  }

  /// Validates the task, then calls `f` (a `WKURLSchemeTask` method), catching ObjC exceptions.
  fn call(&self, f: impl FnOnce()) -> crate::Result<()> {
    self.validate()?;
    objc2::exception::catch(AssertUnwindSafe(f))
      .map_err(|_e| crate::Error::CustomProtocolTaskInvalid)
  }

  fn responds_to(&self, sel: Sel) -> bool {
    let responds: Bool = unsafe { msg_send![&*self.task, respondsToSelector: sel] };
    responds.as_bool()
  }

  /// Delivers `response` for `url` and finishes the task.
  fn send(&self, url: &NSURL, response: HttpResponse<Cow<'_, [u8]>>) -> crate::Result<()> {
    let ns_response = to_ns_response(url, &response);
    self.call(|| unsafe { self.task.didReceiveResponse(&ns_response) })?;

    let data = match response.into_body() {
      Cow::Owned(content) => NSData::from_vec(content),
      Cow::Borrowed(content) => NSData::with_bytes(content),
    };
    self.call(|| unsafe { self.task.didReceiveData(&data) })?;
    self.call(|| unsafe { self.task.didFinish() })?;

    if WEBVIEW_STATE.read().unwrap().contains_key(&self.webview_id) {
      self.webview.remove_custom_task_key(self.task_key);
      Ok(())
    } else {
      Err(crate::Error::CustomProtocolTaskInvalid)
    }
  }
}

/// Calls the handler with `request` and delivers its response to the task, following redirects.
fn dispatch(
  ctx: Arc<TaskContext>,
  request: Request<Vec<u8>>,
  url: Retained<NSURL>,
  redirects_left: u8,
) {
  let original = (
    request.method().clone(),
    request.headers().clone(),
    request.body().clone(),
  );
  let responder_ctx = ctx.clone();
  let responder: Box<dyn FnOnce(HttpResponse<Cow<'static, [u8]>>)> = Box::new(move |response| {
    if let Err(_e) = responder_ctx.validate() {
      #[cfg(feature = "tracing")]
      tracing::warn!("Task invalid before sending response: {:?}", _e);
      return;
    }

    let result = match redirect_target(&url, &response) {
      Some(new_url) if redirects_left > 0 => redirect(
        responder_ctx,
        &url,
        response,
        new_url,
        original,
        redirects_left - 1,
      ),
      _ => responder_ctx.send(&url, response),
    };

    if let Err(_e) = result {
      #[cfg(feature = "tracing")]
      tracing::error!("Error responding to task: {:?}", _e);
    }
  });

  #[cfg(feature = "tracing")]
  let _span = tracing::info_span!("wry::custom_protocol::call_handler").entered();

  (ctx.function)(
    &ctx.webview_id,
    request,
    RequestAsyncResponder { responder },
  );
}

/// The URL a `3xx` response redirects to, if WebKit can follow it within the task:
/// it has a valid `Location` with the same scheme as `url`.
fn redirect_target(url: &NSURL, response: &HttpResponse<Cow<'_, [u8]>>) -> Option<Retained<NSURL>> {
  if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
    return None;
  }
  let location = response.headers().get(LOCATION)?.to_str().ok()?;
  let new_url =
    NSURL::URLWithString_relativeToURL(&NSString::from_str(location), Some(url))?.absoluteURL()?;

  let scheme = |url: &NSURL| url.scheme().map(|s| s.to_string().to_ascii_lowercase());
  (scheme(&new_url)? == scheme(url)?).then_some(new_url)
}

/// Tells WebKit that the task was redirected to `new_url`, then serves the new request
/// with the same handler on the same task.
///
/// Uses `-[WKURLSchemeTask _willPerformRedirection:newRequest:completionHandler:]`, or the older
/// `_didPerformRedirection:newRequest:` (both private WebKit API). Without either, the `3xx`
/// response is delivered as-is.
fn redirect(
  ctx: Arc<TaskContext>,
  url: &NSURL,
  response: HttpResponse<Cow<'_, [u8]>>,
  new_url: Retained<NSURL>,
  (method, headers, body): (Method, HeaderMap, Vec<u8>),
  redirects_left: u8,
) -> crate::Result<()> {
  let (method, headers, body) = redirected_request(response.status(), method, headers, body);

  let ns_response = to_ns_response(url, &response);
  let ns_request = NSMutableURLRequest::requestWithURL(&new_url);
  ns_request.setHTTPMethod(&NSString::from_str(method.as_str()));
  for (name, value) in &headers {
    if let Ok(value) = value.to_str() {
      ns_request.addValue_forHTTPHeaderField(
        &NSString::from_str(value),
        &NSString::from_str(name.as_str()),
      );
    }
  }

  if ctx.responds_to(sel!(_willPerformRedirection:newRequest:completionHandler:)) {
    let block_ctx = ctx.clone();
    let body = Cell::new(Some(body));
    let completion = RcBlock::new(move |final_request: *mut NSURLRequest| {
      // SAFETY: WebKit passes a valid request, or null when the load was cancelled.
      let Some(final_request) = (unsafe { final_request.as_ref() }) else {
        return;
      };
      let Some(final_url) = final_request.URL() else {
        return;
      };
      let body = body.take().unwrap_or_default();
      match to_http_request(final_request, body) {
        Some(request) => dispatch(block_ctx.clone(), request, final_url, redirects_left),
        None => {
          let _ = block_ctx.send(&final_url, not_found());
        }
      }
    });
    ctx.call(|| unsafe {
      let _: () = msg_send![
        &*ctx.task,
        _willPerformRedirection: &*ns_response,
        newRequest: &*ns_request,
        completionHandler: &*completion
      ];
    })
  } else if ctx.responds_to(sel!(_didPerformRedirection:newRequest:)) {
    ctx.call(|| unsafe {
      let _: () = msg_send![
        &*ctx.task,
        _didPerformRedirection: &*ns_response,
        newRequest: &*ns_request
      ];
    })?;
    match to_http_request(&ns_request, body) {
      Some(request) => dispatch(ctx, request, new_url, redirects_left),
      None => ctx.send(&new_url, not_found())?,
    }
    Ok(())
  } else {
    ctx.send(url, response)
  }
}

/// Method, headers and body of the request that follows a redirect with `status`, as in the
/// Fetch standard: `303` (except for `HEAD`), and `301`/`302` for `POST`, switch to a bodiless `GET`.
fn redirected_request(
  status: StatusCode,
  method: Method,
  mut headers: HeaderMap,
  body: Vec<u8>,
) -> (Method, HeaderMap, Vec<u8>) {
  let to_get = (status == StatusCode::SEE_OTHER && method != Method::HEAD)
    || (matches!(status, StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND)
      && method == Method::POST);
  if !to_get {
    return (method, headers, body);
  }
  for name in [
    CONTENT_TYPE,
    CONTENT_LENGTH,
    http::header::CONTENT_ENCODING,
    http::header::CONTENT_LANGUAGE,
  ] {
    headers.remove(name);
  }
  (Method::GET, headers, Vec::new())
}

fn read_body(request: &NSURLRequest) -> Vec<u8> {
  if let Some(body) = request.HTTPBody() {
    return body.to_vec();
  }
  let mut sent_form_body = Vec::new();
  if let Some(body_stream) = request.HTTPBodyStream() {
    body_stream.open();

    let mut buf = [0u8; 128];
    while body_stream.hasBytesAvailable() {
      let count =
        unsafe { body_stream.read_maxLength(NonNull::new(buf.as_mut_ptr()).unwrap(), buf.len()) };
      sent_form_body.extend_from_slice(&buf[..count as usize]);
    }

    body_stream.close();
  }
  sent_form_body
}

fn to_http_request(request: &NSURLRequest, body: Vec<u8>) -> Option<Request<Vec<u8>>> {
  let uri = request.URL()?.absoluteString()?.to_string();
  // Get request method (GET, POST, PUT etc...)
  let method = request
    .HTTPMethod()
    .map(|m| m.to_string())
    .unwrap_or_else(|| "GET".to_string());

  let mut http_request = Request::builder().uri(uri).method(method.as_str());

  // get all our headers values and inject them in our request
  if let Some(all_headers) = request.allHTTPHeaderFields() {
    for current_header in all_headers.allKeys().iter() {
      let header_value = all_headers.valueForKey(&current_header).unwrap();
      http_request = http_request.header(current_header.to_string(), header_value.to_string());
    }
  }

  http_request.body(body).ok()
}

fn to_ns_response(
  url: &NSURL,
  response: &HttpResponse<Cow<'_, [u8]>>,
) -> Retained<NSHTTPURLResponse> {
  let content_len = response.body().len();
  // default: application/octet-stream, but should be provided by the client
  let wanted_mime = response.headers().get(CONTENT_TYPE);
  // default to 200
  let wanted_status_code = response.status().as_u16() as i32;
  // default to HTTP/1.1
  let wanted_version = format!("{:#?}", response.version());

  let headers = NSMutableDictionary::new();
  if let Some(mime) = wanted_mime.and_then(|mime| mime.to_str().ok()) {
    headers.insert(
      &*NSString::from_str(CONTENT_TYPE.as_str()),
      &*NSString::from_str(mime),
    );
  }
  headers.insert(
    &*NSString::from_str(CONTENT_LENGTH.as_str()),
    &*NSString::from_str(&content_len.to_string()),
  );

  // add headers
  for (name, value) in response.headers().iter() {
    if let Ok(value) = value.to_str() {
      headers.insert(
        &*NSString::from_str(name.as_str()),
        &*NSString::from_str(value),
      );
    }
  }

  NSHTTPURLResponse::initWithURL_statusCode_HTTPVersion_headerFields(
    NSHTTPURLResponse::alloc(),
    url,
    wanted_status_code.try_into().unwrap(),
    Some(&NSString::from_str(&wanted_version)),
    Some(&headers),
  )
  .unwrap()
}

fn not_found() -> HttpResponse<Cow<'static, [u8]>> {
  HttpResponse::builder()
    .status(StatusCode::NOT_FOUND)
    .body(Cow::Borrowed(&[][..]))
    .unwrap()
}

extern "C" fn stop_task(
  _this: &ProtocolObject<dyn WKURLSchemeHandler>,
  _sel: objc2::runtime::Sel,
  webview: &WryWebView,
  task: &ProtocolObject<dyn WKURLSchemeTask>,
) {
  webview.remove_custom_task_key(task.hash());
}
