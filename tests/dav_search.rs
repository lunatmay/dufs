mod fixtures;
mod utils;

use fixtures::{server, Error, TestServer};
use rstest::rstest;

fn query(scope: &str, depth: &str, extra: &str) -> String {
    format!(
        r#"<searchrequest xmlns="DAV:"><basicsearch><select><prop><displayname/><getcontentlength/><unknown xmlns="urn:test"/></prop></select><from><scope><href>{scope}</href><depth>{depth}</depth></scope></from>{extra}</basicsearch></searchrequest>"#
    )
}

#[rstest]
fn search_discovery(#[with(&["--allow-search"])] server: TestServer) -> Result<(), Error> {
    let response = fetch!(b"OPTIONS", server.url()).send()?;
    assert_eq!(response.headers()["dasl"], "<DAV:basicsearch>");
    assert!(response.headers()["allow"].to_str()?.contains("SEARCH"));
    Ok(())
}

#[rstest]
fn search_disabled(server: TestServer) -> Result<(), Error> {
    let response = fetch!(b"OPTIONS", server.url()).send()?;
    assert!(!response.headers().contains_key("dasl"));
    assert!(!response.headers()["allow"].to_str()?.contains("SEARCH"));
    Ok(())
}

#[rstest]
fn search_numeric_and_limit(#[with(&["--allow-search"])] server: TestServer) -> Result<(), Error> {
    let response = fetch!(b"SEARCH", server.url())
        .header("content-type", "application/xml")
        .body(query("/dir1/", "1", r#"<where><and><not><is-collection/></not><gte><prop><getcontentlength/></prop><literal>9</literal></gte><like><prop><displayname/></prop><literal>%.html</literal></like></and></where><orderby><order><prop><getcontentlength/></prop><descending/></order></orderby><limit><nresults>1</nresults></limit>"#))
        .send()?;
    assert_eq!(response.status(), 207);
    let body = response.text()?;
    assert!(body.contains("<D:href>/dir1/index.html</D:href>"), "{body}");
    assert!(!body.contains("<D:href>/dir1/test.html</D:href>"));
    assert!(body.contains("507 Insufficient Storage"));
    assert!(body.contains("404 Not Found"));
    Ok(())
}

#[rstest]
fn search_three_valued_logic(#[with(&["--allow-search"])] server: TestServer) -> Result<(), Error> {
    let response = fetch!(b"SEARCH", server.url())
        .body(query("/dir1/", "infinity", r#"<where><not><eq><prop><unknown xmlns="urn:test"/></prop><literal>x</literal></eq></not></where>"#))
        .send()?;
    assert_eq!(response.status(), 207);
    assert!(!response.text()?.contains("<D:response>"));
    let response = fetch!(b"SEARCH", server.url())
        .body(query("/dir1/", "0", r#"<where><and><is-collection/><not><is-defined><prop><unknown xmlns="urn:test"/></prop></is-defined></not></and></where>"#))
        .send()?;
    assert_eq!(response.status(), 207);
    assert!(response.text()?.contains("<D:href>/dir1/</D:href>"));
    Ok(())
}

#[rstest]
fn search_optional_operator(#[with(&["--allow-search"])] server: TestServer) -> Result<(), Error> {
    let response = fetch!(b"SEARCH", server.url())
        .body(query(
            "/dir1/",
            "1",
            "<where><contains>example</contains></where>",
        ))
        .send()?;
    assert_eq!(response.status(), 422);
    Ok(())
}

#[rstest]
fn search_invalid_scope(#[with(&["--allow-search"])] server: TestServer) -> Result<(), Error> {
    let response = fetch!(b"SEARCH", server.url())
        .body(query("/dir-no-found/", "1", ""))
        .send()?;
    assert_eq!(response.status(), 409);
    assert!(response.text()?.contains("search-scope-valid"));
    Ok(())
}

#[rstest]
fn search_relative_scope(#[with(&["--allow-search"])] server: TestServer) -> Result<(), Error> {
    let response = fetch!(b"SEARCH", server.url().join("dir1/")?)
        .body(query("test.txt", "0", ""))
        .send()?;
    assert_eq!(response.status(), 207);
    assert!(response.text()?.contains("<D:href>/dir1/test.txt</D:href>"));
    Ok(())
}

#[rstest]
fn search_readonly_user(
    #[with(&["--allow-search", "--auth", "user:pass@/dir1:ro"])] server: TestServer,
) -> Result<(), Error> {
    let response = fetch!(b"SEARCH", server.url().join("dir1/")?)
        .basic_auth("user", Some("pass"))
        .body(query("/dir1/", "1", ""))
        .send()?;
    assert_eq!(response.status(), 207);
    assert!(response.text()?.contains("<D:href>/dir1/test.txt</D:href>"));
    Ok(())
}
