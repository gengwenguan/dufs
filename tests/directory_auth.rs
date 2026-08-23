mod fixtures;
mod utils;

use fixtures::{server, Error, TestServer};
use rstest::rstest;
use serde_json::Value;

fn directory_password(json: &Value, name: &str) -> String {
    json["paths"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["name"] == name)
        .and_then(|item| item["directory_password"].as_str())
        .unwrap()
        .to_string()
}

#[rstest]
fn directory_password_isolates_paths(
    #[with(&["--directory-auth", "--auth", "admin:pass@/:rw", "-A"])] server: TestServer,
) -> Result<(), Error> {
    let resp = reqwest::blocking::get(server.url())?;
    assert_eq!(resp.status(), 200);
    let json = utils::retrieve_json(&resp.text()?).unwrap();
    assert!(json["paths"].as_array().unwrap().is_empty());

    let resp = fetch!(b"GET", server.url())
        .basic_auth("admin", Some("pass"))
        .send()?;
    assert_eq!(resp.status(), 200);
    let json = utils::retrieve_json(&resp.text()?).unwrap();
    let dir1_password = directory_password(&json, "dir1");
    let dir2_password = directory_password(&json, "dir2");
    assert_eq!(dir1_password.len(), 32);
    assert_ne!(dir1_password, dir2_password);

    let dir1_url = format!("{}dir1/", server.url());
    let resp = fetch!(b"GET", &dir1_url).send()?;
    assert_eq!(resp.status(), 401);
    let resp = fetch!(b"GET", format!("{dir1_url}?dir_password={dir2_password}")).send()?;
    assert_eq!(resp.status(), 401);
    let resp = fetch!(b"GET", format!("{dir1_url}?dir_password={dir1_password}")).send()?;
    assert_eq!(resp.status(), 200);

    let file_url = format!("{}dir1/index.html", server.url());
    let resp = fetch!(b"GET", format!("{file_url}?dir_password={dir2_password}")).send()?;
    assert_eq!(resp.status(), 401);
    let resp = fetch!(b"GET", format!("{file_url}?dir_password={dir1_password}")).send()?;
    assert_eq!(resp.status(), 200);
    Ok(())
}

#[rstest]
fn new_directory_gets_independent_password(
    #[with(&["--directory-auth", "--auth", "admin:pass@/:rw", "-A"])] server: TestServer,
) -> Result<(), Error> {
    let parent_url = format!("{}shared", server.url());
    let resp = fetch!(b"MKCOL", &parent_url)
        .basic_auth("admin", Some("pass"))
        .send()?;
    assert_eq!(resp.status(), 201);
    let parent_password = resp
        .headers()
        .get("x-dufs-directory-password")
        .unwrap()
        .to_str()?
        .to_string();

    let child_url = format!("{parent_url}/child");
    let resp = fetch!(b"MKCOL", &child_url)
        .basic_auth("admin", Some("pass"))
        .send()?;
    assert_eq!(resp.status(), 201);
    let child_password = resp
        .headers()
        .get("x-dufs-directory-password")
        .unwrap()
        .to_str()?
        .to_string();
    assert_ne!(parent_password, child_password);

    let resp = fetch!(
        b"GET",
        format!("{child_url}/?dir_password={parent_password}")
    )
    .send()?;
    assert_eq!(resp.status(), 401);
    let resp = fetch!(
        b"GET",
        format!("{child_url}/?dir_password={child_password}")
    )
    .send()?;
    assert_eq!(resp.status(), 200);

    let renamed_url = format!("{parent_url}/renamed");
    let resp = fetch!(b"MOVE", &child_url)
        .basic_auth("admin", Some("pass"))
        .header("Destination", &renamed_url)
        .send()?;
    assert_eq!(resp.status(), 204);
    let resp = fetch!(
        b"GET",
        format!("{renamed_url}/?dir_password={child_password}")
    )
    .send()?;
    assert_eq!(resp.status(), 200);
    Ok(())
}

#[rstest]
fn directory_password_grants_readonly_access(
    #[with(&["--directory-auth", "--auth", "admin:pass@/:rw", "-A"])] server: TestServer,
) -> Result<(), Error> {
    let resp = fetch!(b"GET", server.url())
        .basic_auth("admin", Some("pass"))
        .send()?;
    let json = utils::retrieve_json(&resp.text()?).unwrap();
    let password = directory_password(&json, "dir1");
    let directory_url = format!("{}dir1/", server.url());
    let share = |path: &str| {
        let separator = if path.contains('?') { '&' } else { '?' };
        format!("{directory_url}{path}{separator}dir_password={password}")
    };

    let resp = fetch!(
        b"GET",
        format!("{directory_url}?json&dir_password={password}")
    )
    .send()?;
    assert_eq!(resp.status(), 200);
    let json: Value = serde_json::from_str(&resp.text()?)?;
    assert_eq!(json["allow_upload"], false);
    assert_eq!(json["allow_delete"], false);

    let resp = fetch!(b"GET", share("index.html")).send()?;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text()?, "This is dir1/index.html");

    let resp = fetch!(b"GET", share("index.html?edit")).send()?;
    assert_eq!(resp.status(), 200);
    let json = utils::retrieve_json(&resp.text()?).unwrap();
    assert_eq!(json["kind"], "View");
    assert_eq!(json["allow_upload"], false);
    assert_eq!(json["allow_delete"], false);

    for method in [
        b"PUT".as_slice(),
        b"PATCH".as_slice(),
        b"DELETE".as_slice(),
        b"MKCOL".as_slice(),
        b"COPY".as_slice(),
        b"MOVE".as_slice(),
        b"PROPPATCH".as_slice(),
        b"LOCK".as_slice(),
        b"UNLOCK".as_slice(),
        b"SETPASSWORD".as_slice(),
    ] {
        let resp = reqwest::blocking::Client::new()
            .request(reqwest::Method::from_bytes(method)?, share("index.html"))
            .header("Destination", share("renamed.html"))
            .body("changed")
            .send()?;
        assert_eq!(
            resp.status(),
            403,
            "{} unexpectedly allowed",
            String::from_utf8_lossy(method)
        );
    }

    let resp = fetch!(b"GET", share("index.html?tokengen")).send()?;
    assert_eq!(resp.status(), 403);
    let resp = fetch!(
        b"PROPFIND",
        format!("{directory_url}?dir_password={password}")
    )
    .send()?;
    assert_eq!(resp.status(), 207);
    Ok(())
}

#[rstest]
fn directory_password_cookie_loads_static_assets(
    #[with(&["--directory-auth", "--auth", "admin:pass@/:rw", "-A"])] server: TestServer,
) -> Result<(), Error> {
    std::fs::create_dir_all(server.path().join("dir1/assets"))?;
    std::fs::write(
        server.path().join("dir1/assets/site.css"),
        "body { color: blue; }",
    )?;

    let resp = fetch!(b"GET", server.url())
        .basic_auth("admin", Some("pass"))
        .send()?;
    let json = utils::retrieve_json(&resp.text()?).unwrap();
    let password = directory_password(&json, "dir1");

    let resp = fetch!(
        b"GET",
        format!("{}dir1/index.html?dir_password={password}", server.url())
    )
    .send()?;
    assert_eq!(resp.status(), 200);
    let cookie = resp
        .headers()
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .unwrap();
    assert!(cookie.starts_with("dufs_dir_password="));
    assert!(cookie.contains("Path=/dir1/"));
    assert!(cookie.contains("HttpOnly"));
    assert!(cookie.contains("SameSite=Lax"));

    let cookie = cookie.split(';').next().unwrap();
    let resp = fetch!(b"GET", format!("{}dir1/assets/site.css", server.url()))
        .header("Cookie", cookie)
        .send()?;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text()?, "body { color: blue; }");

    let resp = fetch!(b"GET", format!("{}dir2/test.txt", server.url()))
        .header("Cookie", cookie)
        .send()?;
    assert_eq!(resp.status(), 401);
    Ok(())
}

#[rstest]
fn only_account_can_change_directory_password(
    #[with(&["--directory-auth", "--auth", "admin:pass@/:rw", "-A"])] server: TestServer,
) -> Result<(), Error> {
    let directory_url = format!("{}shared", server.url());
    let resp = fetch!(b"MKCOL", &directory_url)
        .basic_auth("admin", Some("pass"))
        .send()?;
    let old_password = resp
        .headers()
        .get("x-dufs-directory-password")
        .unwrap()
        .to_str()?
        .to_string();

    let resp = fetch!(
        b"SETPASSWORD",
        format!("{directory_url}?dir_password={old_password}")
    )
    .body("new-password")
    .send()?;
    assert_eq!(resp.status(), 403);

    let resp = fetch!(b"SETPASSWORD", &directory_url)
        .basic_auth("admin", Some("pass"))
        .body("new-password")
        .send()?;
    assert_eq!(resp.status(), 204);

    let resp = fetch!(
        b"GET",
        format!("{directory_url}/?dir_password={old_password}")
    )
    .send()?;
    assert_eq!(resp.status(), 401);
    let resp = fetch!(
        b"GET",
        format!("{directory_url}/?dir_password=new-password")
    )
    .send()?;
    assert_eq!(resp.status(), 200);
    Ok(())
}

#[rstest]
fn directory_auth_file_is_not_served(
    #[with(&["--directory-auth", "--auth", "admin:pass@/:rw", "-A"])] server: TestServer,
) -> Result<(), Error> {
    let resp = fetch!(b"GET", format!("{}dir1/", server.url()))
        .basic_auth("admin", Some("pass"))
        .send()?;
    assert_eq!(resp.status(), 200);
    assert!(server.path().join(".dufs-directory-auth.json").exists());

    let resp = fetch!(b"GET", format!("{}.dufs-directory-auth.json", server.url()))
        .basic_auth("admin", Some("pass"))
        .send()?;
    assert_eq!(resp.status(), 404);
    Ok(())
}

#[cfg(unix)]
#[rstest]
fn directory_auth_file_symlink_is_not_served(
    #[with(&[
        "--directory-auth",
        "--auth",
        "admin:pass@/:rw",
        "--allow-search",
        "--allow-symlink"
    ])]
    server: TestServer,
) -> Result<(), Error> {
    let resp = fetch!(b"GET", format!("{}dir1/", server.url()))
        .basic_auth("admin", Some("pass"))
        .send()?;
    assert_eq!(resp.status(), 200);

    std::os::unix::fs::symlink(
        server.path().join(".dufs-directory-auth.json"),
        server.path().join("password-leak"),
    )?;
    let resp = fetch!(b"GET", format!("{}password-leak", server.url()))
        .basic_auth("admin", Some("pass"))
        .send()?;
    assert_eq!(resp.status(), 404);

    let resp = fetch!(b"GET", format!("{}?q=password-leak", server.url()))
        .basic_auth("admin", Some("pass"))
        .send()?;
    let paths = utils::retrieve_index_paths(&resp.text()?);
    assert!(!paths.contains("password-leak"));
    Ok(())
}
