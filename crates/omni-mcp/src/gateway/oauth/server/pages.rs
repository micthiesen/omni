//! The authorization pages: a waiting page that polls for the owner's
//! decision, and error pages. Self-contained HTML in Omni's dark look under
//! a strict, nonce-based CSP; nothing is loaded from elsewhere.

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;

use super::{COMPLETE_PATH, STATUS_PATH};

const STYLE: &str = "\
:root{color-scheme:dark;--canvas:#0a0b0d;--surface:#111316;--well:#07080a;--line:#22262c;\
--text:#eceef1;--text-2:#a9b0ba;--text-3:#858d98;--signal:#c6f36a}\
*{box-sizing:border-box}\
body{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;\
padding:24px 16px;background:var(--canvas);color:var(--text);\
font:15px/1.55 system-ui,-apple-system,\"Segoe UI\",Roboto,sans-serif}\
main{width:100%;max-width:440px;background:var(--surface);border:1px solid var(--line);\
border-radius:12px;padding:28px 24px}\
.brand{margin:0 0 18px;font-size:12px;font-weight:600;letter-spacing:.08em;\
text-transform:uppercase;color:var(--text-3)}\
h1{margin:0 0 12px;font-size:20px;line-height:1.3;font-weight:600}\
p{margin:0 0 12px;color:var(--text-2)}\
strong{color:var(--text);font-weight:600}\
.code{margin:20px 0;padding:14px 12px;border:1px solid var(--line);border-radius:8px;\
background:var(--well);text-align:center;color:var(--signal);\
font:600 28px/1.2 ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;letter-spacing:.14em}\
.status{margin:16px 0 0;font-size:14px;color:var(--text-3)}\
a{color:var(--signal)}";

const SCRIPT: &str = r#"(function () {
  var id = document.body.getAttribute("data-request");
  var query = "?request=" + encodeURIComponent(id);
  var line = document.getElementById("status");
  function poll() {
    fetch("STATUS_PATH" + query, { credentials: "same-origin", cache: "no-store" })
      .then(function (response) {
        if (response.status === 403 || response.status === 404) return { status: "gone" };
        return response.ok ? response.json() : null;
      })
      .then(function (body) {
        var status = body && body.status;
        if (status === "pending") { setTimeout(poll, 2000); return; }
        if (!status) {
          line.textContent = "Lost contact with Omni. Retrying…";
          setTimeout(poll, 5000);
          return;
        }
        line.textContent = status === "approved"
          ? "Approved. Returning to the app…"
          : "Returning to the app…";
        window.location.assign("COMPLETE_PATH" + query);
      })
      .catch(function () {
        line.textContent = "Lost contact with Omni. Retrying…";
        setTimeout(poll, 5000);
      });
  }
  setTimeout(poll, 2000);
})();"#;

/// What the waiting page shows.
pub(super) struct Waiting<'a> {
    pub request_id: &'a str,
    pub client_name: &'a str,
    pub redirect_host: &'a str,
    pub user_code: &'a str,
}

/// The page shown while the owner decides; `set_cookie` binds the browser.
pub(super) fn waiting(view: &Waiting<'_>, set_cookie: Option<&str>) -> Response {
    let id = escape(view.request_id);
    let body = format!(
        "<h1>Approve access to Omni</h1>\
         <p><strong>{client}</strong> wants to use Omni's MCP gateway and will return to \
         <strong>{host}</strong>.</p>\
         <p>To approve it, open the MCP gateway page in Omni on your home network and enter \
         this code there:</p>\
         <p class=\"code\" aria-label=\"Approval code {code}\">{code}</p>\
         <p>If you did not start this, deny the request instead. It expires in 10 \
         minutes.</p>\
         <p class=\"status\" id=\"status\" role=\"status\" aria-live=\"polite\">\
         Waiting for approval\u{2026}</p>\
         <noscript><p>After approving or denying, \
         <a href=\"{COMPLETE_PATH}?request={id}\">continue</a>.</p></noscript>",
        client = escape(view.client_name),
        host = escape(view.redirect_host),
        code = escape(view.user_code),
    );
    let script = SCRIPT
        .replace("STATUS_PATH", STATUS_PATH)
        .replace("COMPLETE_PATH", COMPLETE_PATH);
    let mut response = render(
        StatusCode::OK,
        "Approve access",
        &body,
        Some(view.request_id),
        Some(&script),
    );
    if let Some(cookie) = set_cookie.and_then(|c| HeaderValue::from_str(c).ok()) {
        response.headers_mut().insert(header::SET_COOKIE, cookie);
    }
    response
}

/// An error page (never a redirect).
pub(super) fn error(status: StatusCode, title: &str, message: &str) -> Response {
    let body = format!(
        "<h1>{}</h1><p>{}</p><p>Start again from the app that sent you here.</p>",
        escape(title),
        escape(message)
    );
    render(status, title, &body, None, None)
}

fn render(
    status: StatusCode,
    title: &str,
    body: &str,
    request_id: Option<&str>,
    script: Option<&str>,
) -> Response {
    let nonce = omni_core::ids::secure_id_b64url(18);
    let data = request_id
        .map(|id| format!(" data-request=\"{}\"", escape(id)))
        .unwrap_or_default();
    let script = script
        .map(|code| format!("<script nonce=\"{nonce}\">{code}</script>"))
        .unwrap_or_default();
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta name=\"color-scheme\" content=\"dark\">\
         <title>{title} \u{b7} Omni</title><style>{STYLE}</style></head>\
         <body{data}><main><p class=\"brand\">Omni</p>{body}</main>{script}</body></html>",
        title = escape(title),
    );
    let csp = format!(
        "default-src 'none'; style-src 'unsafe-inline'; script-src 'nonce-{nonce}'; \
         connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'"
    );
    let mut response = Response::new(Body::from(html));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    if let Ok(value) = HeaderValue::from_str(&csp) {
        headers.insert(header::CONTENT_SECURITY_POLICY, value);
    }
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    response
}

/// HTML text and attribute escaping.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_covers_markup_and_quotes() {
        assert_eq!(
            escape(r#"<a href="x">'&'</a>"#),
            "&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;&lt;/a&gt;"
        );
    }
}
