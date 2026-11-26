mod fixtures;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use fixtures::{server, Error, TestServer};
use rstest::rstest;
use sha2::{Digest, Sha256, Sha512};

fn sha256(data: &[u8]) -> String {
    format!("sha-256=:{}:", STANDARD.encode(Sha256::digest(data)))
}

fn sha512(data: &[u8]) -> String {
    format!("sha-512=:{}:", STANDARD.encode(Sha512::digest(data)))
}

#[rstest]
fn file_digests(server: TestServer) -> Result<(), Error> {
    let url = format!("{}test.txt", server.url());
    let client = reqwest::blocking::Client::new();
    let full = b"This is test.txt";
    let response = client
        .get(&url)
        .header("Want-Content-Digest", "sha-256=1;note=\"a,b\", sha-512=9")
        .header("Want-Repr-Digest", "sha-256=10")
        .send()?;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-digest"], sha512(full));
    assert_eq!(response.headers()["repr-digest"], sha256(full));
    assert_eq!(response.bytes()?.as_ref(), full);

    let response = client
        .head(&url)
        .header("Want-Content-Digest", "sha-256=10")
        .header("Want-Repr-Digest", "sha-256=10")
        .send()?;
    assert_eq!(response.headers()["content-digest"], sha256(b""));
    assert_eq!(response.headers()["repr-digest"], sha256(full));
    assert!(response.bytes()?.is_empty());

    let response = client
        .get(&url)
        .header("Range", "bytes=2-5")
        .header("Want-Content-Digest", "sha-256=10")
        .header("Want-Repr-Digest", "sha-256=10")
        .send()?;
    assert_eq!(response.status(), 206);
    assert_eq!(response.headers()["content-digest"], sha256(&full[2..6]));
    assert_eq!(response.headers()["repr-digest"], sha256(full));
    assert_eq!(response.bytes()?.as_ref(), &full[2..6]);

    let response = client
        .get(&url)
        .header("Range", "bytes=0-1,4-5")
        .header("Want-Content-Digest", "sha-256=10")
        .header("Want-Repr-Digest", "sha-256=10")
        .send()?;
    assert_eq!(response.status(), 206);
    assert_eq!(response.headers()["repr-digest"], sha256(full));
    if let Some(digest) = response.headers().get("content-digest").cloned() {
        assert_eq!(digest, sha256(&response.bytes()?));
    }
    Ok(())
}

#[rstest]
fn upload_digests(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let url = format!("{}test.txt", server.url());
    let client = reqwest::blocking::Client::new();
    let original = b"This is test.txt";
    let response = client
        .put(&url)
        .header("Content-Digest", sha256(b"wrong"))
        .body("new")
        .send()?;
    assert_eq!(response.status(), 400);
    assert_eq!(client.get(&url).send()?.bytes()?.as_ref(), original);

    let response = client
        .put(&url)
        .header("Content-Digest", format!("{};note=\"a,b\"", sha256(b"new")))
        .header("Repr-Digest", sha512(b"new"))
        .body("new")
        .send()?;
    assert_eq!(response.status(), 204);

    let response = client
        .patch(&url)
        .header("X-Update-Range", "append")
        .header("Content-Digest", sha256(b"wrong"))
        .body("more")
        .send()?;
    assert_eq!(response.status(), 400);
    assert_eq!(client.get(&url).send()?.text()?, "new");

    let response = client
        .patch(&url)
        .header("X-Update-Range", "append")
        .header("Content-Digest", sha256(b"more"))
        .header("Repr-Digest", sha512(b"more"))
        .body("more")
        .send()?;
    assert_eq!(response.status(), 204);
    assert_eq!(client.get(&url).send()?.text()?, "newmore");
    Ok(())
}
