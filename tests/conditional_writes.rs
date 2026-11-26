// These tests use the real HTTP server and ordinary file representations.
use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::{Barrier, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

struct Server {
    port: u16,
    dir: tempfile::TempDir,
    child: Child,
}

impl Server {
    fn start() -> Self {
        // Keep port selection and server startup serialized within this test binary.
        static START: Mutex<()> = Mutex::new(());
        let _startup = START.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let child = Command::new(env!("CARGO_BIN_EXE_dufs"))
            .arg(dir.path())
            .args([
                "--bind",
                "127.0.0.1",
                "--port",
                &port.to_string(),
                "--allow-all",
            ])
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let server = Self { port, dir, child };
        wait_until(|| TcpStream::connect(("127.0.0.1", port)).is_ok());
        server
    }

    fn connect(&self) -> TcpStream {
        let stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
    }

    fn request(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Reply {
        let mut stream = self.connect();
        write!(
            stream,
            "{method} /{path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\nContent-Length: {}\r\n",
            self.port, body.len()
        )
        .unwrap();
        for (name, value) in headers {
            write!(stream, "{name}: {value}\r\n").unwrap();
        }
        write!(stream, "\r\n{body}").unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        Reply {
            status: head.split_whitespace().nth(1).unwrap().parse().unwrap(),
            headers: head.to_owned(),
            body: body.to_owned(),
        }
    }

    fn head(&self, path: &str) -> Reply {
        let response = self.request("HEAD", path, &[], "");
        assert_eq!(response.status, 200);
        response
    }

    fn put(&self, path: &str, body: &str) -> Reply {
        let response = self.request("PUT", path, &[], body);
        assert!(matches!(response.status, 201 | 204));
        response
    }

    fn temps(&self) -> Vec<std::path::PathBuf> {
        fs::read_dir(self.dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".dufs-upload-")
            })
            .collect()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Reply {
    status: u16,
    headers: String,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> &str {
        self.headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .unwrap_or_else(|| panic!("Missing {name} in {}", self.headers))
            .1
            .trim()
    }

    fn etag(&self) -> &str {
        let tag = self.header("ETag");
        assert!(tag.starts_with('"') && tag.ends_with('"'), "{tag}");
        tag
    }
}

fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "Timed out waiting for server");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn sync_probe() {
    let server = Server::start();
    for conditional in [false, true] {
        let path = if conditional {
            ".probe-conditional"
        } else {
            ".probe-unconditional"
        };
        let headers: &[(&str, &str)] = if conditional {
            &[("If-None-Match", "*")]
        } else {
            &[]
        };
        let created = server.request("PUT", path, headers, "{}");
        assert_eq!(created.status, 201);
        let before = server.request("GET", path, &[], "");
        assert_eq!(before.status, 200);
        assert_eq!(before.body, "{}");
        assert_eq!(created.etag(), before.etag());
        before.header("Date");
        let modified = before.header("Last-Modified");
        let prop = server.request("PROPFIND", path, &[("Depth", "0")], "");
        assert_eq!(prop.status, 207);
        prop.header("Date");
        assert!(prop
            .body
            .contains(&format!("<D:getetag>{}</D:getetag>", before.etag())));
        assert!(prop.body.contains(&format!(
            "<D:getlastmodified>{modified}</D:getlastmodified>"
        )));
        for (method, name, value) in [
            ("PUT", "If-None-Match", "*"),
            ("PUT", "If-Match", "\"invalid\""),
            ("DELETE", "If-Match", "\"invalid\""),
        ] {
            let rejected = server.request(
                method,
                path,
                &[(name, value)],
                if method == "DELETE" { "" } else { "{}" },
            );
            assert_eq!(rejected.status, 412);
        }
        let replaced = server.request(
            "PUT",
            path,
            &[("If-Match", before.etag())],
            "{\"probe\":true}",
        );
        assert_eq!(replaced.status, 204);
        let after = server.request("GET", path, &[], "");
        assert_eq!(after.status, 200);
        assert_eq!(after.body, "{\"probe\":true}");
        assert_ne!(before.etag(), after.etag());
        assert_eq!(replaced.etag(), after.etag());
        assert_eq!(
            server
                .request("DELETE", path, &[("If-Match", after.etag())], "")
                .status,
            204
        );
        assert_eq!(server.request("GET", path, &[], "").status, 404);
    }
}

#[test]
fn compare_exchange_put() {
    let server = Server::start();
    let initial = server.put("save", "initial");
    let barrier = Barrier::new(8);
    let results = thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let (server, barrier, tag) = (&server, &barrier, initial.etag());
                scope.spawn(move || {
                    let body = format!("save-{i}");
                    barrier.wait();
                    let reply = server.request("PUT", "save", &[("If-Match", tag)], &body);
                    (reply, body)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert!(results.iter().all(|(r, _)| matches!(r.status, 204 | 412)));
    let winners: Vec<_> = results.iter().filter(|(r, _)| r.status == 204).collect();
    assert_eq!(winners.len(), 1);
    let (winner, body) = winners[0];
    let final_reply = server.request("GET", "save", &[], "");
    assert_eq!(&final_reply.body, body);
    assert_eq!(final_reply.etag(), winner.etag());
    assert_ne!(final_reply.etag(), initial.etag());
}

#[test]
fn interrupted_upload_keeps_old_file() {
    let server = Server::start();
    let initial = server.put("save", "old-content");
    let mut stream = server.connect();
    write!(
        stream,
        "PUT /save HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Length: 4096\r\nIf-Match: {}\r\n\r\npartial",
        server.port, initial.etag()
    ).unwrap();
    wait_until(|| !server.temps().is_empty());

    // A reader sees the last complete representation; unrelated writes can proceed.
    let during = server.request("GET", "save", &[], "");
    assert_eq!(during.body, "old-content");
    assert_eq!(during.etag(), initial.etag());
    server.put("other", "independent");
    for (method, path, headers) in [
        ("PROPFIND", "", &[("Depth", "1")][..]),
        ("GET", "?json", &[][..]),
    ] {
        let listing = server.request(method, path, headers, "");
        assert!(matches!(listing.status, 200 | 207));
        assert!(!listing.body.contains(".dufs-upload-"));
        assert!(listing.body.contains("save"));
    }
    stream.shutdown(Shutdown::Both).unwrap();
    drop(stream);
    wait_until(|| server.temps().is_empty());
    let after = server.request("GET", "save", &[], "");
    assert_eq!(after.body, "old-content");
    assert_eq!(after.etag(), initial.etag());
    assert_eq!(
        server
            .request(
                "PUT",
                "save",
                &[("If-Match", initial.etag())],
                "new-content"
            )
            .status,
        204
    );
}

#[test]
fn same_mtime_writes_change_etag() {
    let server = Server::start();
    let initial = server.put("save", "same");
    let same = server.request("PUT", "save", &[("If-Match", initial.etag())], "same");
    assert_eq!(same.status, 204);
    assert_ne!(same.etag(), initial.etag());
    assert_eq!(server.head("save").etag(), same.etag());

    // Force exactly the same mtime and size on successive file objects.
    // The Unix identity fields must still distinguish the representations.
    #[cfg(unix)]
    {
        let path = server.dir.path().join("save");
        let time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        fs::File::open(&path).unwrap().set_modified(time).unwrap();
        let old = server.head("save");
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let replaced = server.request("PUT", "save", &[("If-Match", old.etag())], "same");
        assert_eq!(replaced.status, 204);
        fs::File::open(&path).unwrap().set_modified(time).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
        let new = server.head("save");
        assert_eq!(old.header("Last-Modified"), new.header("Last-Modified"));
        assert_ne!(old.etag(), new.etag());
    }
}

#[test]
fn delete_and_patch_preconditions() {
    let server = Server::start();
    let saved = server.put("save", "abc");
    for method in ["PUT", "PATCH", "DELETE", "MOVE", "COPY"] {
        assert_eq!(
            server
                .request(
                    method,
                    "missing",
                    &[
                        ("If-Match", "*"),
                        ("Destination", "/new"),
                        ("X-Update-Range", "append")
                    ],
                    ""
                )
                .status,
            if method == "PUT" { 412 } else { 404 },
            "{method}"
        );
    }
    let weak = format!("W/{}", saved.etag());
    let stale = "Sat, 01 Jan 2000 00:00:00 GMT";
    for method in ["DELETE", "PATCH"] {
        for (name, value) in [
            ("If-Match", weak.as_str()),
            ("If-None-Match", "*"),
            ("If-None-Match", weak.as_str()),
            ("If-Unmodified-Since", stale),
        ] {
            assert_eq!(
                server
                    .request(
                        method,
                        "save",
                        &[(name, value), ("X-Update-Range", "append")],
                        if method == "DELETE" { "" } else { "def" }
                    )
                    .status,
                412
            );
        }
    }
    let patched = server.request(
        "PATCH",
        "save",
        &[
            ("If-Match", saved.etag()),
            ("If-Unmodified-Since", stale),
            ("X-Update-Range", "append"),
        ],
        "def",
    );
    assert_eq!(patched.status, 204);
    assert_ne!(patched.etag(), saved.etag());
    assert_eq!(server.request("GET", "save", &[], "").body, "abcdef");
    assert_eq!(server.head("save").etag(), patched.etag());
    assert_eq!(
        server
            .request("DELETE", "save", &[("If-Match", saved.etag())], "")
            .status,
        412
    );
    assert_eq!(
        server
            .request("DELETE", "save", &[("If-Match", patched.etag())], "")
            .status,
        204
    );
}

#[test]
fn copy_and_move_preconditions() {
    let server = Server::start();
    for method in ["COPY", "MOVE"] {
        let source = server.put("source", "new");
        let destination = server.put("dest", "old");
        for (name, value) in [
            ("If-Match", "\"invalid\""),
            ("If-None-Match", "*"),
            ("If-Unmodified-Since", "Sat, 01 Jan 2000 00:00:00 GMT"),
            ("Overwrite", "F"),
        ] {
            let response = server.request(
                method,
                "source",
                &[("Destination", "/dest"), (name, value)],
                "",
            );
            assert_eq!(response.status, 412, "{method} {name}");
            assert_eq!(server.head("dest").etag(), destination.etag());
            assert_eq!(server.head("source").etag(), source.etag());
        }
        let response = server.request(
            method,
            "source",
            &[
                ("Destination", "/dest"),
                ("If-Match", source.etag()),
                ("Overwrite", "T"),
            ],
            "",
        );
        assert_eq!(response.status, 204);
        let final_reply = server.request("GET", "dest", &[], "");
        assert_eq!(final_reply.body, "new");
        assert_eq!(final_reply.etag(), response.etag());
        assert_ne!(final_reply.etag(), destination.etag());
        assert_eq!(
            server.request("GET", "source", &[], "").status,
            if method == "COPY" { 200 } else { 404 }
        );
    }
}

#[test]
fn copy_move_destination_is_serialized() {
    let server = Server::start();
    let a = server.put("a", "a");
    let b = server.put("b", "b");
    let barrier = Barrier::new(2);
    let results = thread::scope(|scope| {
        let handles: Vec<_> = [("COPY", "a", a.etag()), ("MOVE", "b", b.etag())]
            .into_iter()
            .map(|(method, path, tag)| {
                let (server, barrier) = (&server, &barrier);
                scope.spawn(move || {
                    barrier.wait();
                    let reply = server.request(
                        method,
                        path,
                        &[
                            ("Destination", "/dest"),
                            ("Overwrite", "F"),
                            ("If-Match", tag),
                        ],
                        "",
                    );
                    (reply.status, path)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect::<Vec<_>>()
    });
    let winners: Vec<_> = results.iter().filter(|r| r.0 == 201).collect();
    assert_eq!(winners.len(), 1);
    assert_eq!(results.iter().filter(|r| r.0 == 412).count(), 1);
    assert_eq!(server.request("GET", "dest", &[], "").body, winners[0].1);
}

#[test]
fn interrupted_patch_keeps_complete_representation() {
    let server = Server::start();
    let saved = server.put("save", "old-content");
    let mut stream = server.connect();
    write!(stream, "PATCH /save HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Length: 4096\r\nIf-Match: {}\r\nX-Update-Range: append\r\n\r\npartial", server.port, saved.etag()).unwrap();
    wait_until(|| !server.temps().is_empty());
    assert_eq!(server.request("GET", "save", &[], "").body, "old-content");
    stream.shutdown(Shutdown::Both).unwrap();
    drop(stream);
    wait_until(|| server.temps().is_empty());
    let after = server.request("GET", "save", &[], "");
    assert_eq!(after.body, "old-content");
    assert_eq!(after.etag(), saved.etag());
}
