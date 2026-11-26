mod fixtures;
mod utils;

use fixtures::{server, Error, TestServer};
use rstest::rstest;

#[rstest]
fn quota_properties(server: TestServer) -> Result<(), Error> {
    let response = fetch!(b"PROPFIND", server.url())
        .header("Depth", "0")
        .body(r#"<D:propfind xmlns:D="DAV:"><D:prop><D:quota-available-bytes/><D:quota-used-bytes/></D:prop></D:propfind>"#)
        .send()?;
    assert_eq!(response.status(), 207);
    let body = response.text()?;
    for property in ["quota-available-bytes", "quota-used-bytes"] {
        let value = body
            .split(&format!("<D:{property}>"))
            .nth(1)
            .unwrap()
            .split('<')
            .next()
            .unwrap();
        assert!(value.parse::<u64>().is_ok());
    }
    let names = fetch!(b"PROPFIND", server.url())
        .header("Depth", "0")
        .body(r#"<D:propfind xmlns:D="DAV:"><D:propname/></D:propfind>"#)
        .send()?
        .text()?;
    assert!(names.contains("<D:quota-available-bytes/>"));
    let all = fetch!(b"PROPFIND", server.url())
        .header("Depth", "0")
        .send()?
        .text()?;
    assert!(!all.contains("quota-available-bytes"));
    Ok(())
}

#[rstest]
fn minimal_and_depth_noroot(server: TestServer) -> Result<(), Error> {
    let response = fetch!(b"PROPFIND", server.url())
        .header("Depth", "1")
        .header("Prefer", "return=minimal, depth-noroot")
        .body(r#"<D:propfind xmlns:D="DAV:"><D:prop><D:resourcetype/><D:unknown/></D:prop></D:propfind>"#)
        .send()?;
    assert_eq!(response.status(), 207);
    assert_eq!(
        response.headers()["Preference-Applied"],
        "return=minimal, depth-noroot"
    );
    let body = response.text()?;
    assert!(!body.contains("<D:href>/</D:href>"));
    assert!(body.contains("<D:href>/dir1/</D:href>"));
    assert!(!body.contains("404 Not Found"));
    let response = fetch!(b"PROPFIND", server.url())
        .header("Depth", "0")
        .header("Prefer", "return=minimal, depth-noroot")
        .body(r#"<D:propfind xmlns:D="DAV:"><D:prop><D:unknown/></D:prop></D:propfind>"#)
        .send()?;
    assert_eq!(response.headers()["Preference-Applied"], "return=minimal");
    let body = response.text()?;
    assert!(body.contains("<D:href>/</D:href>"));
    assert!(body.contains("200 OK"));
    assert!(!body.contains("404 Not Found"));
    Ok(())
}

#[rstest]
fn return_representation(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let url = format!("{}preferred.txt", server.url());
    let created = fetch!(b"PUT", &url)
        .header("Prefer", "return=representation")
        .body("first")
        .send()?;
    assert_eq!(created.status(), 201);
    assert_eq!(
        created.headers()["Preference-Applied"],
        "return=representation"
    );
    assert_eq!(created.text()?, "first");
    let replaced = fetch!(b"PUT", &url)
        .header("Prefer", "return=representation")
        .body("second")
        .send()?;
    assert_eq!(replaced.status(), 200);
    assert_eq!(replaced.text()?, "second");
    let rejected = fetch!(b"PUT", &url)
        .header("Prefer", "return=representation")
        .header("If-None-Match", "*")
        .body("third")
        .send()?;
    assert_eq!(rejected.status(), 412);
    assert_eq!(
        rejected.headers()["Preference-Applied"],
        "return=representation"
    );
    assert_eq!(rejected.text()?, "second");
    let response = fetch!(b"PROPPATCH", &url).header("Prefer", "return=minimal")
        .body(r#"<D:propertyupdate xmlns:D="DAV:"><D:remove><D:prop><D:custom/></D:prop></D:remove></D:propertyupdate>"#).send()?;
    assert_eq!(response.status(), 204);
    assert_eq!(response.headers()["Preference-Applied"], "return=minimal");
    Ok(())
}

#[rstest]
fn extended_mkcol(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let url = format!("{}extended/", server.url());
    let options = fetch!(b"OPTIONS", &url).send()?;
    assert!(options.headers()["DAV"]
        .to_str()?
        .contains("extended-mkcol"));
    let response = fetch!(b"MKCOL", &url).header("Content-Type", "application/xml")
        .header("Prefer", "return=minimal")
        .body(r#"<D:mkcol xmlns:D="DAV:"><D:set><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop></D:set></D:mkcol>"#).send()?;
    assert_eq!(response.status(), 201);
    assert_eq!(response.headers()["Preference-Applied"], "return=minimal");
    assert!(response.text()?.is_empty());
    let failed = format!("{}unsupported/", server.url());
    let response = fetch!(b"MKCOL", &failed).header("Content-Type", "application/xml")
        .body(r#"<D:mkcol xmlns:D="DAV:" xmlns:X="urn:test"><D:set><D:prop><D:resourcetype><D:collection/><X:special/></D:resourcetype><X:custom/></D:prop></D:set></D:mkcol>"#).send()?;
    assert_eq!(response.status(), 403);
    let body = response.text()?;
    assert!(body.contains("<D:mkcol-response"));
    assert!(body.contains("valid-resourcetype"));
    assert_eq!(
        fetch!(b"PROPFIND", &failed)
            .header("Depth", "0")
            .send()?
            .status(),
        404
    );
    let rejected = format!("{}rollback/", server.url());
    let response = fetch!(b"MKCOL", &rejected).header("Content-Type", "text/xml")
        .body(r#"<D:mkcol xmlns:D="DAV:"><D:set><D:prop><D:resourcetype><D:collection/></D:resourcetype><D:displayname>Custom</D:displayname></D:prop></D:set></D:mkcol>"#).send()?;
    assert_eq!(response.status(), 403);
    assert!(response.text()?.contains("424 Failed Dependency"));
    assert_eq!(
        fetch!(b"PROPFIND", &rejected)
            .header("Depth", "0")
            .send()?
            .status(),
        404
    );
    Ok(())
}
