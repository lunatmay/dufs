mod fixtures;
mod utils;

use fixtures::{server, Error, TestServer};
use reqwest::blocking::Response;
use rstest::rstest;
use xml::escape::escape_str_pcdata;

fn report_body(token: &str, level: &str, limit: Option<usize>) -> String {
    let limit = limit
        .map(|count| format!("<D:limit><D:nresults>{count}</D:nresults></D:limit>"))
        .unwrap_or_default();
    format!(
        r#"<D:sync-collection xmlns:D="DAV:"><D:sync-token>{}</D:sync-token><D:sync-level>{level}</D:sync-level>{limit}<D:prop><D:getetag/><D:resourcetype/></D:prop></D:sync-collection>"#,
        escape_str_pcdata(token)
    )
}

fn report(url: &str, token: &str) -> Result<Response, Error> {
    Ok(fetch!(b"REPORT", url)
        .header("Depth", "0")
        .header("Content-Type", "application/xml")
        .body(report_body(token, "1", None))
        .send()?)
}

fn extract_token(body: &str) -> &str {
    body.split("<D:sync-token>")
        .nth(1)
        .and_then(|value| value.split("</D:sync-token>").next())
        .filter(|value| value.starts_with("urn:"))
        .expect("Missing synchronization token")
}

#[rstest]
fn initial_and_incremental(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let url = format!("{}dir1/", server.url());
    let initial = report(&url, "")?;
    assert_eq!(initial.status(), 207);
    let initial = initial.text()?;
    let token = extract_token(&initial).to_owned();
    assert!(initial.contains("<D:href>/dir1/test.txt</D:href>"));
    assert!(initial.contains("<D:getetag>"));
    assert!(!initial.contains("<D:href>/dir1/</D:href>"));
    assert!(!initial.contains("404 Not Found"));

    let unchanged = report(&url, &token)?.text()?;
    assert_eq!(extract_token(&unchanged), token);
    assert!(!unchanged.contains("<D:response>"));

    assert_eq!(
        fetch!(b"PUT", format!("{url}test.txt"))
            .body("changed")
            .send()?
            .status(),
        204
    );
    assert_eq!(
        fetch!(b"PUT", format!("{url}new.txt"))
            .body("new")
            .send()?
            .status(),
        201
    );
    assert_eq!(
        fetch!(b"DELETE", format!("{url}test.html"))
            .send()?
            .status(),
        204
    );
    assert_eq!(
        fetch!(b"MKCOL", format!("{url}child")).send()?.status(),
        201
    );

    let incremental = report(&url, &token)?;
    assert_eq!(incremental.status(), 207);
    let incremental = incremental.text()?;
    let next = extract_token(&incremental).to_owned();
    assert_ne!(next, token);
    for href in ["/dir1/test.txt", "/dir1/new.txt", "/dir1/child/"] {
        assert!(incremental.contains(&format!("<D:href>{href}</D:href>")));
    }
    assert!(incremental
        .contains("<D:href>/dir1/test.html</D:href><D:status>HTTP/1.1 404 Not Found</D:status>"));
    assert!(!incremental.contains("<D:href>/dir1/index.html</D:href>"));

    let unchanged = report(&url, &next)?.text()?;
    assert_eq!(extract_token(&unchanged), next);
    assert!(!unchanged.contains("<D:response>"));
    Ok(())
}

#[rstest]
fn transient_member_has_tombstone(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let url = format!("{}dir1/", server.url());
    let initial = report(&url, "")?.text()?;
    let token = extract_token(&initial);
    let member = format!("{url}transient.txt");
    assert_eq!(fetch!(b"PUT", &member).body("new").send()?.status(), 201);
    assert_eq!(fetch!(b"DELETE", &member).send()?.status(), 204);
    let next = report(&url, token)?;
    assert_eq!(next.status(), 207);
    let body = next.text()?;
    assert!(body.contains(
        "<D:href>/dir1/transient.txt</D:href><D:status>HTTP/1.1 404 Not Found</D:status>"
    ));
    assert_ne!(extract_token(&body), token);
    Ok(())
}

#[rstest]
fn live_properties_and_if(#[with(&["-A"])] server: TestServer) -> Result<(), Error> {
    let url = format!("{}dir1/", server.url());
    let property = fetch!(b"PROPFIND", &url)
        .header("Depth", "0")
        .body(r#"<D:propfind xmlns:D="DAV:"><D:prop><D:sync-token/><D:supported-report-set/></D:prop></D:propfind>"#)
        .send()?;
    assert_eq!(property.status(), 207);
    let property = property.text()?;
    let token = extract_token(&property);
    assert!(property.contains("sync-collection"));

    let unchanged = report(&url, token)?.text()?;
    assert_eq!(extract_token(&unchanged), token);
    assert!(!unchanged.contains("<D:response>"));

    let condition = format!("</dir1/> (<{token}>)");
    assert_eq!(
        fetch!(b"PUT", format!("{url}new.txt"))
            .header("If", &condition)
            .body("new")
            .send()?
            .status(),
        201
    );
    assert_eq!(
        fetch!(b"MKCOL", format!("{url}stale"))
            .header("If", &condition)
            .send()?
            .status(),
        412
    );
    Ok(())
}

#[rstest]
fn invalid_tokens_and_limits(server: TestServer) -> Result<(), Error> {
    let url = format!("{}dir1/", server.url());
    let initial = report(&url, "")?.text()?;
    let token = extract_token(&initial);
    let invalid = report(&format!("{}dir2/", server.url()), token)?;
    assert_eq!(invalid.status(), 403);
    assert!(invalid.text()?.contains("valid-sync-token"));
    let invalid = report(&url, "urn:uuid:00000000-0000-0000-0000-000000000000")?;
    assert_eq!(invalid.status(), 403);
    assert!(invalid.text()?.contains("valid-sync-token"));

    let limited = fetch!(b"REPORT", &url)
        .header("Depth", "0")
        .body(report_body("", "1", Some(1)))
        .send()?;
    assert_eq!(limited.status(), 507);
    assert!(limited.text()?.contains("number-of-matches-within-limits"));
    let allowed = fetch!(b"REPORT", &url)
        .header("Depth", "0")
        .body(report_body("", "1", Some(1000)))
        .send()?;
    assert_eq!(allowed.status(), 207);

    let wrong_depth = fetch!(b"REPORT", &url)
        .header("Depth", "1")
        .body(report_body("", "1", None))
        .send()?;
    assert_eq!(wrong_depth.status(), 400);
    let infinite = fetch!(b"REPORT", &url)
        .header("Depth", "0")
        .body(report_body("", "infinite", None))
        .send()?;
    assert_eq!(infinite.status(), 403);
    assert!(infinite.text()?.contains("sync-traversal-supported"));
    Ok(())
}

#[rstest]
fn external_changes(server: TestServer) -> Result<(), Error> {
    let url = format!("{}dir1/", server.url());
    let initial = report(&url, "")?.text()?;
    let token = extract_token(&initial);
    std::fs::write(server.path().join("dir1/test.txt"), "externally modified")?;
    let modified = report(&url, token)?;
    assert_eq!(modified.status(), 207);
    let modified = modified.text()?;
    assert!(modified.contains("<D:href>/dir1/test.txt</D:href>"));
    let next = extract_token(&modified);
    // An external directory membership edit has no DAV mutation observation.
    // The old token must not falsely claim complete historical knowledge.
    std::fs::write(server.path().join("dir1/external.txt"), "new")?;
    let invalidated = report(&url, next)?;
    assert_eq!(invalidated.status(), 403);
    assert!(invalidated.text()?.contains("valid-sync-token"));
    let fresh = report(&url, "")?.text()?;
    assert!(fresh.contains("<D:href>/dir1/external.txt</D:href>"));
    Ok(())
}

#[rstest]
fn principal_scoped_tokens(
    #[with(&["-a", "alice:alice@/:rw", "-a", "bob:bob@/:rw", "-A"])] server: TestServer,
) -> Result<(), Error> {
    let url = format!("{}dir1/", server.url());
    let initial = fetch!(b"REPORT", &url)
        .basic_auth("alice", Some("alice"))
        .header("Depth", "0")
        .body(report_body("", "1", None))
        .send()?;
    assert_eq!(initial.status(), 207);
    let initial = initial.text()?;
    let token = extract_token(&initial);
    let other = fetch!(b"REPORT", &url)
        .basic_auth("bob", Some("bob"))
        .header("Depth", "0")
        .body(report_body(token, "1", None))
        .send()?;
    assert_eq!(other.status(), 403);
    assert!(other.text()?.contains("valid-sync-token"));
    Ok(())
}
