// Copyright 2020-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

@file:Suppress("unused")

package {{package}}

import android.os.SystemClock
import android.util.Base64
import android.webkit.*
import java.util.concurrent.ConcurrentHashMap

/**
 * Request bodies handed over by the page for HTTP interceptors.
 *
 * `shouldInterceptRequest` never exposes the request body, so the app's initialization script
 * stores it here (synchronously, before the request leaves) and tags the request URL with
 * `__wry_body=<id>`. [RustWebViewClient] takes it back by that id. See
 * `WebViewBuilder::with_http_interceptor` in wry for the contract.
 */
class HttpBodies(webView: RustWebView) {
    private class Entry(val body: ByteArray, val storedAt: Long)

    private val bodies = ConcurrentHashMap<String, Entry>()

    init {
        webView.httpBodies = this
        // Registers itself as `window.__wryHttpBodies`
        webView.addJavascriptInterface(this, "__wryHttpBodies")
    }

    @JavascriptInterface
    fun put(id: String?, base64: String?) {
        if (id == null || base64 == null) return
        val body = try { Base64.decode(base64, Base64.NO_WRAP) } catch (e: IllegalArgumentException) { return }
        val now = SystemClock.elapsedRealtime()
        // Bodies of requests that never reach `shouldInterceptRequest` (cancelled, …) expire.
        bodies.entries.removeIf { now - it.value.storedAt > EXPIRY_MS }
        bodies[id] = Entry(body, now)
    }

    /**
     * Removes and returns the body named by the request URL, if any.
     *
     * A CORS preflight (`OPTIONS`) goes to the same tagged URL before the actual request, so
     * methods without a body never take it.
     */
    fun take(request: WebResourceRequest): ByteArray? {
        if (request.method.uppercase() in METHODS_WITHOUT_BODY) return null
        val id = try { request.url.getQueryParameter(BODY_ID_PARAM) } catch (e: UnsupportedOperationException) { null }
        return id?.let { bodies.remove(it)?.body }
    }

    companion object {
        const val BODY_ID_PARAM = "__wry_body"
        private const val EXPIRY_MS = 60_000L
        private val METHODS_WITHOUT_BODY = setOf("GET", "HEAD", "OPTIONS")
    }
}
