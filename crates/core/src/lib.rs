//! Read-only virtual disks and WebDAV views with application-defined policies.
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use percent_encoding::{NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use std::fs::File;
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::sync::Arc;
use tiny_http::{Header, Request, Response, Server, StatusCode};
mod disk;
mod view;
pub use view::{Passthrough, Plugin, Source, View, validate_label};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Disk,
    Webdav,
}
fn request_parts(url: &str, token: &str) -> Result<Vec<String>, u16> {
    let decoded = percent_decode_str(url.split('?').next().unwrap_or(""))
        .decode_utf8()
        .map_err(|_| 400u16)?;
    let prefix = format!("/{token}/");
    let path = decoded
        .strip_prefix(&prefix)
        .ok_or(404u16)?
        .trim_end_matches('/');
    if path.is_empty() {
        return Ok(vec![]);
    }
    let parts: Vec<String> = path.split('/').map(str::to_owned).collect();
    if parts
        .iter()
        .any(|p| p.is_empty() || p == "." || p == ".." || p.contains('\0'))
    {
        return Err(403);
    }
    Ok(parts)
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn http_date(epoch: i64) -> Result<String> {
    Ok(DateTime::<Utc>::from_timestamp(epoch, 0)
        .context("invalid date")?
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string())
}

// Namespace URI and local name, rather than the client's arbitrary XML prefix.
type Property = (String, String);

fn requested_properties(body: &str) -> Result<Option<Vec<Property>>> {
    if body.is_empty() {
        return Ok(None);
    }
    let doc = roxmltree::Document::parse(body)?;
    let root = doc.root_element();
    ensure!(
        root.tag_name().namespace() == Some("DAV:") && root.tag_name().name() == "propfind",
        "expected DAV propfind"
    );
    Ok(root
        .children()
        .find(|n| n.has_tag_name(("DAV:", "prop")))
        .map(|node| {
            node.children()
                .filter(|n| n.is_element())
                .map(|n| {
                    (
                        n.tag_name().namespace().unwrap_or("").into(),
                        n.tag_name().name().into(),
                    )
                })
                .collect()
        }))
}

fn prop_response(
    view: &View,
    token: &str,
    parts: &[String],
    file: &File,
    requested: Option<&[Property]>,
) -> Result<String> {
    let meta = view.metadata(parts, file)?;
    let mut href = format!(
        "/{token}/{}",
        parts
            .iter()
            .map(|p| utf8_percent_encode(p, NON_ALPHANUMERIC).to_string())
            .collect::<Vec<_>>()
            .join("/")
    );
    if meta.directory && !href.ends_with('/') {
        href.push('/');
    }
    let known = [
        "displayname",
        "getlastmodified",
        "getcontentlength",
        "getcontenttype",
        "getetag",
        "resourcetype",
        "supportedlock",
        "lockdiscovery",
    ];
    let defaults: Vec<Property> = known.iter().map(|k| ("DAV:".into(), (*k).into())).collect();
    let props = requested.unwrap_or(&defaults);
    let mut good = String::new();
    let mut bad = String::new();
    for (namespace, name) in props {
        if namespace != "DAV:" || !known.contains(&name.as_str()) {
            if namespace.is_empty() {
                bad.push_str(&format!("<{name}/>"));
            } else {
                bad.push_str(&format!("<X:{name} xmlns:X=\"{}\"/>", escape(namespace)));
            }
            continue;
        }
        let value = match name.as_str() {
            "displayname" => escape(parts.last().map_or(view.label.as_str(), String::as_str)),
            "getlastmodified" => http_date(meta.mtime)?,
            "getcontentlength" => if meta.directory { 0 } else { meta.size }.to_string(),
            "getcontenttype" => if meta.directory {
                "httpd/unix-directory"
            } else {
                "application/octet-stream"
            }
            .into(),
            "getetag" => escape(&meta.etag),
            "resourcetype" if meta.directory => "<D:collection/>".into(),
            _ => String::new(),
        };
        good.push_str(&format!("<D:{name}>{value}</D:{name}>"));
    }
    let mut xml = format!("<D:response><D:href>{}</D:href>", escape(&href));
    for (fields, status) in [(good, "200 OK"), (bad, "404 Not Found")] {
        if !fields.is_empty() {
            xml.push_str(&format!("<D:propstat><D:prop>{fields}</D:prop><D:status>HTTP/1.1 {status}</D:status></D:propstat>"));
        }
    }
    xml.push_str("</D:response>");
    Ok(xml)
}

struct Reply {
    code: u16,
    headers: Vec<Header>,
    body: Box<dyn Read + Send>,
    length: usize,
}

impl Reply {
    fn bytes(code: u16, data: Vec<u8>) -> Self {
        let length = data.len();
        Self {
            code,
            headers: vec![],
            body: Box::new(Cursor::new(data)),
            length,
        }
    }
    fn empty(code: u16) -> Self {
        Self::bytes(code, vec![])
    }
    fn header(mut self, name: &str, value: impl AsRef<str>) -> Self {
        self.headers.push(
            Header::from_bytes(name, value.as_ref()).expect("valid internal response header"),
        );
        self
    }
}

fn header<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

fn byte_range(value: Option<&str>, size: u64) -> Result<(u64, u64), ()> {
    let Some(value) = value else {
        return Ok((0, size));
    };
    let (first, last) = value
        .strip_prefix("bytes=")
        .and_then(|s| s.split_once('-'))
        .ok_or(())?;
    if size == 0
        || (!first.is_empty() && !first.bytes().all(|b| b.is_ascii_digit()))
        || (!last.is_empty() && !last.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(());
    }
    let (start, end) = if first.is_empty() {
        let suffix = last.parse::<u64>().map_err(|_| ())?;
        if suffix == 0 {
            return Err(());
        }
        (size.saturating_sub(suffix), size - 1)
    } else {
        (
            first.parse::<u64>().map_err(|_| ())?,
            if last.is_empty() {
                size - 1
            } else {
                last.parse::<u64>().map_err(|_| ())?.min(size - 1)
            },
        )
    };
    if start > end || start >= size {
        return Err(());
    }
    Ok((start, end - start + 1))
}

fn handle(
    view: &Arc<View>,
    token: &str,
    request: &mut Request,
    image: Option<&Arc<disk::Image>>,
) -> Result<Reply> {
    let method = request.method().as_str();
    if !matches!(method, "OPTIONS" | "PROPFIND" | "GET" | "HEAD") {
        return Ok(Reply::empty(403));
    }
    let parts = match request_parts(request.url(), token) {
        Ok(p) => p,
        Err(code) => return Ok(Reply::empty(code)),
    };
    if request.body_length().is_some_and(|s| s > 65536) {
        return Ok(Reply::empty(413));
    }
    let mut body = String::new();
    request.as_reader().take(65537).read_to_string(&mut body)?;
    if body.len() > 65536 {
        return Ok(Reply::empty(413));
    }
    if let Some(image) = image {
        if parts == ["card.img"] && matches!(request.method().as_str(), "GET" | "HEAD") {
            return image.reply(view, request);
        }
        return Ok(Reply::empty(404));
    }
    match request.method().as_str() {
        "OPTIONS" => Ok(Reply::empty(200)
            .header("DAV", "1")
            .header("Allow", "OPTIONS, PROPFIND, GET, HEAD")
            .header("MS-Author-Via", "DAV")),
        "PROPFIND" => {
            let depth = header(request, "Depth").unwrap_or("infinity");
            if depth != "0" && depth != "1" {
                return Ok(Reply::empty(403));
            }
            let requested = requested_properties(&body)?;
            let file = view.open(&parts)?;
            let mut xml = String::from(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?><D:multistatus xmlns:D=\"DAV:\">",
            );
            xml.push_str(&prop_response(
                view,
                token,
                &parts,
                &file,
                requested.as_deref(),
            )?);
            if depth == "1" && file.metadata()?.is_dir() {
                for name in view.names(&parts, &file)? {
                    let mut child = parts.clone();
                    child.push(name);
                    match view.open(&child) {
                        Ok(f) => xml.push_str(&prop_response(
                            view,
                            token,
                            &child,
                            &f,
                            requested.as_deref(),
                        )?),
                        Err(e) if matches!(error_status(&e), 403 | 404) => (),
                        Err(e) => return Err(e),
                    }
                }
            }
            xml.push_str("</D:multistatus>");
            Ok(Reply::bytes(207, xml.into_bytes())
                .header("Content-Type", "application/xml; charset=utf-8"))
        }
        "GET" | "HEAD" => {
            let mut file = view.open(&parts)?;
            let meta = view.metadata(&parts, &file)?;
            if meta.directory {
                return Ok(Reply::empty(405));
            }
            let range = header(request, "Range");
            let (start, length) = match byte_range(range, meta.size) {
                Ok(pair) => pair,
                Err(()) => {
                    return Ok(
                        Reply::empty(416).header("Content-Range", format!("bytes */{}", meta.size))
                    );
                }
            };
            file.seek(SeekFrom::Start(start))?;
            let mut reply = Reply {
                code: if range.is_some() { 206 } else { 200 },
                headers: vec![],
                body: Box::new(file.take(length)),
                length: usize::try_from(length)?,
            }
            .header("Content-Type", "application/octet-stream")
            .header("Last-Modified", http_date(meta.mtime)?)
            .header("ETag", &meta.etag)
            .header("Accept-Ranges", "bytes");
            if range.is_some() {
                reply = reply.header(
                    "Content-Range",
                    format!("bytes {start}-{}/{}", start + length - 1, meta.size),
                );
            }
            Ok(reply)
        }
        _ => unreachable!(),
    }
}

pub fn error_status(error: &anyhow::Error) -> u16 {
    if error.downcast_ref::<io::Error>().is_some_and(|e| {
        matches!(
            e.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
        )
    }) {
        return 404;
    }
    let number = error
        .downcast_ref::<rustix::io::Errno>()
        .map(|e| e.raw_os_error())
        .or_else(|| {
            error
                .downcast_ref::<io::Error>()
                .and_then(io::Error::raw_os_error)
        });
    if let Some(n) = number {
        if n == rustix::io::Errno::NOENT.raw_os_error()
            || n == rustix::io::Errno::NOTDIR.raw_os_error()
        {
            return 404;
        }
        if n == rustix::io::Errno::LOOP.raw_os_error()
            || n == rustix::io::Errno::ACCESS.raw_os_error()
        {
            return 403;
        }
        return 503;
    }
    if error
        .downcast_ref::<io::Error>()
        .is_some_and(|e| e.kind() == io::ErrorKind::PermissionDenied)
    {
        return 403;
    }
    400
}

pub fn serve(view: View, mode: Mode, port: u16, mut info: serde_json::Value) -> Result<()> {
    let image = if mode == Mode::Disk {
        Some(Arc::new(disk::Image::new(&view)?))
    } else {
        None
    };
    let view = Arc::new(view);
    let mut random = [0u8; 24];
    getrandom::fill(&mut random).map_err(|e| anyhow::anyhow!("OS random source: {e}"))?;
    let token = Arc::new(
        random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
    );
    let server = Arc::new(Server::http(("127.0.0.1", port)).map_err(|e| anyhow::anyhow!("{e}"))?);
    let object = info
        .as_object_mut()
        .context("startup info must be an object")?;
    object.insert(
        "url".into(),
        serde_json::json!(format!(
            "http://{}/{token}/{}",
            server.server_addr(),
            if image.is_some() { "card.img" } else { "" }
        )),
    );
    object.insert(
        "mode".into(),
        serde_json::json!(if image.is_some() { "disk" } else { "webdav" }),
    );
    object.insert("label".into(), serde_json::json!(view.label));
    object.insert(
        "virtual_disk_bytes".into(),
        serde_json::json!(image.as_ref().map(|i| i.length)),
    );
    println!("{info}");
    // Bounded workers stream reads; image content is not buffered in memory.
    std::thread::scope(|scope| {
        for _ in 0..4 {
            let (server, view, token, image) =
                (server.clone(), view.clone(), token.clone(), image.clone());
            scope.spawn(move || {
                for mut request in server.incoming_requests() {
                    let reply = handle(&view, &token, &mut request, image.as_ref())
                        .unwrap_or_else(|e| Reply::empty(error_status(&e)));
                    let response = Response::new(
                        StatusCode(reply.code),
                        reply.headers,
                        reply.body,
                        Some(reply.length),
                        None,
                    )
                    .with_chunked_threshold(usize::MAX);
                    let _ = request.respond(response);
                }
            });
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paths_and_range_boundaries_are_rejected() {
        for path in ["/t/../etc", "/t/%2e%2e/etc", "/t/a//b", "/t/%00"] {
            assert_eq!(request_parts(path, "t"), Err(403));
        }
        assert_eq!(request_parts("/wrong/file", "t"), Err(404));
        assert_eq!(byte_range(Some("bytes=17-35"), 100), Ok((17, 19)));
        assert_eq!(byte_range(Some("bytes=-19"), 100), Ok((81, 19)));
        assert_eq!(byte_range(Some("bytes=95-"), 100), Ok((95, 5)));
        assert_eq!(byte_range(None, 0), Ok((0, 0)));
        for value in ["bytes=-0", "bytes=1-0", "bytes=1-2,4-5", "bytes=100-"] {
            assert!(byte_range(Some(value), 100).is_err());
        }
    }
}
