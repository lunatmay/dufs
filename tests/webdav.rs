mod fixtures;
mod utils;

use chrono::{DateTime, Duration};
use fixtures::{server, Error, TestServer, FILES};
use reqwest::header::{ETAG, IF_MATCH, IF_NONE_MATCH, IF_UNMODIFIED_SINCE, LAST_MODIFIED};
use rstest::rstest;
use xml::escape::escape_str_pcdata;

fn file_etag(server: &TestServer, name: &str) -> Result<String, Error> {
    let resp = fetch!(b"HEAD", format!("{}{}", server.url(), name)).send()?;
    Ok(resp
        .headers()
        .get(ETAG)
        .and_then(|v| v.to_str().ok())
        .expect("Received no valid etag header")
        .to_owned())
}

fn file_last_modified(server: &TestServer, name: &str) -> Result<String, Error> {
    let resp = fetch!(b"HEAD", format!("{}{}", server.url(), name)).send()?;
    Ok(resp
        .headers()
        .get(LAST_MODIFIED)
        .and_then(|v| v.to_str().ok())
        .expect("Received no valid last modified header")
        .to_owned())
}

#[rstest]
fn propfind_dir(server: TestServer) -> Result<(), Error> {
    let resp = fetch!(b"PROPFIND", format!("{}dir1", server.url()))
        .header("depth", "1")
        .send()?;
    assert_eq!(resp.status(), 207);
    let body = resp.text()?;
    assert!(body.contains("<D:href>/dir1/</D:href>"));
    assert!(body.contains("<D:displayname>dir1</D:displayname>"));
    for f in FILES {
        assert!(body.contains(&format!("<D:href>/dir1/{}</D:href>", utils::encode_uri(f))));
        assert!(body.contains(&format!(
            "<D:displayname>{}</D:displayname>",
            escape_str_pcdata(f)
        )));
    }
    Ok(())
}

#[rstest]
fn propfind_dir_depth0(server: TestServer) -> Result<(), Error> {
    let resp = fetch!(b"PROPFIND", format!("{}dir1", server.url()))
        .header("depth", "0")
        .send()?;
    assert_eq!(resp.status(), 207);
    let body = resp.text()?;
    assert!(body.contains("<D:href>/dir1/</D:href>"));
    assert!(body.contains("<D:displayname>dir1</D:displayname>"));
    assert_eq!(
        body.lines()
            .filter(|v| *v == "<D:status>HTTP/1.1 200 OK</D:status>")
            .count(),
        1
    );
    Ok(())
}

#[rstest]
fn propfind_dir_depth2(server: TestServer) -> Result<(), Error> {
    let resp = fetch!(b"PROPFIND", format!("{}dir1", server.url()))
        .header("depth", "2")
        .send()?;
    assert_eq!(resp.status(), 400);
    let body = resp.text()?;
    assert_eq!(body, "Invalid depth: only 0 and 1 are allowed.");
    Ok(())
}

#[rstest]
fn propfind_dir_depth_infinity(server: TestServer) -> Result<(), Error> {
    let resp = fetch!(b"PROPFIND", format!("{}dir1", server.url()))
        .header("depth", "infinity")
        .send()?;
    assert_eq!(resp.status(), 403);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/xml; charset=utf-8")
    );
    let body = resp.text()?;
    assert!(body.contains("<D:propfind-finite-depth/>"));
    Ok(())
}

#[rstest]
fn propfind_404(server: TestServer) -> Result<(), Error> {
    let resp = fetch!(b"PROPFIND", format!("{}404", server.url())).send()?;
    assert_eq!(resp.status(), 404);
    Ok(())
}

#[rstest]
fn propfind_double_slash(server: TestServer) -> Result<(), Error> {
    let resp = fetch!(b"PROPFIND", server.url())
        .header("depth", "1")
        .send()?;
    assert_eq!(resp.status(), 207);
    Ok(())
}

#[rstest]
fn propfind_file(server: TestServer) -> Result<(), Error> {
    let resp = fetch!(b"PROPFIND", format!("{}test.html", server.url())).send()?;
    assert_eq!(resp.status(), 207);
    let body = resp.text()?;
    assert!(body.contains("<D:href>/test.html</D:href>"));
    assert!(body.contains("<D:displayname>test.html</D:displayname>"));
    assert_eq!(
        body.lines()
            .filter(|v| *v == "<D:status>HTTP/1.1 200 OK</D:status>")
            .count(),
        1
    );
    Ok(())
}

#[rstest]
fn propfind_file_getetag(server: TestServer) -> Result<(), Error> {
    let etag = file_etag(&server, "test.html")?;
    let resp = fetch!(b"PROPFIND", format!("{}test.html", server.url()))
        .header("depth", "0")
        .send()?;
    assert_eq!(resp.status(), 207);
    let body = resp.text()?;
    assert!(body.contains(&format!("<D:getetag>{etag}</D:getetag>")));
    Ok(())
}

#[rstest]
fn propfind_dir_getetag(server: TestServer) -> Result<(), Error> {
    let resp = fetch!(b"PROPFIND", format!("{}dir1", server.url()))
        .header("depth", "1")
        .send()?;
    assert_eq!(resp.status(), 207);
    let body = resp.text()?;

    for name in ["dir1/test.txt", "dir1/test.html", "dir1/index.html"] {
        let etag = file_etag(&server, name)?;
        assert!(
            body.contains(&format!("<D:getetag>{etag}</D:getetag>")),
            "missing getetag for {name}"
        );
    }
    Ok(())
}

#[rstest]
fn propfind_getetag_if_match(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let resp = fetch!(b"PROPFIND", format!("{}test.txt", server.url()))
        .header("depth", "0")
        .send()?;
    assert_eq!(resp.status(), 207);
    let body = resp.text()?;
    let etag = body
        .split("<D:getetag>")
        .nth(1)
        .and_then(|v| v.split("</D:getetag>").next())
        .expect("PROPFIND response has no getetag");

    let resp = fetch!(b"PUT", format!("{}test.txt", server.url()))
        .header(IF_MATCH, etag)
        .body("changed")
        .send()?;
    assert_eq!(resp.status(), 204);

    let new_etag = file_etag(&server, "test.txt")?;
    assert_ne!(etag, new_etag.as_str());
    Ok(())
}

#[rstest]
fn proppatch_file(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let resp = fetch!(b"PROPPATCH", format!("{}test.html", server.url())).body(r#"<D:propertyupdate xmlns:D="DAV:"><D:set><D:prop><test xmlns="urn:test">value</test></D:prop></D:set></D:propertyupdate>"#).send()?;
    assert_eq!(resp.status(), 207);
    let body = resp.text()?;
    assert!(body.contains("<D:href>/test.html</D:href>"));
    assert!(body.contains("403 Forbidden"));
    assert!(body.contains(r#"<test xmlns="urn:test"/>"#));
    Ok(())
}

#[rstest]
fn proppatch_404(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let resp = fetch!(b"PROPPATCH", format!("{}404", server.url())).send()?;
    assert_eq!(resp.status(), 404);
    Ok(())
}

#[rstest]
fn mkcol_dir(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let resp = fetch!(b"MKCOL", format!("{}newdir", server.url())).send()?;
    assert_eq!(resp.status(), 201);
    Ok(())
}

#[rstest]
fn mkcol_not_allow_upload(server: TestServer) -> Result<(), Error> {
    let resp = fetch!(b"MKCOL", format!("{}newdir", server.url())).send()?;
    assert_eq!(resp.status(), 403);
    Ok(())
}

#[rstest]
fn mkcol_already_exists(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let resp = fetch!(b"MKCOL", format!("{}dir1", server.url())).send()?;
    assert_eq!(resp.status(), 405);
    Ok(())
}

#[rstest]
fn copy_file(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let new_url = format!("{}test2.html", server.url());
    let resp = fetch!(b"COPY", format!("{}test.html", server.url()))
        .header("Destination", &new_url)
        .send()?;
    assert_eq!(resp.status(), 201);
    let resp = reqwest::blocking::get(new_url)?;
    assert_eq!(resp.status(), 200);
    Ok(())
}

#[rstest]
fn copy_not_allow_upload(server: TestServer) -> Result<(), Error> {
    let new_url = format!("{}test2.html", server.url());
    let resp = fetch!(b"COPY", format!("{}test.html", server.url()))
        .header("Destination", &new_url)
        .send()?;
    assert_eq!(resp.status(), 403);
    Ok(())
}

#[rstest]
fn copy_no_overwrite_without_delete(
    #[with(&["--allow-upload"])] server: TestServer,
) -> Result<(), Error> {
    let dest_url = format!("{}test.html", server.url());
    let resp = fetch!(b"COPY", format!("{}test.txt", server.url()))
        .header("Destination", &dest_url)
        .send()?;
    assert_eq!(resp.status(), 403);
    let resp = reqwest::blocking::get(dest_url)?;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text()?, "This is test.html");
    Ok(())
}

#[rstest]
fn copy_overwrite_false(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let dest_url = format!("{}test.html", server.url());
    let resp = fetch!(b"COPY", format!("{}test.txt", server.url()))
        .header("Destination", &dest_url)
        .header("Overwrite", "F")
        .send()?;
    assert_eq!(resp.status(), 412);
    let resp = reqwest::blocking::get(dest_url)?;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text()?, "This is test.html");
    Ok(())
}

#[rstest]
fn copy_file_404(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let new_url = format!("{}test2.html", server.url());
    let resp = fetch!(b"COPY", format!("{}404", server.url()))
        .header("Destination", &new_url)
        .send()?;
    assert_eq!(resp.status(), 404);
    Ok(())
}

#[rstest]
fn move_file(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let origin_url = format!("{}test.html", server.url());
    let new_url = format!("{}test2.html", server.url());
    let resp = fetch!(b"MOVE", &origin_url)
        .header("Destination", &new_url)
        .send()?;
    assert_eq!(resp.status(), 201);
    let resp = reqwest::blocking::get(new_url)?;
    assert_eq!(resp.status(), 200);
    let resp = reqwest::blocking::get(origin_url)?;
    assert_eq!(resp.status(), 404);
    Ok(())
}

#[rstest]
fn move_not_allow_upload(#[with(&["--allow-delete"])] server: TestServer) -> Result<(), Error> {
    let origin_url = format!("{}test.html", server.url());
    let new_url = format!("{}test2.html", server.url());
    let resp = fetch!(b"MOVE", &origin_url)
        .header("Destination", &new_url)
        .send()?;
    assert_eq!(resp.status(), 403);
    Ok(())
}

#[rstest]
fn move_not_allow_delete(#[with(&["--allow-upload"])] server: TestServer) -> Result<(), Error> {
    let origin_url = format!("{}test.html", server.url());
    let new_url = format!("{}test2.html", server.url());
    let resp = fetch!(b"MOVE", &origin_url)
        .header("Destination", &new_url)
        .send()?;
    assert_eq!(resp.status(), 403);
    Ok(())
}

#[rstest]
fn move_overwrite_false(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let dest_url = format!("{}index.html", server.url());
    let resp = fetch!(b"MOVE", format!("{}test.txt", server.url()))
        .header("Destination", &dest_url)
        .header("Overwrite", "F")
        .send()?;
    assert_eq!(resp.status(), 412);
    let resp = reqwest::blocking::get(&dest_url)?;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text()?, "This is index.html");
    let resp = reqwest::blocking::get(format!("{}test.txt", server.url()))?;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text()?, "This is test.txt");
    Ok(())
}

#[rstest]
fn move_overwrite_false_missing_destination(
    #[with(&["-A"])] server: TestServer,
) -> Result<(), Error> {
    let new_url = format!("{}test2.html", server.url());
    let resp = fetch!(b"MOVE", format!("{}test.html", server.url()))
        .header("Destination", &new_url)
        .header("Overwrite", "F")
        .send()?;
    assert_eq!(resp.status(), 201);
    let resp = reqwest::blocking::get(new_url)?;
    assert_eq!(resp.status(), 200);
    let resp = reqwest::blocking::get(format!("{}test.html", server.url()))?;
    assert_eq!(resp.status(), 404);
    Ok(())
}

#[rstest]
fn move_file_404(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let new_url = format!("{}test2.html", server.url());
    let resp = fetch!(b"MOVE", format!("{}404", server.url()))
        .header("Destination", &new_url)
        .send()?;
    assert_eq!(resp.status(), 404);
    Ok(())
}

#[rstest]
fn lock_file(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let resp = fetch!(b"LOCK", format!("{}test.html", server.url())).send()?;
    assert_eq!(resp.status(), 405);
    Ok(())
}

#[rstest]
fn lock_file_404(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let resp = fetch!(b"LOCK", format!("{}404", server.url())).send()?;
    assert_eq!(resp.status(), 405);
    Ok(())
}

#[rstest]
fn unlock_file(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let resp = fetch!(b"UNLOCK", format!("{}test.html", server.url())).send()?;
    assert_eq!(resp.status(), 405);
    Ok(())
}

#[rstest]
fn unlock_file_404(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let resp = fetch!(b"UNLOCK", format!("{}404", server.url())).send()?;
    assert_eq!(resp.status(), 405);
    Ok(())
}

#[rstest]
fn put_if_match(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let url = format!("{}test.txt", server.url());
    let etag = file_etag(&server, "test.txt")?;

    let resp = fetch!(b"PUT", &url)
        .header(IF_MATCH, "\"stale\"")
        .body("changed")
        .send()?;
    assert_eq!(resp.status(), 412);

    let resp = fetch!(b"PUT", &url)
        .header(IF_MATCH, etag.as_str())
        .body("changed")
        .send()?;
    assert_eq!(resp.status(), 204);
    let resp = reqwest::blocking::get(&url)?;
    assert_eq!(resp.text()?, "changed");
    Ok(())
}

#[rstest]
fn put_if_none_match(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let url = format!("{}test.txt", server.url());

    let resp = fetch!(b"PUT", &url)
        .header(IF_NONE_MATCH, "*")
        .body("changed")
        .send()?;
    assert_eq!(resp.status(), 412);

    let new_url = format!("{}new.txt", server.url());
    let resp = fetch!(b"PUT", &new_url)
        .header(IF_NONE_MATCH, "*")
        .body("new")
        .send()?;
    assert_eq!(resp.status(), 201);
    Ok(())
}

#[rstest]
fn put_if_unmodified_since(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let url = format!("{}test.txt", server.url());
    let last_modified = file_last_modified(&server, "test.txt")?;
    let modified_at = DateTime::parse_from_rfc2822(&last_modified)?;
    let stale = (modified_at - Duration::days(1))
        .format("%a, %d %b %Y %T GMT")
        .to_string();

    let resp = fetch!(b"PUT", &url)
        .header(IF_UNMODIFIED_SINCE, stale)
        .body("changed")
        .send()?;
    assert_eq!(resp.status(), 412);

    let resp = fetch!(b"PUT", &url)
        .header(IF_UNMODIFIED_SINCE, last_modified)
        .body("changed")
        .send()?;
    assert_eq!(resp.status(), 204);
    Ok(())
}

#[rstest]
fn delete_if_match(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let url = format!("{}test.txt", server.url());
    let etag = file_etag(&server, "test.txt")?;

    let resp = fetch!(b"DELETE", &url)
        .header(IF_MATCH, "\"stale\"")
        .send()?;
    assert_eq!(resp.status(), 412);
    let resp = reqwest::blocking::get(&url)?;
    assert_eq!(resp.status(), 200);

    let resp = fetch!(b"DELETE", &url)
        .header(IF_MATCH, etag.as_str())
        .send()?;
    assert_eq!(resp.status(), 204);
    Ok(())
}

#[rstest]
fn copy_if_match(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let url = format!("{}test.txt", server.url());
    let new_url = format!("{}test2.txt", server.url());
    let etag = file_etag(&server, "test.txt")?;

    let resp = fetch!(b"COPY", &url)
        .header("Destination", &new_url)
        .header(IF_MATCH, "\"stale\"")
        .send()?;
    assert_eq!(resp.status(), 412);
    let resp = reqwest::blocking::get(&new_url)?;
    assert_eq!(resp.status(), 404);

    let resp = fetch!(b"COPY", &url)
        .header("Destination", &new_url)
        .header(IF_MATCH, etag.as_str())
        .send()?;
    assert_eq!(resp.status(), 201);
    Ok(())
}

#[rstest]
fn move_if_match(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let url = format!("{}test.txt", server.url());
    let new_url = format!("{}test2.txt", server.url());
    let etag = file_etag(&server, "test.txt")?;

    let resp = fetch!(b"MOVE", &url)
        .header("Destination", &new_url)
        .header(IF_MATCH, "\"stale\"")
        .send()?;
    assert_eq!(resp.status(), 412);
    let resp = reqwest::blocking::get(&url)?;
    assert_eq!(resp.status(), 200);

    let resp = fetch!(b"MOVE", &url)
        .header("Destination", &new_url)
        .header(IF_MATCH, etag.as_str())
        .send()?;
    assert_eq!(resp.status(), 201);
    Ok(())
}

#[rstest]
fn mkcol_if_match_missing(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let resp = fetch!(b"MKCOL", format!("{}newdir", server.url()))
        .header(IF_MATCH, "*")
        .send()?;
    assert_eq!(resp.status(), 412);
    Ok(())
}

#[rstest]
fn mkcol_if_none_match_missing(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let resp = fetch!(b"MKCOL", format!("{}newdir", server.url()))
        .header(IF_NONE_MATCH, "*")
        .send()?;
    assert_eq!(resp.status(), 201);
    Ok(())
}

#[rstest]
fn propfind_if_none_match(server: TestServer) -> Result<(), Error> {
    let etag = file_etag(&server, "test.html")?;
    let resp = fetch!(b"PROPFIND", format!("{}test.html", server.url()))
        .header(IF_NONE_MATCH, etag.as_str())
        .send()?;
    assert_eq!(resp.status(), 412);
    Ok(())
}

#[rstest]
fn proppatch_if_match(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let resp = fetch!(b"PROPPATCH", format!("{}test.html", server.url()))
        .header(IF_MATCH, "\"stale\"")
        .send()?;
    assert_eq!(resp.status(), 412);
    Ok(())
}

#[rstest]
fn patch_if_match(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let etag = file_etag(&server, "test.txt")?;
    let resp = fetch!(b"PATCH", format!("{}test.txt", server.url()))
        .header("X-Update-Range", "bytes=0-")
        .header(IF_MATCH, etag.as_str())
        .body("changed")
        .send()?;
    assert_eq!(resp.status(), 204);
    Ok(())
}

#[rstest]
fn propfind_selected_properties(server: TestServer) -> Result<(), Error> {
    let resp = fetch!(b"PROPFIND", format!("{}test.html", server.url()))
        .header("Depth", "0")
        .body(r#"<propfind xmlns="DAV:"><optional xmlns="urn:extension"/><prop><getetag/><custom xmlns="urn:test"/></prop></propfind>"#)
        .send()?;
    assert_eq!(resp.status(), 207);
    let body = resp.text()?;
    assert!(body.contains("<D:getetag>"));
    assert!(!body.contains("<D:displayname>"));
    assert!(body.contains(r#"<custom xmlns="urn:test"/>"#));
    assert!(body.contains("404 Not Found"));
    Ok(())
}

#[rstest]
fn dav_if_etag(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let url = format!("{}test.txt", server.url());
    let tag = file_etag(&server, "test.txt")?;
    let resp = fetch!(b"PUT", &url)
        .header("If", r#"(["different"])"#)
        .body("new")
        .send()?;
    assert_eq!(resp.status(), 412);
    let resp = fetch!(b"PUT", &url)
        .header("If", format!("([{tag}])"))
        .body("new")
        .send()?;
    assert_eq!(resp.status(), 204);
    Ok(())
}

#[rstest]
fn dav_capabilities(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let url = format!("{}test.html", server.url());
    let resp = fetch!(b"OPTIONS", &url).send()?;
    assert_eq!(resp.headers().get("DAV").unwrap(), "1, 3");
    let missing = fetch!(b"OPTIONS", format!("{}missing", server.url())).send()?;
    assert_eq!(
        missing.headers().get("DAV").unwrap(),
        "1, 3, extended-mkcol"
    );
    let allow: Vec<_> = resp
        .headers()
        .get("Allow")
        .unwrap()
        .to_str()?
        .split(',')
        .map(str::trim)
        .collect();
    assert!(allow.contains(&"PROPFIND") && allow.contains(&"PROPPATCH"));
    assert!(!allow.contains(&"LOCK") && !allow.contains(&"UNLOCK"));
    for method in [b"LOCK".as_slice(), b"UNLOCK".as_slice()] {
        let resp = reqwest::blocking::Client::new()
            .request(reqwest::Method::from_bytes(method)?, &url)
            .send()?;
        assert_eq!(resp.status(), 405);
        let allow = resp.headers().get("Allow").unwrap().to_str()?;
        assert!(!allow.contains("LOCK"));
    }
    Ok(())
}

#[rstest]
fn collection_operations(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let base = server.url();
    let src = format!("{base}dir1");
    let shallow = format!("{base}shallow");
    let deep = format!("{base}deep");
    let moved = format!("{base}moved");
    let resp = fetch!(b"COPY", &src)
        .header("Destination", &shallow)
        .header("Depth", "0")
        .send()?;
    assert_eq!(resp.status(), 201);
    assert_eq!(
        reqwest::blocking::get(format!("{shallow}/test.txt"))?.status(),
        404
    );
    let resp = fetch!(b"COPY", &src).header("Destination", &deep).send()?;
    assert_eq!(resp.status(), 201);
    assert_eq!(
        reqwest::blocking::get(format!("{deep}/test.txt"))?.text()?,
        "This is dir1/test.txt"
    );
    let resp = fetch!(b"PUT", format!("{shallow}/obsolete.txt"))
        .body("old")
        .send()?;
    assert_eq!(resp.status(), 201);
    let resp = fetch!(b"COPY", &src)
        .header("Destination", &shallow)
        .header("Overwrite", "T")
        .send()?;
    assert_eq!(resp.status(), 204);
    assert_eq!(
        reqwest::blocking::get(format!("{shallow}/obsolete.txt"))?.status(),
        404
    );
    assert_eq!(
        reqwest::blocking::get(format!("{shallow}/test.txt"))?.status(),
        200
    );
    let resp = fetch!(b"MOVE", &deep)
        .header("Destination", &moved)
        .header("Depth", "0")
        .send()?;
    assert_eq!(resp.status(), 400);
    let resp = fetch!(b"MOVE", &deep)
        .header("Destination", &moved)
        .send()?;
    assert_eq!(resp.status(), 201);
    assert_eq!(
        reqwest::blocking::get(format!("{moved}/test.txt"))?.status(),
        200
    );
    assert_eq!(
        reqwest::blocking::get(format!("{deep}/test.txt"))?.status(),
        404
    );
    let resp = fetch!(b"MOVE", &moved)
        .header("Destination", &shallow)
        .send()?;
    assert_eq!(resp.status(), 204);
    let resp = fetch!(b"DELETE", &shallow).header("Depth", "0").send()?;
    assert_eq!(resp.status(), 400);
    let resp = fetch!(b"DELETE", &shallow).send()?;
    assert_eq!(resp.status(), 204);
    assert_eq!(
        reqwest::blocking::get(format!("{shallow}/test.txt"))?.status(),
        404
    );
    Ok(())
}

#[rstest]
fn dav_tagged_if(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let url = format!("{}test.txt", server.url());
    let dest = format!("{}test.html", server.url());
    let tag = file_etag(&server, "test.html")?;
    let resp = fetch!(b"COPY", &url)
        .header("Destination", &dest)
        .header("If", format!(r#"<{dest}> (["stale"])"#))
        .send()?;
    assert_eq!(resp.status(), 412);
    let resp = fetch!(b"COPY", &url)
        .header("Destination", &dest)
        .header("If", format!(r#"<{dest}> ([{tag}])"#))
        .send()?;
    assert_eq!(resp.status(), 204);
    Ok(())
}

#[rstest]
fn propname_and_getcontenttype(server: TestServer) -> Result<(), Error> {
    let url = format!("{}test.html", server.url());
    let resp = fetch!(b"PROPFIND", &url)
        .header("Depth", "0")
        .body(r#"<propfind xmlns="DAV:"><propname/></propfind>"#)
        .send()?;
    let body = resp.text()?;
    assert!(body.contains("<D:getcontenttype/>"));
    assert!(body.contains("<D:getetag/>"));
    let resp = fetch!(b"HEAD", &url).send()?;
    let content_type = resp
        .headers()
        .get("Content-Type")
        .unwrap()
        .to_str()?
        .to_owned();
    let resp = fetch!(b"PROPFIND", &url)
        .header("Depth", "0")
        .body(r#"<propfind xmlns="DAV:"><prop><getcontenttype/></prop></propfind>"#)
        .send()?;
    assert!(resp.text()?.contains(&format!(
        "<D:getcontenttype>{content_type}</D:getcontenttype>"
    )));
    Ok(())
}

#[rstest]
fn missing_depth_and_parent(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let resp = fetch!(b"PROPFIND", server.url()).send()?;
    assert_eq!(resp.status(), 403);
    assert!(resp.text()?.contains("propfind-finite-depth"));
    let resp = fetch!(b"MKCOL", format!("{}missing/child", server.url())).send()?;
    assert_eq!(resp.status(), 409);
    let resp = fetch!(b"MKCOL", format!("{}newdir", server.url()))
        .body("unsupported")
        .send()?;
    assert_eq!(resp.status(), 415);
    Ok(())
}

#[rstest]
fn proppatch_transaction(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let url = format!("{}dir1/", server.url());
    let resp = fetch!(b"PROPPATCH", &url).body(r#"<propertyupdate xmlns="DAV:"><remove><prop><absent xmlns="urn:test"/></prop></remove><set><prop><resourcetype/></prop></set></propertyupdate>"#).send()?;
    assert_eq!(resp.status(), 207);
    let body = resp.text()?;
    assert!(body.contains("403 Forbidden"));
    assert!(body.contains("cannot-modify-protected-property"));
    assert!(body.contains("424 Failed Dependency"));
    let resp = fetch!(b"PROPPATCH", &url).body(r#"<propertyupdate xmlns="DAV:"><remove><prop><absent xmlns="urn:test"/></prop></remove></propertyupdate>"#).send()?;
    assert_eq!(resp.status(), 207);
    assert!(resp.text()?.contains("200 OK"));
    Ok(())
}

#[cfg(unix)]
#[rstest]
fn copy_member_failure_preserves_destination(
    #[with(&["-A"])] server: TestServer,
) -> Result<(), Error> {
    std::os::unix::fs::symlink("test.txt", server.path().join("dir1/link.txt"))?;
    let dest = format!("{}dir2", server.url());
    let resp = fetch!(b"COPY", format!("{}dir1", server.url()))
        .header("Destination", &dest)
        .send()?;
    assert_eq!(resp.status(), 207);
    let body = resp.text()?;
    assert!(body.contains("/dir1/link.txt"));
    assert!(body.contains("403 Forbidden"));
    assert!(body.contains("424 Failed Dependency"));
    assert_eq!(
        reqwest::blocking::get(format!("{dest}/test.txt"))?.text()?,
        "This is dir2/test.txt"
    );
    Ok(())
}

#[rstest]
fn unsupported_collection_body(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let url = format!("{}dir1", server.url());
    let dest = format!("{}destination", server.url());
    for method in ["COPY", "MOVE", "DELETE"] {
        let resp = reqwest::blocking::Client::new()
            .request(method.parse()?, &url)
            .header("Destination", &dest)
            .body("unsupported")
            .send()?;
        assert_eq!(resp.status(), 415);
    }
    assert_eq!(reqwest::blocking::get(&url)?.status(), 200);
    assert!(!server.path().join("destination").exists());
    Ok(())
}
