mod fixtures;
mod utils;

use assert_fs::fixture::TempDir;
use fixtures::{port, server, tmpdir, wait_for_port, Error, TestServer, DIR_ASSETS};
use rstest::rstest;
use std::process::{Command, Stdio};

fn extract_assets_prefix(content: &str) -> &str {
    let start = content.find("__dufs_v").unwrap();
    let rest = &content[start..];
    let end = rest.find("__/").unwrap() + 3;
    &rest[..end]
}

#[rstest]
fn assets(server: TestServer) -> Result<(), Error> {
    let resp = reqwest::blocking::get(server.url())?;
    let text = resp.text()?;
    let prefix = extract_assets_prefix(&text);
    let index_js = format!("/{prefix}index.js");
    let index_css = format!("/{prefix}index.css");
    let favicon_ico = format!("/{prefix}favicon.ico");
    assert!(text.contains(&format!(r#"href="{index_css}""#)));
    assert!(text.contains(&format!(r#"href="{favicon_ico}""#)));
    assert!(text.contains(&format!(r#"src="{index_js}""#)));
    Ok(())
}

#[rstest]
fn asset_js(server: TestServer) -> Result<(), Error> {
    let text = reqwest::blocking::get(server.url())?.text()?;
    let prefix = extract_assets_prefix(&text);
    let url = format!("{}{prefix}index.js", server.url());
    let resp = reqwest::blocking::get(url)?;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/javascript; charset=UTF-8"
    );
    let text = resp.text()?;
    assert!(text.contains(r#"const filePageUrl = isDir ? url : withQuery(url, "edit");"#));
    Ok(())
}

#[rstest]
fn asset_css(server: TestServer) -> Result<(), Error> {
    let text = reqwest::blocking::get(server.url())?.text()?;
    let prefix = extract_assets_prefix(&text);
    let url = format!("{}{prefix}index.css", server.url());
    let resp = reqwest::blocking::get(url)?;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/css; charset=UTF-8"
    );
    Ok(())
}

#[rstest]
fn asset_ico(server: TestServer) -> Result<(), Error> {
    let text = reqwest::blocking::get(server.url())?.text()?;
    let prefix = extract_assets_prefix(&text);
    let url = format!("{}{prefix}favicon.ico", server.url());
    let resp = reqwest::blocking::get(url)?;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("content-type").unwrap(), "image/x-icon");
    Ok(())
}

#[rstest]
fn assets_with_prefix(#[with(&["--path-prefix", "xyz"])] server: TestServer) -> Result<(), Error> {
    let resp = reqwest::blocking::get(format!("{}xyz/", server.url()))?;
    let text = resp.text()?;
    let prefix = extract_assets_prefix(&text);
    let index_js = format!("/xyz/{prefix}index.js");
    let index_css = format!("/xyz/{prefix}index.css");
    let favicon_ico = format!("/xyz/{prefix}favicon.ico");
    assert!(text.contains(&format!(r#"href="{index_css}""#)));
    assert!(text.contains(&format!(r#"href="{favicon_ico}""#)));
    assert!(text.contains(&format!(r#"src="{index_js}""#)));
    Ok(())
}

#[rstest]
fn asset_js_with_prefix(
    #[with(&["--path-prefix", "xyz"])] server: TestServer,
) -> Result<(), Error> {
    let base_url = format!("{}xyz/", server.url());
    let text = reqwest::blocking::get(&base_url)?.text()?;
    let prefix = extract_assets_prefix(&text);
    let url = format!("{base_url}{prefix}index.js");
    let resp = reqwest::blocking::get(url)?;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/javascript; charset=UTF-8"
    );
    Ok(())
}

#[rstest]
fn assets_override(tmpdir: TempDir, port: u16) -> Result<(), Error> {
    let mut child = Command::new(assert_cmd::cargo::cargo_bin!())
        .arg(tmpdir.path())
        .arg("-p")
        .arg(port.to_string())
        .arg("--assets")
        .arg(tmpdir.join(DIR_ASSETS))
        .stdout(Stdio::piped())
        .spawn()?;

    wait_for_port(port);

    let url = format!("http://localhost:{port}");
    let resp = reqwest::blocking::get(&url)?;
    let text = resp.text()?;
    let prefix = extract_assets_prefix(&text);
    assert!(text.starts_with(&format!("/{prefix}index.js;<template id=\"index-data\">")));
    let resp = reqwest::blocking::get(&url)?;
    assert_resp_paths!(resp);

    child.kill()?;
    Ok(())
}

#[rstest]
fn assets_override_not_found_page(tmpdir: TempDir, port: u16) -> Result<(), Error> {
    let not_found_html = "<html><body>custom 404 page</body></html>";
    std::fs::write(tmpdir.join(format!("{DIR_ASSETS}404.html")), not_found_html)?;

    let mut child = Command::new(assert_cmd::cargo::cargo_bin!())
        .arg(tmpdir.path())
        .arg("-p")
        .arg(port.to_string())
        .arg("--assets")
        .arg(tmpdir.join(DIR_ASSETS))
        .stdout(Stdio::piped())
        .spawn()?;

    wait_for_port(port);

    let url = format!("http://localhost:{port}/missing-path");
    let resp = reqwest::blocking::get(&url)?;
    assert_eq!(resp.status(), 404);
    assert_eq!(resp.text()?, not_found_html);

    let url = format!("http://localhost:{port}/missing-path?noscript");
    let resp = reqwest::blocking::get(&url)?;
    assert_eq!(resp.status(), 404);
    assert_eq!(resp.text()?, "Not Found");

    child.kill()?;
    Ok(())
}
