//! WebDAV XML response building. Mirrors og/cli XMLUtils output shape (DAV:
//! namespace prefix `D`). Hand-rolled string building — the responses are small
//! and regular, so a full XML library isn't warranted.

use chrono::{DateTime, Utc};

const XML_HEADER: &str = "<?xml version=\"1.0\" encoding=\"utf-8\" ?>";

/// Escape text for use inside an XML element body / attribute.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// `encodeURIComponent(uri)` but keep `/` unescaped — matches og
/// `XMLUtils.encodeWebDavUri`.
pub fn encode_href(uri: &str) -> String {
    let mut out = String::with_capacity(uri.len());
    for b in uri.bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            | b'/' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// RFC1123 GMT date, as og `formatDateForWebDav` (dayjs
/// `ddd, DD MMM YYYY HH:mm:ss [GMT]`). Falls back to epoch on parse failure.
pub fn webdav_date(iso: &str) -> String {
    let dt = DateTime::parse_from_rfc3339(iso)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| DateTime::<Utc>::from(std::time::UNIX_EPOCH));
    dt.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

/// A `<D:response>` node for a folder (collection).
pub fn folder_response(href: &str, display_name: &str, last_modified_iso: &str) -> String {
    format!(
        "<D:response><D:href>{href}</D:href><D:propstat><D:status>HTTP/1.1 200 OK</D:status>\
<D:prop><D:displayname>{name}</D:displayname>\
<D:getlastmodified>{modified}</D:getlastmodified>\
<D:getcontentlength>0</D:getcontentlength>\
<D:resourcetype><D:collection/></D:resourcetype></D:prop></D:propstat></D:response>",
        href = encode_href(href),
        name = escape(display_name),
        modified = webdav_date(last_modified_iso),
    )
}

/// A `<D:response>` node for a file.
pub fn file_response(
    href: &str,
    display_name: &str,
    content_type: &str,
    last_modified_iso: &str,
    size: u64,
    etag: &str,
) -> String {
    format!(
        "<D:response><D:href>{href}</D:href><D:propstat><D:status>HTTP/1.1 200 OK</D:status>\
<D:prop><D:resourcetype/>\
<D:getetag>\"{etag}\"</D:getetag>\
<D:displayname>{name}</D:displayname>\
<D:getcontenttype>{ctype}</D:getcontenttype>\
<D:getlastmodified>{modified}</D:getlastmodified>\
<D:getcontentlength>{size}</D:getcontentlength></D:prop></D:propstat></D:response>",
        href = encode_href(href),
        etag = escape(etag),
        name = escape(display_name),
        ctype = escape(content_type),
        modified = webdav_date(last_modified_iso),
    )
}

/// Wrap response nodes in a `<D:multistatus>` document.
pub fn multistatus(responses: &str) -> String {
    format!("{XML_HEADER}<D:multistatus xmlns:D=\"DAV:\">{responses}</D:multistatus>")
}

/// A `<D:error>` document carrying a human-readable description.
pub fn error(message: &str) -> String {
    format!(
        "{XML_HEADER}<D:error xmlns:D=\"DAV:\"><D:responsedescription>{}</D:responsedescription></D:error>",
        escape(message)
    )
}

/// A fake `<D:prop><D:lockdiscovery>` document for a granted lock. The server
/// doesn't actually enforce locks (mirrors og), it just hands out a token so
/// clients that require LOCK before PUT keep working.
pub fn lock_discovery(lock_token: &str, href: &str, depth: &str, timeout: &str) -> String {
    format!(
        "{XML_HEADER}<D:prop xmlns:D=\"DAV:\"><D:lockdiscovery><D:activelock>\
<D:locktype><D:write/></D:locktype>\
<D:lockscope><D:exclusive/></D:lockscope>\
<D:depth>{depth}</D:depth>\
<D:timeout>{timeout}</D:timeout>\
<D:locktoken><D:href>{token}</D:href></D:locktoken>\
<D:lockroot><D:href>{href}</D:href></D:lockroot>\
</D:activelock></D:lockdiscovery></D:prop>",
        depth = escape(depth),
        timeout = escape(timeout),
        token = escape(lock_token),
        href = encode_href(href),
    )
}
