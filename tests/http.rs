//! End-to-end tests launch the real binary on synthetic card directories only.
use std::collections::HashMap;
use std::fs::{File, FileTimes};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, UNIX_EPOCH};

const GUID: &str = "{FA82B2D0-03D3-e144-B987-7AB6F743C201}";

struct Fixture {
    child: Child,
    temp: tempfile::TempDir,
    address: String,
    base: String,
    bytes: Vec<u8>,
}

struct Reply {
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl Fixture {
    fn new() -> Self {
        Self::new_mode("webdav")
    }

    fn new_mode(mode: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::write(root.join(".pmingest.dat"), GUID).unwrap();
        std::fs::create_dir_all(root.join("DCIM/100LEICA")).unwrap();
        let bytes = (0..2_560_000).map(|i| (i % 256) as u8).collect::<Vec<_>>();
        let photo = root.join("DCIM/100LEICA/A & B.DNG");
        std::fs::write(&photo, &bytes).unwrap();
        File::open(photo)
            .unwrap()
            .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1789359132)))
            .unwrap();
        std::os::unix::fs::symlink("/etc/hosts", root.join("DCIM/100LEICA/escape.JPG")).unwrap();
        std::os::unix::fs::symlink("/etc", root.join("escape")).unwrap();
        let history = root.join("test-history");
        if mode == "filtered" {
            for (name, bytes) in [
                ("NEW.JPG", b"new".as_slice()),
                ("CHANGED.DNG", b"changed".as_slice()),
            ] {
                let path = root.join("DCIM/100LEICA").join(name);
                std::fs::write(&path, bytes).unwrap();
                File::open(path)
                    .unwrap()
                    .set_times(
                        FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1789359132)),
                    )
                    .unwrap();
            }
            std::fs::write(
                &history,
                concat!(
                    "/DCIM/100LEICA/A & B.DNG\t2560000\t1789413132\n",
                    "DCIM/100LEICA/A & B.DNG\t2560000\t1789413132\n",
                    "/DCIM/100LEICA/CHANGED.DNG\t8\t1789413132\n"
                ),
            )
            .unwrap();
        }
        let mut command = Command::new(env!("CARGO_BIN_EXE_diskproxy-server"));
        command
            .arg(root)
            .arg("--plugin=pmp")
            .arg("--source-offset=+08:00")
            .arg("--target-offset=-07:00")
            .arg("--profiles-dir")
            .arg(root.join("profiles"))
            .arg("--mode")
            .arg(mode);
        if mode == "filtered" {
            command.arg("--history").arg(&history);
        }
        let mut child = command.stdout(Stdio::piped()).spawn().unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let startup: serde_json::Value = serde_json::from_str(&line).expect("startup JSON");
        assert_eq!(startup["guid"], GUID);
        let url = startup["url"]
            .as_str()
            .unwrap()
            .strip_prefix("http://")
            .unwrap();
        let (address, path) = url.split_once('/').unwrap();
        Self {
            child,
            temp,
            address: address.into(),
            base: format!("/{path}"),
            bytes,
        }
    }

    fn request(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Reply {
        let path = format!("{}{path}", self.base);
        self.absolute(method, &path, headers, body)
    }

    fn absolute(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Reply {
        let mut stream = TcpStream::connect(&self.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: {}\r\n",
            self.address,
            body.len()
        )
        .unwrap();
        for (name, value) in headers {
            write!(stream, "{name}: {value}\r\n").unwrap();
        }
        write!(stream, "\r\n{body}").unwrap();
        let mut data = Vec::new();
        stream.read_to_end(&mut data).unwrap();
        let split = data.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = std::str::from_utf8(&data[..split]).unwrap();
        let status = head
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let headers = head
            .lines()
            .skip(1)
            .map(|l| {
                let (name, value) = l.split_once(':').unwrap();
                (name.to_ascii_lowercase(), value.trim().into())
            })
            .collect();
        Reply {
            status,
            headers,
            body: data[split + 4..].to_vec(),
        }
    }

    fn photo(&self) -> PathBuf {
        self.temp.path().join("DCIM/100LEICA/A & B.DNG")
    }
}

#[test]
fn filtered_mode_normalizes_history_paths_and_hides_only_exact_matches() {
    let f = Fixture::new_mode("filtered");
    let original_history = std::fs::read(f.temp.path().join("test-history")).unwrap();
    for method in ["GET", "HEAD", "PROPFIND"] {
        assert_eq!(
            f.request(
                method,
                "DCIM/100LEICA/A%20%26%20B.DNG",
                &[("Depth", "0")],
                ""
            )
            .status,
            404
        );
    }
    let r = f.request("PROPFIND", "DCIM/100LEICA/", &[("Depth", "1")], "");
    assert_eq!(r.status, 207);
    let xml = std::str::from_utf8(&r.body).unwrap();
    let doc = roxmltree::Document::parse(xml).unwrap();
    assert_eq!(
        doc.descendants()
            .filter(|n| n.has_tag_name(("DAV:", "response")))
            .count(),
        3
    );
    assert!(!xml.contains("A%20%26%20B"));
    assert_eq!(
        f.request("GET", "DCIM/100LEICA/NEW.JPG", &[], "").body,
        b"new"
    );
    assert_eq!(
        f.request("GET", "DCIM/100LEICA/CHANGED.DNG", &[], "").body,
        b"changed"
    );
    assert_eq!(
        f.request("GET", ".pmingest.dat", &[], "").body,
        GUID.as_bytes()
    );
    assert!(std::fs::read(f.photo()).unwrap() == f.bytes);
    assert_eq!(
        std::fs::read(f.temp.path().join("test-history")).unwrap(),
        original_history
    );
}

#[test]
fn disk_mode_serves_exfat_boot_sector_and_rejects_writes() {
    let f = Fixture::new_mode("disk");
    // Disk startup URL already ends in card.img.
    let r = f.request("GET", "", &[("Range", "bytes=0-511")], "");
    assert_eq!(r.status, 206);
    assert_eq!(r.body.len(), 512);
    assert_eq!(&r.body[3..11], b"EXFAT   ");
    assert_eq!(&r.body[510..512], &[0x55, 0xaa]);
    assert_eq!(f.request("PUT", "", &[], "replace").status, 403);
    assert_eq!(f.request("GET", "/DCIM", &[], "").status, 404);
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn bytes_marker_head_and_corrected_mtime() {
    let fixture = Fixture::new();
    let response = fixture.request("GET", "DCIM/100LEICA/A%20%26%20B.DNG", &[], "");
    assert_eq!(response.status, 200);
    assert!(
        response.body == fixture.bytes,
        "streamed image bytes differ"
    );
    assert_eq!(
        response.headers["last-modified"],
        "Mon, 14 Sep 2026 19:12:12 GMT"
    );
    let head = fixture.request("HEAD", "DCIM/100LEICA/A%20%26%20B.DNG", &[], "");
    assert_eq!(head.status, 200);
    assert!(head.body.is_empty());
    assert_eq!(
        head.headers["content-length"],
        fixture.bytes.len().to_string()
    );
    assert_eq!(
        fixture.request("GET", ".pmingest.dat", &[], "").body,
        GUID.as_bytes()
    );
    assert_eq!(
        std::fs::metadata(fixture.photo())
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        1789359132
    );
}

#[test]
fn range_reads_and_invalid_ranges() {
    let f = Fixture::new();
    for (value, expected) in [
        ("bytes=17-35", &f.bytes[17..36]),
        ("bytes=-19", &f.bytes[f.bytes.len() - 19..]),
        ("bytes=2559990-", &f.bytes[2559990..]),
    ] {
        let r = f.request(
            "GET",
            "DCIM/100LEICA/A%20%26%20B.DNG",
            &[("Range", value)],
            "",
        );
        assert_eq!(r.status, 206);
        assert_eq!(r.body, expected);
    }
    for value in ["bytes=-0", "bytes=1-0", "bytes=1-2,4-5", "bytes=999999999-"] {
        assert_eq!(
            f.request("GET", ".pmingest.dat", &[("Range", value)], "")
                .status,
            416
        );
    }
}

#[test]
fn dav_listing_escaping_and_property_statuses() {
    let f = Fixture::new();
    let r = f.request("OPTIONS", "", &[], "");
    assert_eq!(r.status, 200);
    assert_eq!(r.headers["dav"], "1");
    for _ in 0..2 {
        let r = f.request("PROPFIND", "DCIM/100LEICA/", &[("Depth", "1")], "");
        assert_eq!(r.status, 207);
        let xml = std::str::from_utf8(&r.body).unwrap();
        let doc = roxmltree::Document::parse(xml).unwrap();
        assert_eq!(
            doc.descendants()
                .filter(|n| n.has_tag_name(("DAV:", "response")))
                .count(),
            2
        );
        assert!(xml.contains("A%20%26%20B%2EDNG"));
        assert!(!xml.contains("escape"));
    }
    let body = "<D:propfind xmlns:D=\"DAV:\"><D:prop><D:getlastmodified/><unknown/><D:unknown/></D:prop></D:propfind>";
    let r = f.request("PROPFIND", ".pmingest.dat", &[("Depth", "0")], body);
    assert_eq!(r.status, 207);
    let xml = std::str::from_utf8(&r.body).unwrap();
    roxmltree::Document::parse(xml).unwrap();
    assert!(xml.contains("404 Not Found"));
    assert_eq!(
        f.request("PROPFIND", "", &[("Depth", "infinity")], "")
            .status,
        403
    );
    assert_eq!(
        f.request("PROPFIND", "", &[("Depth", "0")], "<broken>")
            .status,
        400
    );
}

#[test]
fn writes_and_path_escapes_are_rejected() {
    let f = Fixture::new();
    for method in [
        "PUT",
        "DELETE",
        "MKCOL",
        "MOVE",
        "COPY",
        "PROPPATCH",
        "LOCK",
        "UNLOCK",
        "POST",
        "PATCH",
    ] {
        assert_eq!(
            f.request(method, ".pmingest.dat", &[], "replace").status,
            403
        );
    }
    for path in [
        "../etc/hosts",
        "%2e%2e/etc/hosts",
        "escape/hosts",
        "DCIM/100LEICA/escape.JPG",
        "DCIM//100LEICA/a.DNG",
    ] {
        assert!(matches!(f.request("GET", path, &[], "").status, 403 | 404));
    }
    assert_eq!(
        f.absolute("GET", "/wrong-token/.pmingest.dat", &[], "")
            .status,
        404
    );
    assert_eq!(
        std::fs::read(f.temp.path().join(".pmingest.dat")).unwrap(),
        GUID.as_bytes()
    );
    assert!(
        std::fs::read(f.photo()).unwrap() == f.bytes,
        "source bytes changed"
    );
}
