mod fixtures;
mod utils;

use fixtures::{server, Error, TestServer};
use rstest::rstest;
use tokio::io::AsyncReadExt;

#[rstest]
fn negotiated_zstd(server: TestServer) -> Result<(), Error> {
    let url = format!("{}index.html", server.url());
    let plain = fetch!(b"GET", &url).send()?;
    let tag = plain.headers()["ETag"].clone();
    assert_eq!(plain.text()?, "This is index.html");
    let encoded = fetch!(b"GET", &url)
        .header("Accept-Encoding", "zstd")
        .send()?;
    assert_eq!(encoded.status(), 200);
    assert_eq!(encoded.headers()["Content-Encoding"], "zstd");
    assert_eq!(encoded.headers()["Vary"], "Accept-Encoding");
    let encoded_tag = encoded.headers()["ETag"].clone();
    assert_ne!(encoded_tag, tag);
    let bytes = encoded.bytes()?;
    assert_eq!(&bytes[..4], &[0x28, 0xb5, 0x2f, 0xfd]);
    let runtime = tokio::runtime::Runtime::new()?;
    let decoded = runtime.block_on(async {
        let mut decoder = async_compression::tokio::bufread::ZstdDecoder::new(&bytes[..]);
        let mut result = String::new();
        decoder.read_to_string(&mut result).await?;
        Ok::<_, std::io::Error>(result)
    })?;
    assert_eq!(decoded, "This is index.html");
    let head = fetch!(b"HEAD", &url)
        .header("Accept-Encoding", "zstd")
        .send()?;
    assert_eq!(head.headers()["ETag"], encoded_tag);
    assert_eq!(head.headers()["Content-Encoding"], "zstd");
    assert!(head.bytes()?.is_empty());
    let cached = fetch!(b"GET", &url)
        .header("Accept-Encoding", "zstd")
        .header("If-None-Match", encoded_tag)
        .send()?;
    assert_eq!(cached.status(), 304);
    assert_eq!(cached.headers()["Vary"], "Accept-Encoding");

    for accept in ["zstd;q=0", "zstd;q=0.5, identity;q=1", "*;q=1, zstd;q=0"] {
        let resp = fetch!(b"GET", &url)
            .header("Accept-Encoding", accept)
            .send()?;
        assert_eq!(resp.status(), 200);
        assert!(resp.headers().get("Content-Encoding").is_none());
        assert_eq!(resp.headers()["ETag"], tag);
    }
    let resp = fetch!(b"GET", &url)
        .header("Accept-Encoding", "*;q=0")
        .send()?;
    assert_eq!(resp.status(), 406);
    let resp = fetch!(b"GET", &url)
        .header("Accept-Encoding", "zstd")
        .header("Range", "bytes=0-3")
        .send()?;
    assert_eq!(resp.status(), 206);
    assert!(resp.headers().get("Content-Encoding").is_none());
    assert_eq!(resp.text()?, "This");
    Ok(())
}

#[rstest]
fn unsupported_upload_coding(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let url = format!("{}index.html", server.url());
    let resp = fetch!(b"PUT", &url)
        .header("Content-Encoding", "zstd")
        .body("not compressed")
        .send()?;
    assert_eq!(resp.status(), 415);
    assert_eq!(resp.headers()["Accept-Encoding"], "identity");
    assert_eq!(fetch!(b"GET", &url).send()?.text()?, "This is index.html");
    Ok(())
}

#[rstest]
fn encoded_validator_preserves_conditional_writes(
    #[with(&["-A"])] server: TestServer,
) -> Result<(), Error> {
    let url = format!("{}index.html", server.url());
    let response = fetch!(b"GET", &url)
        .header("Accept-Encoding", "zstd")
        .send()?;
    let tag = response.headers()["ETag"].clone();
    assert!(!tag.to_str()?.starts_with("W/"));
    let rejected = fetch!(b"PUT", &url)
        .header("If-None-Match", tag.clone())
        .body("no")
        .send()?;
    assert_eq!(rejected.status(), 412);
    let updated = fetch!(b"PUT", &url)
        .header("If-Match", tag.clone())
        .body("new")
        .send()?;
    assert_eq!(updated.status(), 204);
    let stale = fetch!(b"PUT", &url)
        .header("If-Match", tag.clone())
        .body("stale")
        .send()?;
    assert_eq!(stale.status(), 412);
    assert_eq!(fetch!(b"GET", &url).send()?.text()?, "new");
    Ok(())
}

#[rstest]
fn zstd_only_and_preferred_representation(
    #[with(&["-A"])] server: TestServer,
) -> Result<(), Error> {
    let url = format!("{}index.html", server.url());
    let response = fetch!(b"GET", &url)
        .header("Accept-Encoding", "identity;q=0, zstd;q=0.5")
        .header("Range", "bytes=0-3")
        .send()?;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["Content-Encoding"], "zstd");
    assert!(response.headers().get("Content-Range").is_none());
    let response = fetch!(b"PUT", &url)
        .header("Accept-Encoding", "zstd")
        .header("Prefer", "return=representation")
        .body("updated")
        .send()?;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["Content-Encoding"], "zstd");
    assert_eq!(
        response.headers()["Preference-Applied"],
        "return=representation"
    );
    let archive = format!("{}file.zst", server.url());
    assert_eq!(fetch!(b"PUT", &archive).body("data").send()?.status(), 201);
    let response = fetch!(b"GET", &archive).send()?;
    assert_eq!(response.headers()["Content-Type"], "application/zstd");
    Ok(())
}
