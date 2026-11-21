//! OCS response envelope (JSON + XML) as used by Nextcloud clients.

use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
    V1,
    V2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Json,
    Xml,
}

impl Version {
    /// OCS v1 reports statuscode 100 for success, v2 uses 200.
    pub fn ok_code(self) -> u16 {
        match self {
            Version::V1 => 100,
            Version::V2 => 200,
        }
    }
}

/// `?format=json` wins, then `Accept: application/json`; XML is the default.
pub fn negotiate(query: Option<&str>, headers: &HeaderMap) -> Format {
    if let Some(q) = query {
        for kv in q.split('&') {
            if let Some(("format", v)) = kv.split_once('=') {
                return if v.eq_ignore_ascii_case("json") {
                    Format::Json
                } else {
                    Format::Xml
                };
            }
        }
    }
    match headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()) {
        Some(a) if a.contains("application/json") => Format::Json,
        _ => Format::Xml,
    }
}

pub fn envelope(version: Version, data: Value) -> Value {
    json!({"ocs": {"meta": {"status": "ok", "statuscode": version.ok_code(), "message": "OK"}, "data": data}})
}

pub fn escape(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {}
            c => out.push(c),
        }
    }
}

fn write_xml(name: &str, v: &Value, out: &mut String) {
    out.push('<');
    out.push_str(name);
    out.push('>');
    match v {
        Value::Null => {}
        // Nextcloud renders booleans as 1 / empty.
        Value::Bool(b) => out.push_str(if *b { "1" } else { "" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => escape(s, out),
        Value::Array(a) => a.iter().for_each(|i| write_xml("element", i, out)),
        Value::Object(o) => o.iter().for_each(|(k, i)| write_xml(k, i, out)),
    }
    out.push_str("</");
    out.push_str(name);
    out.push('>');
}

pub fn to_xml(doc: &Value) -> String {
    let mut s = String::from("<?xml version=\"1.0\"?>\n");
    // serde_json maps are key-sorted; clients expect <meta> (status, statuscode, message)
    // before <data>.
    s.push_str("<ocs>");
    if let Some(o) = doc.get("ocs") {
        s.push_str("<meta>");
        for key in ["status", "statuscode", "message"] {
            if let Some(v) = o.get("meta").and_then(|m| m.get(key)) {
                write_xml(key, v, &mut s);
            }
        }
        s.push_str("</meta>");
        if let Some(d) = o.get("data") {
            write_xml("data", d, &mut s);
        }
    }
    s.push_str("</ocs>");
    s
}

const NO_CACHE: &str = "no-cache, no-store, must-revalidate";

pub fn respond(version: Version, format: Format, data: Value) -> Response {
    let doc = envelope(version, data);
    render(StatusCode::OK, format, &doc)
}

fn render(http: StatusCode, format: Format, doc: &Value) -> Response {
    // stock Nextcloud marks every OCS answer as uncacheable
    match format {
        Format::Json => (
            http,
            [
                (header::CONTENT_TYPE, "application/json; charset=utf-8"),
                (header::CACHE_CONTROL, NO_CACHE),
            ],
            doc.to_string(),
        )
            .into_response(),
        Format::Xml => (
            http,
            [
                (header::CONTENT_TYPE, "application/xml; charset=utf-8"),
                (header::CACHE_CONTROL, NO_CACHE),
            ],
            to_xml(doc),
        )
            .into_response(),
    }
}

/// Error envelope. v1 always answers HTTP 200 with the OCS code; v2 maps it to the HTTP status.
pub fn error(version: Version, format: Format, code: u16, message: &str) -> Response {
    error_data(version, format, code, message, json!([]))
}

/// Like [`error`] with an explicit `data` value (a few stock endpoints answer `null`).
pub fn error_data(
    version: Version,
    format: Format,
    code: u16,
    message: &str,
    data: Value,
) -> Response {
    let doc = json!({"ocs": {"meta": {"status": "failure", "statuscode": code, "message": message}, "data": data}});
    let http = match version {
        Version::V1 => StatusCode::OK,
        Version::V2 => StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
    };
    render(http, format, &doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_codes_per_version() {
        assert_eq!(
            envelope(Version::V1, json!({}))["ocs"]["meta"]["statuscode"],
            100
        );
        assert_eq!(
            envelope(Version::V2, json!({}))["ocs"]["meta"]["statuscode"],
            200
        );
    }

    #[test]
    fn xml_shape_and_escaping() {
        let x = to_xml(&envelope(
            Version::V2,
            json!({"a": true, "b": false, "c": "x<&>", "d": [1, 2], "n": null}),
        ));
        assert!(x.starts_with(
            "<?xml version=\"1.0\"?>\n<ocs><meta><status>ok</status><statuscode>200</statuscode>"
        ));
        assert!(x.contains(
            "<a>1</a><b></b><c>x&lt;&amp;&gt;</c><d><element>1</element><element>2</element></d>"
        ));
    }

    #[test]
    fn negotiation() {
        let mut h = HeaderMap::new();
        assert_eq!(negotiate(None, &h), Format::Xml);
        assert_eq!(negotiate(Some("format=json"), &h), Format::Json);
        assert_eq!(negotiate(Some("x=1&format=xml"), &h), Format::Xml);
        h.insert(header::ACCEPT, "application/json".parse().unwrap());
        assert_eq!(negotiate(None, &h), Format::Json);
        assert_eq!(negotiate(Some("format=xml"), &h), Format::Xml);
    }
}
