---
"wry": minor
---

Add `WebViewBuilder::with_http_interceptor` to resolve every request to an `http(s)` origin (e.g. `https://app.com`) with a handler instead of the network, keeping the real origin. Use `is_http_interception_supported` to check support: Android, Windows, macOS and iOS are supported (on macOS and iOS by swizzling `+[WKWebView handlesURLScheme:]`, with other origins of an intercepted scheme fetched through `NSURLSession`). Linux is not and `build` returns `Error::HttpInterceptionUnsupported`.

`with_custom_protocol` and `with_asynchronous_custom_protocol` forward a `name` containing `://` (e.g. `"https://app.com"`) to `with_http_interceptor`, so wrappers that only expose custom protocols, such as Tauri's `register_asynchronous_uri_scheme_protocol`, can intercept `http(s)` origins.

`with_http_interceptor` accepts the wildcard origins `http://*` and `https://*` to intercept every origin of a scheme. Precedence: exact origin, then custom protocol (including its `http(s)` workaround URI on Windows and Android), then wildcard.

On macOS and iOS, a `3xx` response with a `Location` of the same scheme returned by a custom protocol or HTTP interceptor handler is now followed within the request (via WebKit's private `_willPerformRedirection:newRequest:completionHandler:`), updating the page or `fetch` URL and calling the handler again with the redirected request.

On Android, HTTP interceptors can now receive request bodies: with interceptors registered, wry exposes `window.__wryHttpBodies` (`HttpBodies`, a synchronous `@JavascriptInterface`). An app initialization script hands a body over with `put(id, base64)` and tags the request URL with `__wry_body=<id>`; wry passes the body to the handler and strips the parameter. `Rust.handleRequest` in the generated Kotlin files takes a new `body: ByteArray?` parameter.
