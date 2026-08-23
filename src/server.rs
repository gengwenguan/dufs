#![allow(clippy::too_many_arguments)]

use crate::auth::{www_authenticate, AccessPaths, AccessPerm};
use crate::directory_auth::{password_matches, DirectoryAuth, DIRECTORY_PASSWORD_QUERY};
use crate::http_utils::{body_full, IncomingStream, LengthLimitedStream};
use crate::noscript::{detect_noscript, generate_noscript_html};
use crate::utils::{
    decode_uri, encode_uri, get_file_name, glob, parse_range, try_get_file_name, unix_now,
};
use crate::Args;

use anyhow::{anyhow, Result};
use async_deflate_zip::{Compression, WriterOptions, ZipWriter};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
use bytes::Bytes;
use chrono::{LocalResult, TimeZone, Utc};
use futures_util::{pin_mut, TryStreamExt};
use headers::{
    AcceptRanges, AccessControlAllowCredentials, AccessControlAllowOrigin, CacheControl,
    ContentLength, ContentType, ETag, HeaderMap, HeaderMapExt, IfMatch, IfModifiedSince,
    IfNoneMatch, IfRange, IfUnmodifiedSince, LastModified, Range,
};
use http_body_util::{combinators::BoxBody, BodyExt, Limited, StreamBody};
use hyper::body::Frame;
use hyper::{
    body::Incoming,
    header::{
        HeaderValue, AUTHORIZATION, CONNECTION, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE,
        CONTENT_TYPE, COOKIE, RANGE, SET_COOKIE,
    },
    Method, StatusCode, Uri,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs::Metadata;
use std::io::SeekFrom;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf, MAIN_SEPARATOR};
use std::sync::atomic::{self, AtomicBool};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWrite};
use tokio::sync::{broadcast, RwLock};
use tokio::{fs, io};

use tokio_util::io::{ReaderStream, StreamReader};
use uuid::Uuid;
use walkdir::{DirEntry, WalkDir};
use xml::escape::escape_str_pcdata;

pub type Request = hyper::Request<Incoming>;
pub type Response = hyper::Response<BoxBody<Bytes, anyhow::Error>>;

const INDEX_HTML: &str = include_str!("../assets/index.html");
const INDEX_CSS: &str = include_str!("../assets/index.css");
const INDEX_JS: &str = include_str!("../assets/index.js");
const FAVICON_ICO: &[u8] = include_bytes!("../assets/favicon.ico");
const INDEX_NAME: &str = "index.html";
const BUF_SIZE: usize = 65536;
const EDITABLE_TEXT_MAX_SIZE: u64 = 4194304; // 4M
const RESUMABLE_UPLOAD_MIN_SIZE: u64 = 20971520; // 20M
const HEALTH_CHECK_PATH: &str = "__dufs__/health";
const CLIPBOARD_PATH: &str = "__dufs__/clipboard";
const CLIPBOARD_EVENTS_PATH: &str = "__dufs__/clipboard/events";
const CLIPBOARD_MAX_SIZE: u64 = 65536; // 64K
const DIRECTORY_PASSWORD_COOKIE: &str = "dufs_dir_password";
pub const MAX_SUBPATHS_COUNT: u64 = 1000;

#[derive(Debug, Clone, Default, Serialize)]
struct ClipboardState {
    content: String,
    version: u64,
    mtime: u64,
    user: Option<String>,
}

struct Clipboard {
    state: Arc<RwLock<ClipboardState>>,
    notifier: broadcast::Sender<u64>,
}

impl Default for Clipboard {
    fn default() -> Self {
        let (notifier, _) = broadcast::channel(16);
        Self {
            state: Arc::new(RwLock::new(ClipboardState::default())),
            notifier,
        }
    }
}

pub struct Server {
    args: Args,
    directory_auth: Option<DirectoryAuth>,
    assets_prefix: String,
    html: Cow<'static, str>,
    single_file_req_paths: Vec<String>,
    running: Arc<AtomicBool>,
    clipboard: Clipboard,
}

impl Server {
    pub fn init(args: Args, running: Arc<AtomicBool>) -> Result<Self> {
        let directory_auth = args
            .directory_auth_file
            .as_ref()
            .map(|file| DirectoryAuth::load(args.serve_path.clone(), file.clone()))
            .transpose()?;
        let assets_revision = assets_revision(args.assets.as_deref())?;
        let assets_prefix = format!(
            "__dufs_v{}_{}__/",
            env!("CARGO_PKG_VERSION"),
            assets_revision
        );
        let single_file_req_paths = if args.path_is_file {
            vec![
                args.uri_prefix.to_string(),
                args.uri_prefix[0..args.uri_prefix.len() - 1].to_string(),
                encode_uri(&format!(
                    "{}{}",
                    &args.uri_prefix,
                    get_file_name(&args.serve_path)
                )),
            ]
        } else {
            vec![]
        };
        let html = match args.assets.as_ref() {
            Some(path) => Cow::Owned(std::fs::read_to_string(path.join("index.html"))?),
            None => Cow::Borrowed(INDEX_HTML),
        };
        Ok(Self {
            args,
            directory_auth,
            running,
            single_file_req_paths,
            assets_prefix,
            html,
            clipboard: Clipboard::default(),
        })
    }

    pub async fn call(
        self: Arc<Self>,
        req: Request,
        addr: Option<SocketAddr>,
    ) -> Result<Response, hyper::Error> {
        let uri = req.uri().clone();
        let assets_prefix = &self.assets_prefix;
        let enable_cors = self.args.enable_cors;
        let mut http_log_data = self.args.http_logger.data(&req);
        if let Some(addr) = addr {
            http_log_data.insert("remote_addr".to_string(), addr.ip().to_string());
        }

        let mut res = match self.clone().handle(req).await {
            Ok(res) => {
                http_log_data.insert("status".to_string(), res.status().as_u16().to_string());
                if !uri.path().starts_with(assets_prefix) {
                    self.args.http_logger.log(&http_log_data, None);
                }
                res
            }
            Err(err) => {
                let mut res = Response::default();
                let status = StatusCode::INTERNAL_SERVER_ERROR;
                *res.status_mut() = status;
                http_log_data.insert("status".to_string(), status.as_u16().to_string());
                self.args
                    .http_logger
                    .log(&http_log_data, Some(err.to_string()));
                res
            }
        };

        if enable_cors {
            add_cors(&mut res);
        }
        res.headers_mut()
            .insert("referrer-policy", HeaderValue::from_static("no-referrer"));
        Ok(res)
    }

    pub async fn handle(self: Arc<Self>, req: Request) -> Result<Response> {
        let mut res = Response::default();

        let req_path = req.uri().path();
        let headers = req.headers();
        let method = req.method().clone();

        let relative_path = match self.resolve_path(req_path) {
            Some(v) => v,
            None => {
                status_bad_request(&mut res, "Invalid Path");
                return Ok(res);
            }
        };

        if method == Method::GET
            && self
                .handle_internal(&relative_path, headers, &mut res)
                .await?
        {
            return Ok(res);
        }

        let user_agent = headers
            .get("user-agent")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_lowercase())
            .unwrap_or_default();

        let is_microsoft_webdav = user_agent.starts_with("microsoft-webdav-miniredir/");

        if is_microsoft_webdav {
            // microsoft webdav requires this.
            res.headers_mut()
                .insert(CONNECTION, HeaderValue::from_static("close"));
        }

        let authorization = headers.get(AUTHORIZATION);

        let query = req.uri().query().unwrap_or_default();
        let mut query_params: HashMap<String, String> = form_urlencoded::parse(query.as_bytes())
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        let guard = self.args.auth.guard(
            &relative_path,
            &method,
            authorization,
            query_params.get("token"),
            is_microsoft_webdav,
        );

        let (user, access_paths) = if let Some(directory_auth) = &self.directory_auth {
            match guard {
                (Some(user), Some(access_paths)) => (Some(user), access_paths),
                (Some(_), None) => {
                    status_forbid(&mut res);
                    return Ok(res);
                }
                (None, _) if authorization.is_some() => {
                    self.auth_reject(&mut res)?;
                    return Ok(res);
                }
                (None, _) => {
                    if method.as_str() == "LOGOUT" {
                        self.auth_reject(&mut res)?;
                        return Ok(res);
                    }
                    let target = self
                        .join_path(&relative_path)
                        .ok_or_else(|| anyhow!("invalid path"))?;
                    if self.is_directory_auth_metadata(&target).await {
                        status_not_found(&mut res);
                        return Ok(res);
                    }
                    let password_grant = directory_auth
                        .grant_for_target(&relative_path, &target)
                        .await?;
                    let query_password = query_params.get(DIRECTORY_PASSWORD_QUERY);
                    let query_granted = password_grant
                        .as_ref()
                        .map(|(_, expected)| password_matches(expected, query_password))
                        .unwrap_or(false);
                    let mut cookie_granted = false;
                    if let Some((target_directory, _)) = &password_grant {
                        for (scope, candidate) in directory_password_cookies(headers) {
                            let scope_prefix = format!("{scope}/");
                            if target_directory != &scope
                                && !target_directory.starts_with(&scope_prefix)
                            {
                                continue;
                            }
                            if let Some(expected) = directory_auth.registered_password(&scope).await
                            {
                                if password_matches(&expected, Some(&candidate)) {
                                    cookie_granted = true;
                                    break;
                                }
                            }
                        }
                    }
                    let password_granted = query_granted || cookie_granted;

                    if query_granted && is_directory_share_readonly_method(&method) {
                        if let Some((directory, password)) = &password_grant {
                            self.set_directory_password_cookie(directory, password, &mut res)?;
                        }
                    }

                    if method.as_str() == "CHECKAUTH" {
                        if has_query_flag(&query_params, "login") || !password_granted {
                            self.auth_reject(&mut res)?;
                        } else {
                            *res.body_mut() = body_full("");
                        }
                        return Ok(res);
                    }
                    if method == Method::OPTIONS {
                        (None, AccessPaths::new(AccessPerm::ReadOnly))
                    } else if relative_path.is_empty()
                        && matches!(method, Method::GET | Method::HEAD)
                    {
                        (None, AccessPaths::default())
                    } else if password_granted {
                        if !is_directory_share_readonly_method(&method) {
                            status_forbid(&mut res);
                            return Ok(res);
                        }
                        (None, AccessPaths::new(AccessPerm::ReadOnly))
                    } else {
                        status_directory_password_required(&mut res);
                        return Ok(res);
                    }
                }
            }
        } else {
            match guard {
                (None, None) => {
                    self.auth_reject(&mut res)?;
                    return Ok(res);
                }
                (Some(_), None) => {
                    status_forbid(&mut res);
                    return Ok(res);
                }
                (x, Some(y)) => (x, y),
            }
        };

        if detect_noscript(&user_agent) {
            query_params.insert("noscript".to_string(), String::new());
        }

        if method.as_str() == "CHECKAUTH" {
            match user.clone() {
                Some(user) => {
                    *res.body_mut() = body_full(user);
                }
                None => {
                    if has_query_flag(&query_params, "login") || !access_paths.perm().readwrite() {
                        self.auth_reject(&mut res)?
                    } else {
                        *res.body_mut() = body_full("");
                    }
                }
            }
            return Ok(res);
        } else if method.as_str() == "LOGOUT" {
            self.auth_reject(&mut res)?;
            return Ok(res);
        }

        if has_query_flag(&query_params, "tokengen") {
            if let Some(user) = user {
                self.handle_tokengen(&relative_path, &user, &mut res)
                    .await?;
            } else {
                status_forbid(&mut res);
            }
            return Ok(res);
        }

        if relative_path == CLIPBOARD_EVENTS_PATH {
            if method == Method::GET {
                self.handle_clipboard_events(&mut res)?;
            } else {
                *res.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
            }
            return Ok(res);
        } else if relative_path == CLIPBOARD_PATH {
            match method {
                Method::GET | Method::HEAD => {
                    self.handle_clipboard_get(method == Method::HEAD, &mut res)
                        .await?;
                }
                Method::PUT => {
                    if self.args.allow_upload && access_paths.perm().readwrite() {
                        self.handle_clipboard_set(user, req, &mut res).await?;
                    } else {
                        status_forbid(&mut res);
                    }
                }
                _ => {
                    *res.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
                }
            }
            return Ok(res);
        }

        let head_only = method == Method::HEAD;

        if self.args.path_is_file {
            if self
                .single_file_req_paths
                .iter()
                .any(|v| v.as_str() == req_path)
            {
                self.handle_send_file(&self.args.serve_path, headers, head_only, &mut res)
                    .await?;
            } else {
                self.handle_not_found(&query_params, headers, head_only, &mut res)
                    .await?;
            }
            return Ok(res);
        }
        let path = match self.join_path(&relative_path) {
            Some(v) => v,
            None => {
                status_forbid(&mut res);
                return Ok(res);
            }
        };

        let path = path.as_path();
        if self.is_directory_auth_metadata(path).await {
            status_not_found(&mut res);
            return Ok(res);
        }

        let (is_miss, is_dir, is_file, size) = match fs::metadata(path).await.ok() {
            Some(meta) => (false, meta.is_dir(), meta.is_file(), meta.len()),
            None => (true, false, false, 0),
        };

        let allow_upload = self.args.allow_upload;
        let allow_delete = self.args.allow_delete;
        let allow_search = self.args.allow_search;
        let allow_archive = self.args.allow_archive;
        let render_index = self.args.render_index;
        let render_spa = self.args.render_spa;
        let render_try_index = self.args.render_try_index;

        if self.guard_root_contained(path).await {
            self.handle_not_found(&query_params, headers, head_only, &mut res)
                .await?;
            return Ok(res);
        }

        if method.as_str() == "SETPASSWORD" {
            if user.is_none() {
                status_forbid(&mut res);
            } else if !is_dir {
                status_not_found(&mut res);
            } else {
                self.handle_set_directory_password(path, req, &mut res)
                    .await?;
            }
            return Ok(res);
        }

        match method {
            Method::GET | Method::HEAD => {
                if is_dir {
                    if render_try_index {
                        if allow_archive && has_query_flag(&query_params, "zip") {
                            if !allow_archive {
                                self.handle_not_found(&query_params, headers, head_only, &mut res)
                                    .await?;
                                return Ok(res);
                            }
                            self.handle_zip_dir(path, head_only, access_paths, &mut res)
                                .await?;
                        } else if allow_search && query_params.contains_key("q") {
                            self.handle_search_dir(
                                path,
                                &query_params,
                                head_only,
                                user,
                                access_paths,
                                &mut res,
                            )
                            .await?;
                        } else {
                            self.handle_render_index(
                                path,
                                &query_params,
                                headers,
                                head_only,
                                user,
                                access_paths,
                                &mut res,
                            )
                            .await?;
                        }
                    } else if render_index || render_spa {
                        self.handle_render_index(
                            path,
                            &query_params,
                            headers,
                            head_only,
                            user,
                            access_paths,
                            &mut res,
                        )
                        .await?;
                    } else if has_query_flag(&query_params, "zip") {
                        if !allow_archive {
                            status_not_found(&mut res);
                            return Ok(res);
                        }
                        self.handle_zip_dir(path, head_only, access_paths, &mut res)
                            .await?;
                    } else if allow_search && query_params.contains_key("q") {
                        self.handle_search_dir(
                            path,
                            &query_params,
                            head_only,
                            user,
                            access_paths,
                            &mut res,
                        )
                        .await?;
                    } else {
                        self.handle_ls_dir(
                            path,
                            true,
                            &query_params,
                            head_only,
                            user,
                            access_paths,
                            &mut res,
                        )
                        .await?;
                    }
                } else if is_file {
                    if has_query_flag(&query_params, "json") {
                        self.handle_file_json(path, head_only, &mut res).await?;
                    } else if has_query_flag(&query_params, "edit") {
                        let readwrite = access_paths.perm().readwrite();
                        let kind = if readwrite {
                            DataKind::Edit
                        } else {
                            DataKind::View
                        };
                        self.handle_edit_file(path, kind, head_only, user, readwrite, &mut res)
                            .await?;
                    } else if has_query_flag(&query_params, "view") {
                        let readwrite = access_paths.perm().readwrite();
                        self.handle_edit_file(
                            path,
                            DataKind::View,
                            head_only,
                            user,
                            readwrite,
                            &mut res,
                        )
                        .await?;
                    } else if has_query_flag(&query_params, "hash") {
                        if self.args.allow_hash {
                            self.handle_hash_file(path, head_only, &mut res).await?;
                        } else {
                            status_forbid(&mut res);
                        }
                    } else {
                        self.handle_send_file(path, headers, head_only, &mut res)
                            .await?;
                    }
                } else if render_spa {
                    self.handle_render_spa(path, &query_params, headers, head_only, &mut res)
                        .await?;
                } else if allow_upload && req_path.ends_with('/') {
                    self.handle_ls_dir(
                        path,
                        false,
                        &query_params,
                        head_only,
                        user,
                        access_paths,
                        &mut res,
                    )
                    .await?;
                } else {
                    self.handle_not_found(&query_params, headers, head_only, &mut res)
                        .await?;
                }
            }
            Method::OPTIONS => {
                set_webdav_headers(&mut res);
            }
            Method::PUT => {
                if is_dir || !allow_upload || (!allow_delete && size > 0) {
                    status_forbid(&mut res);
                } else {
                    self.handle_upload(path, None, size, req, &mut res).await?;
                }
            }
            Method::PATCH => {
                if is_miss {
                    status_not_found(&mut res);
                } else if !allow_upload {
                    status_forbid(&mut res);
                } else {
                    let offset = match parse_upload_offset(headers, size) {
                        Ok(v) => v,
                        Err(err) => {
                            status_bad_request(&mut res, &err.to_string());
                            return Ok(res);
                        }
                    };
                    match offset {
                        Some(offset) => {
                            if offset < size && !allow_delete {
                                status_forbid(&mut res);
                                return Ok(res);
                            }
                            self.handle_upload(path, Some(offset), size, req, &mut res)
                                .await?;
                        }
                        None => {
                            *res.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
                        }
                    }
                }
            }
            Method::DELETE => {
                if !allow_delete {
                    status_forbid(&mut res);
                } else if !is_miss {
                    self.handle_delete(path, is_dir, &mut res).await?
                } else {
                    status_not_found(&mut res);
                }
            }
            method => match method.as_str() {
                "PROPFIND" => {
                    if is_dir {
                        let access_paths =
                            if access_paths.perm().indexonly() && authorization.is_none() {
                                // see https://github.com/sigoden/dufs/issues/229
                                AccessPaths::new(AccessPerm::ReadOnly)
                            } else {
                                access_paths
                            };
                        self.handle_propfind_dir(path, headers, access_paths, &mut res)
                            .await?;
                    } else if is_file {
                        self.handle_propfind_file(path, &mut res).await?;
                    } else {
                        status_not_found(&mut res);
                    }
                }
                "PROPPATCH" => {
                    if is_file {
                        self.handle_proppatch(req_path, &mut res).await?;
                    } else {
                        status_not_found(&mut res);
                    }
                }
                "MKCOL" => {
                    if !allow_upload {
                        status_forbid(&mut res);
                    } else if !is_miss {
                        *res.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
                        *res.body_mut() = body_full("Already exists");
                    } else {
                        self.handle_mkcol(path, &mut res).await?;
                    }
                }
                "COPY" => {
                    if !allow_upload {
                        status_forbid(&mut res);
                    } else if is_miss {
                        status_not_found(&mut res);
                    } else {
                        self.handle_copy(path, &req, &mut res).await?
                    }
                }
                "MOVE" => {
                    if !allow_upload || !allow_delete {
                        status_forbid(&mut res);
                    } else if is_miss {
                        status_not_found(&mut res);
                    } else {
                        self.handle_move(path, &req, &mut res).await?
                    }
                }
                "LOCK" => {
                    // Fake lock
                    if is_file {
                        let has_auth = authorization.is_some();
                        self.handle_lock(req_path, has_auth, &mut res).await?;
                    } else {
                        status_not_found(&mut res);
                    }
                }
                "UNLOCK" => {
                    // Fake unlock
                    if is_miss {
                        status_not_found(&mut res);
                    }
                }
                _ => {
                    *res.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
                }
            },
        }
        Ok(res)
    }

    async fn handle_upload(
        &self,
        path: &Path,
        upload_offset: Option<u64>,
        size: u64,
        req: Request,
        res: &mut Response,
    ) -> Result<()> {
        ensure_path_parent(path).await?;
        self.ensure_directory_auth_for_parent(path).await?;
        let (mut file, status) = match upload_offset {
            None => (fs::File::create(path).await?, StatusCode::CREATED),
            Some(offset) if offset == size => (
                fs::OpenOptions::new().append(true).open(path).await?,
                StatusCode::NO_CONTENT,
            ),
            Some(offset) => {
                let mut file = fs::OpenOptions::new().write(true).open(path).await?;
                file.seek(SeekFrom::Start(offset)).await?;
                (file, StatusCode::NO_CONTENT)
            }
        };
        let stream = IncomingStream::new(req.into_body());

        let body_with_io_error = stream.map_err(io::Error::other);
        let body_reader = StreamReader::new(body_with_io_error);

        pin_mut!(body_reader);

        let ret = io::copy(&mut body_reader, &mut file).await;
        let size = fs::metadata(path)
            .await
            .map(|v| v.len())
            .unwrap_or_default();
        if ret.is_err() {
            if upload_offset.is_none() && size < RESUMABLE_UPLOAD_MIN_SIZE {
                let _ = tokio::fs::remove_file(&path).await;
            }
            ret?;
        }

        *res.status_mut() = status;

        Ok(())
    }

    async fn handle_delete(&self, path: &Path, is_dir: bool, res: &mut Response) -> Result<()> {
        let relative_path = self.relative_path(path)?;
        match is_dir {
            true => fs::remove_dir_all(path).await?,
            false => fs::remove_file(path).await?,
        }
        if is_dir && !relative_path.is_empty() {
            if let Some(directory_auth) = &self.directory_auth {
                directory_auth.remove_tree(&relative_path).await?;
            }
        }

        status_no_content(res);
        Ok(())
    }

    async fn handle_ls_dir(
        &self,
        path: &Path,
        exist: bool,
        query_params: &HashMap<String, String>,
        head_only: bool,
        user: Option<String>,
        access_paths: AccessPaths,
        res: &mut Response,
    ) -> Result<()> {
        let mut paths = vec![];
        if !head_only && exist {
            paths = match self.list_dir(path, path, access_paths.clone()).await {
                Ok(paths) => paths,
                Err(_) => {
                    status_forbid(res);
                    return Ok(());
                }
            }
        };
        self.send_index(
            path,
            paths,
            exist,
            query_params,
            head_only,
            user,
            access_paths,
            res,
        )
        .await
    }

    async fn handle_search_dir(
        &self,
        path: &Path,
        query_params: &HashMap<String, String>,
        head_only: bool,
        user: Option<String>,
        access_paths: AccessPaths,
        res: &mut Response,
    ) -> Result<()> {
        let mut paths: Vec<PathItem> = vec![];
        let search = query_params
            .get("q")
            .ok_or_else(|| anyhow!("invalid q"))?
            .to_lowercase();
        if search.is_empty() {
            return self
                .handle_ls_dir(path, true, query_params, head_only, user, access_paths, res)
                .await;
        }

        if !head_only {
            let path_buf = path.to_path_buf();
            let hidden = Arc::new(self.args.hidden.to_vec());
            let search = search.clone();
            let excluded_path = self
                .directory_auth
                .as_ref()
                .map(|auth| auth.metadata_path().to_path_buf());

            let search_paths = tokio::spawn(collect_dir_entries(
                access_paths.clone(),
                self.running.clone(),
                path_buf,
                hidden,
                self.args.allow_symlink,
                self.args.serve_path.clone(),
                excluded_path,
                move |x| get_file_name(x.path()).to_lowercase().contains(&search),
            ))
            .await?;

            for search_path in search_paths.into_iter() {
                if let Ok(Some(item)) = self.to_pathitem(search_path, path.to_path_buf()).await {
                    paths.push(item);
                }
            }
        }
        self.send_index(
            path,
            paths,
            true,
            query_params,
            head_only,
            user,
            access_paths,
            res,
        )
        .await
    }

    async fn handle_zip_dir(
        &self,
        path: &Path,
        head_only: bool,
        access_paths: AccessPaths,
        res: &mut Response,
    ) -> Result<()> {
        let (mut writer, reader) = tokio::io::duplex(BUF_SIZE);
        let filename = try_get_file_name(path)?;
        set_content_disposition(res, false, &format!("{filename}.zip"))?;
        res.headers_mut()
            .insert("content-type", HeaderValue::from_static("application/zip"));
        if head_only {
            return Ok(());
        }
        let path = path.to_owned();
        let hidden = self.args.hidden.clone();
        let running = self.running.clone();
        let compression = self.args.compress.to_compression();
        let follow_symlinks = self.args.allow_symlink;
        let serve_path = self.args.serve_path.clone();
        let excluded_path = self
            .directory_auth
            .as_ref()
            .map(|auth| auth.metadata_path().to_path_buf());
        tokio::spawn(async move {
            if let Err(e) = zip_dir(
                &mut writer,
                &path,
                access_paths,
                &hidden,
                compression,
                follow_symlinks,
                serve_path,
                excluded_path,
                running,
            )
            .await
            {
                error!("Failed to zip {}, {e}", path.display());
            }
        });
        let reader_stream = ReaderStream::with_capacity(reader, BUF_SIZE);
        let stream_body = StreamBody::new(
            reader_stream
                .map_ok(Frame::data)
                .map_err(|err| anyhow!("{err}")),
        );
        let boxed_body = stream_body.boxed();
        *res.body_mut() = boxed_body;
        Ok(())
    }

    async fn handle_render_index(
        &self,
        path: &Path,
        query_params: &HashMap<String, String>,
        headers: &HeaderMap<HeaderValue>,
        head_only: bool,
        user: Option<String>,
        access_paths: AccessPaths,
        res: &mut Response,
    ) -> Result<()> {
        let index_path = path.join(INDEX_NAME);
        if fs::metadata(&index_path)
            .await
            .ok()
            .map(|v| v.is_file())
            .unwrap_or_default()
        {
            self.handle_send_file(&index_path, headers, head_only, res)
                .await?;
        } else if self.args.render_try_index {
            self.handle_ls_dir(path, true, query_params, head_only, user, access_paths, res)
                .await?;
        } else {
            self.handle_not_found(query_params, headers, head_only, res)
                .await?;
        }
        Ok(())
    }

    async fn handle_file_json(
        &self,
        path: &Path,
        head_only: bool,
        res: &mut Response,
    ) -> Result<()> {
        let pathitem = match self.to_pathitem(path, &self.args.serve_path).await? {
            Some(v) => v,
            None => {
                status_not_found(res);
                return Ok(());
            }
        };
        let output = serde_json::to_string_pretty(&pathitem)?;
        res.headers_mut()
            .typed_insert(ContentType::from(mime_guess::mime::APPLICATION_JSON));
        res.headers_mut()
            .typed_insert(ContentLength(output.len() as u64));
        if head_only {
            return Ok(());
        }
        *res.body_mut() = body_full(output);
        Ok(())
    }

    async fn handle_render_spa(
        &self,
        path: &Path,
        query_params: &HashMap<String, String>,
        headers: &HeaderMap<HeaderValue>,
        head_only: bool,
        res: &mut Response,
    ) -> Result<()> {
        if path.extension().is_none() {
            let path = self.args.serve_path.join(INDEX_NAME);
            self.handle_send_file(&path, headers, head_only, res)
                .await?;
        } else {
            self.handle_not_found(query_params, headers, head_only, res)
                .await?;
        }
        Ok(())
    }

    async fn handle_not_found(
        &self,
        query_params: &HashMap<String, String>,
        headers: &HeaderMap<HeaderValue>,
        head_only: bool,
        res: &mut Response,
    ) -> Result<()> {
        if let Some(error_page) = &self.args.error_page {
            if !has_query_flag(query_params, "noscript") {
                self.handle_send_file(error_page, headers, head_only, res)
                    .await?;
                *res.status_mut() = StatusCode::NOT_FOUND;
                return Ok(());
            }
        }
        status_not_found(res);
        Ok(())
    }

    async fn handle_internal(
        &self,
        req_path: &str,
        headers: &HeaderMap<HeaderValue>,
        res: &mut Response,
    ) -> Result<bool> {
        if let Some(name) = req_path.strip_prefix(&self.assets_prefix) {
            match self.args.assets.as_ref() {
                Some(assets_path) => {
                    let path = assets_path.join(name);
                    if path.exists() {
                        self.handle_send_file(&path, headers, false, res).await?;
                    } else {
                        status_not_found(res);
                        return Ok(true);
                    }
                }
                None => match name {
                    "index.js" => {
                        *res.body_mut() = body_full(INDEX_JS);
                        res.headers_mut().insert(
                            "content-type",
                            HeaderValue::from_static("application/javascript; charset=UTF-8"),
                        );
                    }
                    "index.css" => {
                        *res.body_mut() = body_full(INDEX_CSS);
                        res.headers_mut().insert(
                            "content-type",
                            HeaderValue::from_static("text/css; charset=UTF-8"),
                        );
                    }
                    "favicon.ico" => {
                        *res.body_mut() = body_full(FAVICON_ICO);
                        res.headers_mut()
                            .insert("content-type", HeaderValue::from_static("image/x-icon"));
                    }
                    _ => {
                        status_not_found(res);
                    }
                },
            }
            res.headers_mut().insert(
                "cache-control",
                HeaderValue::from_static("public, max-age=31536000, immutable"),
            );
            res.headers_mut().insert(
                "x-content-type-options",
                HeaderValue::from_static("nosniff"),
            );
            Ok(true)
        } else if req_path == HEALTH_CHECK_PATH {
            res.headers_mut()
                .typed_insert(ContentType::from(mime_guess::mime::APPLICATION_JSON));

            *res.body_mut() = body_full(r#"{"status":"OK"}"#);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn handle_send_file(
        &self,
        path: &Path,
        headers: &HeaderMap<HeaderValue>,
        head_only: bool,
        res: &mut Response,
    ) -> Result<()> {
        let (file, meta) = tokio::join!(fs::File::open(path), fs::metadata(path),);
        let (mut file, meta) = (file?, meta?);
        let size = meta.len();
        let mut use_range = true;
        if let Some((etag, last_modified)) = extract_cache_headers(&meta) {
            if let Some(if_unmodified_since) = headers.typed_get::<IfUnmodifiedSince>() {
                if !if_unmodified_since.precondition_passes(last_modified.into()) {
                    *res.status_mut() = StatusCode::PRECONDITION_FAILED;
                    return Ok(());
                }
            }
            if let Some(if_match) = headers.typed_get::<IfMatch>() {
                if !if_match.precondition_passes(&etag) {
                    *res.status_mut() = StatusCode::PRECONDITION_FAILED;
                    return Ok(());
                }
            }
            if let Some(if_modified_since) = headers.typed_get::<IfModifiedSince>() {
                if !if_modified_since.is_modified(last_modified.into()) {
                    *res.status_mut() = StatusCode::NOT_MODIFIED;
                    return Ok(());
                }
            }
            if let Some(if_none_match) = headers.typed_get::<IfNoneMatch>() {
                if !if_none_match.precondition_passes(&etag) {
                    *res.status_mut() = StatusCode::NOT_MODIFIED;
                    return Ok(());
                }
            }

            res.headers_mut()
                .typed_insert(CacheControl::new().with_no_cache());
            res.headers_mut().typed_insert(last_modified);
            res.headers_mut().typed_insert(etag.clone());

            if headers.typed_get::<Range>().is_some() {
                use_range = headers
                    .typed_get::<IfRange>()
                    .map(|if_range| !if_range.is_modified(Some(&etag), Some(&last_modified)))
                    // Always be fresh if there is no validators
                    .unwrap_or(true);
            } else {
                use_range = false;
            }
        }

        let ranges = if use_range {
            headers.get(RANGE).map(|range| {
                range
                    .to_str()
                    .ok()
                    .and_then(|range| parse_range(range, size))
            })
        } else {
            None
        };

        res.headers_mut().insert(
            CONTENT_TYPE,
            HeaderValue::from_str(&get_content_type(path).await?)?,
        );

        let filename = try_get_file_name(path)?;
        set_content_disposition(res, true, filename)?;

        res.headers_mut().typed_insert(AcceptRanges::bytes());

        if let Some(ranges) = ranges {
            if let Some(ranges) = ranges {
                if ranges.len() == 1 {
                    let (start, end) = ranges[0];
                    file.seek(SeekFrom::Start(start)).await?;
                    let range_size = end - start + 1;
                    *res.status_mut() = StatusCode::PARTIAL_CONTENT;
                    let content_range = format!("bytes {start}-{end}/{size}");
                    res.headers_mut()
                        .insert(CONTENT_RANGE, content_range.parse()?);
                    res.headers_mut()
                        .insert(CONTENT_LENGTH, format!("{range_size}").parse()?);
                    if head_only {
                        return Ok(());
                    }

                    let stream_body = StreamBody::new(
                        LengthLimitedStream::new(file, range_size as usize)
                            .map_ok(Frame::data)
                            .map_err(|err| anyhow!("{err}")),
                    );
                    let boxed_body = stream_body.boxed();
                    *res.body_mut() = boxed_body;
                } else {
                    *res.status_mut() = StatusCode::PARTIAL_CONTENT;
                    let boundary = Uuid::new_v4();
                    let mut body = Vec::new();
                    let content_type = get_content_type(path).await?;
                    for (start, end) in ranges {
                        file.seek(SeekFrom::Start(start)).await?;
                        let range_size = end - start + 1;
                        let content_range = format!("bytes {start}-{end}/{size}");
                        let part_header = format!(
                            "--{boundary}\r\nContent-Type: {content_type}\r\nContent-Range: {content_range}\r\n\r\n",
                        );
                        body.extend_from_slice(part_header.as_bytes());
                        let mut buffer = vec![0; range_size as usize];
                        file.read_exact(&mut buffer).await?;
                        body.extend_from_slice(&buffer);
                        body.extend_from_slice(b"\r\n");
                    }
                    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
                    res.headers_mut().insert(
                        CONTENT_TYPE,
                        format!("multipart/byteranges; boundary={boundary}").parse()?,
                    );
                    res.headers_mut()
                        .insert(CONTENT_LENGTH, format!("{}", body.len()).parse()?);
                    if head_only {
                        return Ok(());
                    }
                    *res.body_mut() = body_full(body);
                }
            } else {
                *res.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
                res.headers_mut()
                    .insert(CONTENT_RANGE, format!("bytes */{size}").parse()?);
            }
        } else {
            res.headers_mut()
                .insert(CONTENT_LENGTH, format!("{size}").parse()?);
            if head_only {
                return Ok(());
            }

            let reader_stream = ReaderStream::with_capacity(file, BUF_SIZE);
            let stream_body = StreamBody::new(
                reader_stream
                    .map_ok(Frame::data)
                    .map_err(|err| anyhow!("{err}")),
            );
            let boxed_body = stream_body.boxed();
            *res.body_mut() = boxed_body;
        }
        Ok(())
    }

    async fn handle_edit_file(
        &self,
        path: &Path,
        kind: DataKind,
        head_only: bool,
        user: Option<String>,
        readwrite: bool,
        res: &mut Response,
    ) -> Result<()> {
        let (file, meta) = tokio::join!(fs::File::open(path), fs::metadata(path),);
        let (file, meta) = (file?, meta?);
        let href = format!(
            "/{}",
            normalize_path(path.strip_prefix(&self.args.serve_path)?)
        );
        let mut buffer: Vec<u8> = vec![];
        file.take(1024).read_to_end(&mut buffer).await?;
        let editable =
            meta.len() <= EDITABLE_TEXT_MAX_SIZE && content_inspector::inspect(&buffer).is_text();
        let directory_password = if let Some(directory_auth) = &self.directory_auth {
            let relative_path = self.relative_path(path)?;
            directory_auth
                .password_for_target(&relative_path, path)
                .await?
        } else {
            None
        };
        let data = EditData {
            href,
            kind,
            uri_prefix: self.args.uri_prefix.clone(),
            allow_upload: self.args.allow_upload && readwrite,
            allow_delete: self.args.allow_delete && readwrite,
            auth: self.args.auth.has_users(),
            user,
            editable,
            directory_auth: self.directory_auth.is_some(),
            directory_password,
        };
        res.headers_mut()
            .typed_insert(ContentType::from(mime_guess::mime::TEXT_HTML_UTF_8));
        let index_data = STANDARD.encode(serde_json::to_string(&data)?);
        let output = self
            .html
            .replace(
                "__ASSETS_PREFIX__",
                &format!("{}{}", self.args.uri_prefix, self.assets_prefix),
            )
            .replace("__INDEX_DATA__", &index_data);
        res.headers_mut()
            .typed_insert(ContentLength(output.len() as u64));
        res.headers_mut()
            .typed_insert(CacheControl::new().with_no_cache());
        if head_only {
            return Ok(());
        }
        *res.body_mut() = body_full(output);
        Ok(())
    }

    async fn handle_hash_file(
        &self,
        path: &Path,
        head_only: bool,
        res: &mut Response,
    ) -> Result<()> {
        let output = sha256_file(path).await?;
        res.headers_mut()
            .typed_insert(ContentType::from(mime_guess::mime::TEXT_HTML_UTF_8));
        res.headers_mut()
            .typed_insert(ContentLength(output.len() as u64));
        if head_only {
            return Ok(());
        }
        *res.body_mut() = body_full(output);
        Ok(())
    }

    async fn handle_tokengen(
        &self,
        relative_path: &str,
        user: &str,
        res: &mut Response,
    ) -> Result<()> {
        let output = self.args.auth.generate_token(relative_path, user)?;
        res.headers_mut()
            .typed_insert(ContentType::from(mime_guess::mime::TEXT_PLAIN_UTF_8));
        res.headers_mut()
            .typed_insert(ContentLength(output.len() as u64));
        *res.body_mut() = body_full(output);
        Ok(())
    }

    async fn handle_clipboard_get(&self, head_only: bool, res: &mut Response) -> Result<()> {
        let state = self.clipboard.state.read().await.clone();
        let output = serde_json::to_string(&state)?;
        res.headers_mut()
            .typed_insert(ContentType::from(mime_guess::mime::APPLICATION_JSON));
        res.headers_mut()
            .typed_insert(ContentLength(output.len() as u64));
        res.headers_mut()
            .typed_insert(CacheControl::new().with_no_cache());
        if head_only {
            return Ok(());
        }
        *res.body_mut() = body_full(output);
        Ok(())
    }

    async fn handle_clipboard_set(
        &self,
        user: Option<String>,
        req: Request,
        res: &mut Response,
    ) -> Result<()> {
        let body = Limited::new(req.into_body(), CLIPBOARD_MAX_SIZE as usize);
        let content = match body.collect().await {
            Ok(collected) => match String::from_utf8(collected.to_bytes().to_vec()) {
                Ok(v) => v,
                Err(_) => {
                    status_bad_request(res, "Invalid UTF-8 content");
                    return Ok(());
                }
            },
            Err(_) => {
                status_bad_request(res, "Content too large");
                return Ok(());
            }
        };
        let version = {
            let mut state = self.clipboard.state.write().await;
            state.content = content;
            state.version += 1;
            state.mtime = unix_now().as_millis() as u64;
            state.user = user;
            state.version
        };
        let _ = self.clipboard.notifier.send(version);
        status_no_content(res);
        Ok(())
    }

    fn handle_clipboard_events(&self, res: &mut Response) -> Result<()> {
        let mut receiver = self.clipboard.notifier.subscribe();
        let state = self.clipboard.state.clone();
        let stream = async_stream::stream! {
            // Push the current state immediately on connect.
            {
                let snapshot = state.read().await.clone();
                if let Ok(data) = serde_json::to_string(&snapshot) {
                    yield Ok(Frame::data(Bytes::from(format!("data: {data}\n\n"))));
                }
            }
            loop {
                match receiver.recv().await {
                    Ok(_) => {
                        let snapshot = state.read().await.clone();
                        if let Ok(data) = serde_json::to_string(&snapshot) {
                            yield Ok(Frame::data(Bytes::from(format!("data: {data}\n\n"))));
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        };
        res.headers_mut().insert(
            CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream; charset=UTF-8"),
        );
        res.headers_mut()
            .typed_insert(CacheControl::new().with_no_cache());
        res.headers_mut()
            .insert(CONNECTION, HeaderValue::from_static("keep-alive"));
        *res.body_mut() = StreamBody::new(stream).boxed();
        Ok(())
    }

    async fn handle_propfind_dir(
        &self,
        path: &Path,
        headers: &HeaderMap<HeaderValue>,
        access_paths: AccessPaths,
        res: &mut Response,
    ) -> Result<()> {
        let depth: u32 = match headers.get("depth") {
            Some(v) => match v.to_str().ok().and_then(|v| v.parse().ok()) {
                Some(0) => 0,
                Some(1) => 1,
                _ => {
                    status_bad_request(res, "Invalid depth: only 0 and 1 are allowed.");
                    return Ok(());
                }
            },
            None => 1,
        };
        let mut paths = match self.to_pathitem(path, &self.args.serve_path).await? {
            Some(v) => vec![v],
            None => vec![],
        };
        if depth == 1 {
            match self
                .list_dir(path, &self.args.serve_path, access_paths)
                .await
            {
                Ok(child) => paths.extend(child),
                Err(_) => {
                    status_forbid(res);
                    return Ok(());
                }
            }
        }
        let output = paths
            .iter()
            .map(|v| v.to_dav_xml(self.args.uri_prefix.as_str()))
            .fold(String::new(), |mut acc, v| {
                acc.push_str(&v);
                acc
            });
        res_multistatus(res, &output);
        Ok(())
    }

    async fn handle_propfind_file(&self, path: &Path, res: &mut Response) -> Result<()> {
        if let Some(pathitem) = self.to_pathitem(path, &self.args.serve_path).await? {
            res_multistatus(res, &pathitem.to_dav_xml(self.args.uri_prefix.as_str()));
        } else {
            status_not_found(res);
        }
        Ok(())
    }

    async fn handle_mkcol(&self, path: &Path, res: &mut Response) -> Result<()> {
        fs::create_dir_all(path).await?;
        let relative_path = self.relative_path(path)?;
        if let Some(directory_auth) = &self.directory_auth {
            directory_auth.ensure_directory_tree(&relative_path).await?;
            let password = directory_auth
                .password_for_directory(&relative_path)
                .await?;
            res.headers_mut().insert(
                "x-dufs-directory-password",
                HeaderValue::from_str(&password)?,
            );
        }
        *res.status_mut() = StatusCode::CREATED;
        Ok(())
    }

    async fn handle_copy(&self, path: &Path, req: &Request, res: &mut Response) -> Result<()> {
        let dest = match self.extract_dest(req, res).await? {
            Some(dest) => dest,
            None => {
                return Ok(());
            }
        };

        let meta = fs::symlink_metadata(path).await?;
        if meta.is_dir() {
            status_forbid(res);
            return Ok(());
        }
        if self.is_directory_auth_metadata(&dest).await {
            status_bad_request(res, "Invalid Destination");
            return Ok(());
        }

        ensure_path_parent(&dest).await?;
        self.ensure_directory_auth_for_parent(&dest).await?;

        if self.guard_root_contained(&dest).await {
            status_bad_request(res, "Invalid Destination");
            return Ok(());
        }

        fs::copy(path, &dest).await?;

        status_no_content(res);
        Ok(())
    }

    async fn handle_move(&self, path: &Path, req: &Request, res: &mut Response) -> Result<()> {
        let dest = match self.extract_dest(req, res).await? {
            Some(dest) => dest,
            None => {
                return Ok(());
            }
        };

        let is_dir = fs::symlink_metadata(path).await?.is_dir();
        let source_relative = self.relative_path(path)?;
        let destination_relative = self.relative_path(&dest)?;
        if self.is_directory_auth_metadata(&dest).await {
            status_bad_request(res, "Invalid Destination");
            return Ok(());
        }
        ensure_path_parent(&dest).await?;
        self.ensure_directory_auth_for_parent(&dest).await?;

        if self.guard_root_contained(&dest).await {
            status_bad_request(res, "Invalid Destination");
            return Ok(());
        }

        fs::rename(path, &dest).await?;
        if is_dir {
            if let Some(directory_auth) = &self.directory_auth {
                directory_auth
                    .move_tree(&source_relative, &destination_relative)
                    .await?;
                directory_auth
                    .ensure_directory_tree(&destination_relative)
                    .await?;
            }
        }

        status_no_content(res);
        Ok(())
    }

    async fn handle_set_directory_password(
        &self,
        path: &Path,
        req: Request,
        res: &mut Response,
    ) -> Result<()> {
        let Some(directory_auth) = &self.directory_auth else {
            status_not_found(res);
            return Ok(());
        };
        let body = Limited::new(req.into_body(), 129);
        let password = match body.collect().await {
            Ok(collected) => match String::from_utf8(collected.to_bytes().to_vec()) {
                Ok(password) => password,
                Err(_) => {
                    status_bad_request(res, "Directory password must be valid UTF-8");
                    return Ok(());
                }
            },
            Err(_) => {
                status_bad_request(res, "Directory password is too long");
                return Ok(());
            }
        };
        let relative_path = self.relative_path(path)?;
        if relative_path.is_empty() {
            status_forbid(res);
            return Ok(());
        }
        if let Err(err) = directory_auth.set_password(&relative_path, &password).await {
            status_bad_request(res, &err.to_string());
            return Ok(());
        }
        status_no_content(res);
        Ok(())
    }

    async fn handle_lock(&self, req_path: &str, auth: bool, res: &mut Response) -> Result<()> {
        let token = if auth {
            format!("opaquelocktoken:{}", Uuid::new_v4())
        } else {
            Utc::now().timestamp().to_string()
        };

        res.headers_mut().insert(
            "content-type",
            HeaderValue::from_static("application/xml; charset=utf-8"),
        );
        res.headers_mut()
            .insert("lock-token", format!("<{token}>").parse()?);

        *res.body_mut() = body_full(format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<D:prop xmlns:D="DAV:"><D:lockdiscovery><D:activelock>
<D:locktoken><D:href>{token}</D:href></D:locktoken>
<D:lockroot><D:href>{req_path}</D:href></D:lockroot>
</D:activelock></D:lockdiscovery></D:prop>"#
        ));
        Ok(())
    }

    async fn handle_proppatch(&self, req_path: &str, res: &mut Response) -> Result<()> {
        let output = format!(
            r#"<D:response>
<D:href>{req_path}</D:href>
<D:propstat>
<D:prop>
</D:prop>
<D:status>HTTP/1.1 403 Forbidden</D:status>
</D:propstat>
</D:response>"#
        );
        res_multistatus(res, &output);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_index(
        &self,
        path: &Path,
        mut paths: Vec<PathItem>,
        exist: bool,
        query_params: &HashMap<String, String>,
        head_only: bool,
        user: Option<String>,
        access_paths: AccessPaths,
        res: &mut Response,
    ) -> Result<()> {
        if let Some(sort) = query_params.get("sort") {
            if sort == "name" {
                paths.sort_by(|v1, v2| v1.sort_by_name(v2))
            } else if sort == "mtime" {
                paths.sort_by(|v1, v2| v1.sort_by_mtime(v2))
            } else if sort == "size" {
                paths.sort_by(|v1, v2| v1.sort_by_size(v2))
            }
            if query_params
                .get("order")
                .map(|v| v == "desc")
                .unwrap_or_default()
            {
                paths.reverse()
            }
        } else {
            paths.sort_by(|v1, v2| v1.sort_by_name(v2))
        }
        if has_query_flag(query_params, "simple") {
            let output = paths
                .into_iter()
                .map(|v| {
                    let displayname = escape_str_pcdata(&v.name);
                    if v.is_dir() {
                        format!("{displayname}/\n")
                    } else {
                        format!("{displayname}\n")
                    }
                })
                .collect::<Vec<String>>()
                .join("");
            res.headers_mut()
                .typed_insert(ContentType::from(mime_guess::mime::TEXT_HTML_UTF_8));
            res.headers_mut()
                .typed_insert(ContentLength(output.len() as u64));
            *res.body_mut() = body_full(output);
            if head_only {
                return Ok(());
            }
            return Ok(());
        }
        let href = format!(
            "/{}",
            normalize_path(path.strip_prefix(&self.args.serve_path)?)
        );
        let readwrite = access_paths.perm().readwrite();
        let directory_password = self.directory_password_for_path(path).await?;
        let data = IndexData {
            kind: DataKind::Index,
            href,
            uri_prefix: self.args.uri_prefix.clone(),
            allow_upload: self.args.allow_upload && readwrite,
            allow_delete: self.args.allow_delete && readwrite,
            allow_search: self.args.allow_search,
            allow_archive: self.args.allow_archive,
            dir_exists: exist,
            auth: self.args.auth.has_users(),
            user,
            directory_auth: self.directory_auth.is_some(),
            directory_password,
            paths,
        };
        let output = if has_query_flag(query_params, "json") {
            res.headers_mut()
                .typed_insert(ContentType::from(mime_guess::mime::APPLICATION_JSON));
            serde_json::to_string_pretty(&data)?
        } else if has_query_flag(query_params, "noscript") {
            res.headers_mut()
                .typed_insert(ContentType::from(mime_guess::mime::TEXT_HTML_UTF_8));
            generate_noscript_html(&data)?
        } else {
            res.headers_mut()
                .typed_insert(ContentType::from(mime_guess::mime::TEXT_HTML_UTF_8));

            let index_data = STANDARD.encode(serde_json::to_string(&data)?);
            self.html
                .replace(
                    "__ASSETS_PREFIX__",
                    &format!("{}{}", self.args.uri_prefix, self.assets_prefix),
                )
                .replace("__INDEX_DATA__", &index_data)
        };
        res.headers_mut()
            .typed_insert(ContentLength(output.len() as u64));
        res.headers_mut()
            .typed_insert(CacheControl::new().with_no_cache());
        res.headers_mut().insert(
            "x-content-type-options",
            HeaderValue::from_static("nosniff"),
        );
        if head_only {
            return Ok(());
        }
        *res.body_mut() = body_full(output);
        Ok(())
    }

    fn auth_reject(&self, res: &mut Response) -> Result<()> {
        set_webdav_headers(res);

        www_authenticate(res, &self.args)?;
        *res.status_mut() = StatusCode::UNAUTHORIZED;
        Ok(())
    }

    async fn guard_root_contained(&self, path: &Path) -> bool {
        if self.args.allow_symlink {
            return false;
        }
        let mut check_path = path.to_path_buf();
        while !fs::try_exists(&check_path).await.unwrap_or_default() {
            match check_path.parent() {
                Some(parent) => check_path = parent.to_path_buf(),
                None => return true,
            }
        }
        !self.is_root_contained(check_path.as_path()).await
    }

    async fn is_root_contained(&self, path: &Path) -> bool {
        fs::canonicalize(path)
            .await
            .ok()
            .map(|v| v.starts_with(&self.args.serve_path))
            .unwrap_or_default()
    }

    async fn is_directory_auth_metadata(&self, path: &Path) -> bool {
        let Some(directory_auth) = &self.directory_auth else {
            return false;
        };
        let metadata_path = directory_auth.metadata_path();
        if path == metadata_path {
            return true;
        }
        if let (Ok(path), Ok(metadata_path)) = (
            fs::canonicalize(path).await,
            fs::canonicalize(metadata_path).await,
        ) {
            if path == metadata_path {
                return true;
            }
        }
        let Some(parent) = metadata_path.parent() else {
            return false;
        };
        let Some(filename) = metadata_path.file_name().and_then(|name| name.to_str()) else {
            return false;
        };
        path.parent() == Some(parent)
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with(&format!("{filename}.tmp-")))
                .unwrap_or(false)
    }

    async fn directory_password_for_path(&self, path: &Path) -> Result<Option<String>> {
        let Some(directory_auth) = &self.directory_auth else {
            return Ok(None);
        };
        let relative_path = normalize_path(path.strip_prefix(&self.args.serve_path)?);
        if relative_path.is_empty() {
            return Ok(None);
        }
        Ok(Some(
            directory_auth
                .password_for_directory(&relative_path)
                .await?,
        ))
    }

    fn set_directory_password_cookie(
        &self,
        directory: &str,
        password: &str,
        res: &mut Response,
    ) -> Result<()> {
        let mut path = format!("{}{}", self.args.uri_prefix, encode_uri(directory));
        if !path.ends_with('/') {
            path.push('/');
        }
        let payload = URL_SAFE_NO_PAD.encode(format!("{directory}\0{password}"));
        let value =
            format!("{DIRECTORY_PASSWORD_COOKIE}={payload}; Path={path}; HttpOnly; SameSite=Lax");
        res.headers_mut()
            .append(SET_COOKIE, HeaderValue::from_str(&value)?);
        Ok(())
    }

    fn relative_path(&self, path: &Path) -> Result<String> {
        Ok(normalize_path(path.strip_prefix(&self.args.serve_path)?))
    }

    async fn ensure_directory_auth_for_parent(&self, path: &Path) -> Result<()> {
        let Some(directory_auth) = &self.directory_auth else {
            return Ok(());
        };
        let Some(parent) = path.parent() else {
            return Ok(());
        };
        let relative_path = self.relative_path(parent)?;
        if !relative_path.is_empty() {
            directory_auth.ensure_directory_tree(&relative_path).await?;
        }
        Ok(())
    }

    async fn extract_dest(&self, req: &Request, res: &mut Response) -> Result<Option<PathBuf>> {
        let headers = req.headers();
        let destination = match self.extract_destination_header(headers) {
            Some(destination) => destination,
            None => {
                status_bad_request(res, "Invalid Destination");
                return Ok(None);
            }
        };
        let dest_path = match self.resolve_path(destination.path()) {
            Some(dest) => dest,
            None => {
                status_bad_request(res, "Invalid Destination");
                return Ok(None);
            }
        };
        let destination_password = destination.query().and_then(|query| {
            form_urlencoded::parse(query.as_bytes())
                .find(|(key, _)| key == DIRECTORY_PASSWORD_QUERY)
                .map(|(_, value)| value.to_string())
        });

        let authorization = headers.get(AUTHORIZATION);
        let guard = self
            .args
            .auth
            .guard(&dest_path, req.method(), authorization, None, false);

        let dest = match self.join_path(&dest_path) {
            Some(dest) => dest,
            None => {
                *res.status_mut() = StatusCode::BAD_REQUEST;
                return Ok(None);
            }
        };

        let granted = if let Some(directory_auth) = &self.directory_auth {
            match guard {
                (Some(_), Some(_)) => true,
                (None, _) if authorization.is_none() => {
                    let expected = directory_auth
                        .password_for_target(&dest_path, &dest)
                        .await?;
                    expected
                        .as_deref()
                        .map(|expected| password_matches(expected, destination_password.as_ref()))
                        .unwrap_or(false)
                }
                _ => false,
            }
        } else {
            guard.1.is_some()
        };
        if !granted {
            status_forbid(res);
            return Ok(None);
        }

        Ok(Some(dest))
    }

    fn extract_destination_header(&self, headers: &HeaderMap<HeaderValue>) -> Option<Uri> {
        let dest = headers.get("Destination")?.to_str().ok()?;
        dest.parse().ok()
    }

    fn resolve_path(&self, path: &str) -> Option<String> {
        let path = decode_uri(path)?;
        let path = path.trim_matches('/');
        let mut parts = vec![];
        for comp in Path::new(path).components() {
            if let Component::Normal(v) = comp {
                let v = v.to_string_lossy();
                if cfg!(windows) {
                    let chars: Vec<char> = v.chars().collect();
                    if chars.len() == 2 && chars[1] == ':' && chars[0].is_ascii_alphabetic() {
                        return None;
                    }
                }
                parts.push(v);
            } else {
                return None;
            }
        }
        let new_path = parts.join("/");
        let path_prefix = self.args.path_prefix.as_str();
        if path_prefix.is_empty() {
            return Some(new_path);
        }
        new_path
            .strip_prefix(path_prefix.trim_start_matches('/'))
            .map(|v| v.trim_matches('/').to_string())
    }

    fn join_path(&self, path: &str) -> Option<PathBuf> {
        if path.is_empty() {
            return Some(self.args.serve_path.clone());
        }
        let path = if cfg!(windows) {
            path.replace('/', "\\")
        } else {
            path.to_string()
        };
        Some(self.args.serve_path.join(path))
    }

    async fn list_dir(
        &self,
        entry_path: &Path,
        base_path: &Path,
        access_paths: AccessPaths,
    ) -> Result<Vec<PathItem>> {
        let mut paths: Vec<PathItem> = vec![];
        if access_paths.perm().indexonly() {
            for name in access_paths.child_names() {
                let entry_path = entry_path.join(name);
                self.add_pathitem(&mut paths, base_path, &entry_path).await;
            }
        } else {
            let mut rd = fs::read_dir(entry_path).await?;
            while let Ok(Some(entry)) = rd.next_entry().await {
                let entry_path = entry.path();
                self.add_pathitem(&mut paths, base_path, &entry_path).await;
            }
        }
        Ok(paths)
    }

    async fn add_pathitem(&self, paths: &mut Vec<PathItem>, base_path: &Path, entry_path: &Path) {
        if self.is_directory_auth_metadata(entry_path).await {
            return;
        }
        let base_name = get_file_name(entry_path);
        if let Ok(Some(item)) = self.to_pathitem(entry_path, base_path).await {
            if is_hidden(&self.args.hidden, base_name, item.is_dir()) {
                return;
            }
            paths.push(item);
        }
    }

    async fn to_pathitem<P: AsRef<Path>>(&self, path: P, base_path: P) -> Result<Option<PathItem>> {
        let path = path.as_ref();
        let (meta, meta2) = tokio::join!(fs::metadata(&path), fs::symlink_metadata(&path));
        let (meta, meta2) = (meta?, meta2?);
        let is_symlink = meta2.is_symlink();
        if !self.args.allow_symlink && is_symlink && !self.is_root_contained(path).await {
            return Ok(None);
        }
        let is_dir = meta.is_dir();
        let path_type = match (is_symlink, is_dir) {
            (true, true) => PathType::SymlinkDir,
            (false, true) => PathType::Dir,
            (true, false) => PathType::SymlinkFile,
            (false, false) => PathType::File,
        };
        let mtime = match meta.modified().ok().or_else(|| meta.created().ok()) {
            Some(v) => to_timestamp(&v),
            None => 0,
        };
        let size = match path_type {
            PathType::Dir | PathType::SymlinkDir => {
                let mut count = 0;
                let mut entries = tokio::fs::read_dir(&path).await?;
                while let Some(entry) = entries.next_entry().await? {
                    let entry_path = entry.path();
                    let base_name = get_file_name(&entry_path);
                    let is_dir = entry
                        .file_type()
                        .await
                        .map(|v| v.is_dir())
                        .unwrap_or_default();
                    if is_hidden(&self.args.hidden, base_name, is_dir) {
                        continue;
                    }
                    count += 1;
                    if count >= MAX_SUBPATHS_COUNT {
                        break;
                    }
                }
                count
            }
            PathType::File | PathType::SymlinkFile => meta.len(),
        };
        let rel_path = path.strip_prefix(base_path)?;
        let name = normalize_path(rel_path);
        let directory_password = if let Some(directory_auth) = &self.directory_auth {
            let relative_path = normalize_path(path.strip_prefix(&self.args.serve_path)?);
            directory_auth
                .password_for_target(&relative_path, path)
                .await?
        } else {
            None
        };
        Ok(Some(PathItem {
            path_type,
            name,
            mtime,
            size,
            directory_password,
        }))
    }
}

#[derive(Debug, Serialize, PartialEq)]
pub enum DataKind {
    Index,
    Edit,
    View,
}

#[derive(Debug, Serialize)]
pub struct IndexData {
    pub href: String,
    pub kind: DataKind,
    pub uri_prefix: String,
    pub allow_upload: bool,
    pub allow_delete: bool,
    pub allow_search: bool,
    pub allow_archive: bool,
    pub dir_exists: bool,
    pub auth: bool,
    pub user: Option<String>,
    pub directory_auth: bool,
    pub directory_password: Option<String>,
    pub paths: Vec<PathItem>,
}

#[derive(Debug, Serialize, Eq, PartialEq, Ord, PartialOrd)]
pub struct PathItem {
    pub path_type: PathType,
    pub name: String,
    pub mtime: u64,
    pub size: u64,
    pub directory_password: Option<String>,
}

impl PathItem {
    pub fn is_dir(&self) -> bool {
        self.path_type == PathType::Dir || self.path_type == PathType::SymlinkDir
    }

    pub fn to_dav_xml(&self, prefix: &str) -> String {
        let mtime = match Utc.timestamp_millis_opt(self.mtime as i64) {
            LocalResult::Single(v) => format!("{}", v.format("%a, %d %b %Y %H:%M:%S GMT")),
            _ => String::new(),
        };
        let mut href = encode_uri(&format!("{}{}", prefix, &self.name));
        if self.is_dir() && !href.ends_with('/') {
            href.push('/');
        }
        let displayname = escape_str_pcdata(self.base_name());
        match self.path_type {
            PathType::Dir | PathType::SymlinkDir => format!(
                r#"<D:response>
<D:href>{href}</D:href>
<D:propstat>
<D:prop>
<D:displayname>{displayname}</D:displayname>
<D:getlastmodified>{mtime}</D:getlastmodified>
<D:resourcetype><D:collection/></D:resourcetype>
</D:prop>
<D:status>HTTP/1.1 200 OK</D:status>
</D:propstat>
</D:response>"#
            ),
            PathType::File | PathType::SymlinkFile => format!(
                r#"<D:response>
<D:href>{href}</D:href>
<D:propstat>
<D:prop>
<D:displayname>{displayname}</D:displayname>
<D:getcontentlength>{}</D:getcontentlength>
<D:getlastmodified>{mtime}</D:getlastmodified>
<D:resourcetype></D:resourcetype>
</D:prop>
<D:status>HTTP/1.1 200 OK</D:status>
</D:propstat>
</D:response>"#,
                self.size
            ),
        }
    }

    pub fn base_name(&self) -> &str {
        self.name.split('/').next_back().unwrap_or_default()
    }

    pub fn sort_by_name(&self, other: &Self) -> Ordering {
        match self.path_type.cmp(&other.path_type) {
            Ordering::Equal => {
                alphanumeric_sort::compare_str(self.name.to_lowercase(), other.name.to_lowercase())
            }
            v => v,
        }
    }

    pub fn sort_by_mtime(&self, other: &Self) -> Ordering {
        match self.path_type.cmp(&other.path_type) {
            Ordering::Equal => self.mtime.cmp(&other.mtime),
            v => v,
        }
    }

    pub fn sort_by_size(&self, other: &Self) -> Ordering {
        match self.path_type.cmp(&other.path_type) {
            Ordering::Equal => self.size.cmp(&other.size),
            v => v,
        }
    }
}

#[derive(Debug, Serialize, Clone, Copy, Eq, PartialEq)]
pub enum PathType {
    Dir,
    SymlinkDir,
    File,
    SymlinkFile,
}

impl PathType {
    pub fn is_dir(&self) -> bool {
        matches!(self, Self::Dir | Self::SymlinkDir)
    }
}

impl Ord for PathType {
    fn cmp(&self, other: &Self) -> Ordering {
        let to_value = |t: &Self| -> u8 {
            if matches!(t, Self::Dir | Self::SymlinkDir) {
                0
            } else {
                1
            }
        };
        to_value(self).cmp(&to_value(other))
    }
}
impl PartialOrd for PathType {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Serialize)]
struct EditData {
    href: String,
    kind: DataKind,
    uri_prefix: String,
    allow_upload: bool,
    allow_delete: bool,
    auth: bool,
    user: Option<String>,
    editable: bool,
    directory_auth: bool,
    directory_password: Option<String>,
}

fn to_timestamp(time: &SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn normalize_path<P: AsRef<Path>>(path: P) -> String {
    let path = path.as_ref().to_str().unwrap_or_default();
    if cfg!(windows) {
        path.replace('\\', "/")
    } else {
        path.to_string()
    }
}

fn assets_revision(assets_path: Option<&Path>) -> Result<String> {
    let mut hasher = Sha256::new();
    match assets_path {
        Some(path) => {
            for name in ["index.html", "index.js", "index.css", "favicon.ico"] {
                hasher.update(name.as_bytes());
                match std::fs::read(path.join(name)) {
                    Ok(content) => {
                        hasher.update([1]);
                        hasher.update((content.len() as u64).to_be_bytes());
                        hasher.update(content);
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        hasher.update([0]);
                    }
                    Err(err) => return Err(err.into()),
                }
            }
        }
        None => {
            for (name, content) in [
                ("index.html", INDEX_HTML.as_bytes()),
                ("index.js", INDEX_JS.as_bytes()),
                ("index.css", INDEX_CSS.as_bytes()),
                ("favicon.ico", FAVICON_ICO),
            ] {
                hasher.update(name.as_bytes());
                hasher.update([1]);
                hasher.update((content.len() as u64).to_be_bytes());
                hasher.update(content);
            }
        }
    }
    Ok(hex::encode(&hasher.finalize()[..8]))
}

async fn ensure_path_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if fs::symlink_metadata(parent).await.is_err() {
            fs::create_dir_all(&parent).await?;
        }
    }
    Ok(())
}

fn add_cors(res: &mut Response) {
    res.headers_mut()
        .typed_insert(AccessControlAllowOrigin::ANY);
    res.headers_mut()
        .typed_insert(AccessControlAllowCredentials);
    res.headers_mut().insert(
        "Access-Control-Allow-Methods",
        HeaderValue::from_static("*"),
    );
    res.headers_mut().insert(
        "Access-Control-Allow-Headers",
        HeaderValue::from_static("Authorization,*"),
    );
    res.headers_mut().insert(
        "Access-Control-Expose-Headers",
        HeaderValue::from_static("Authorization,*"),
    );
}

fn res_multistatus(res: &mut Response, content: &str) {
    *res.status_mut() = StatusCode::MULTI_STATUS;
    res.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("application/xml; charset=utf-8"),
    );
    *res.body_mut() = body_full(format!(
        r#"<?xml version="1.0" encoding="utf-8" ?>
<D:multistatus xmlns:D="DAV:">
{content}
</D:multistatus>"#,
    ));
}

async fn zip_dir<W: AsyncWrite + Unpin>(
    writer: &mut W,
    dir: &Path,
    access_paths: AccessPaths,
    hidden: &[String],
    compression: Compression,
    follow_symlinks: bool,
    serve_path: PathBuf,
    excluded_path: Option<PathBuf>,
    running: Arc<AtomicBool>,
) -> Result<()> {
    let hidden = Arc::new(hidden.to_vec());
    let zip_paths = tokio::task::spawn(collect_dir_entries(
        access_paths,
        running,
        dir.to_path_buf(),
        hidden,
        follow_symlinks,
        serve_path,
        excluded_path,
        move |x| x.path().symlink_metadata().is_ok() && x.file_type().is_file(),
    ))
    .await?;
    let mut zip = ZipWriter::new(&mut *writer).with_level(compression);
    for zip_path in zip_paths.into_iter() {
        let filename = match zip_path
            .strip_prefix(dir)
            .ok()
            .and_then(|v| v.to_str())
            .map(|v| v.replace(MAIN_SEPARATOR, "/"))
        {
            Some(v) => v,
            None => continue,
        };
        let options = WriterOptions::from_path(&zip_path).await?;
        let mut file = File::open(&zip_path).await?;
        let mut entry = zip.append_file(&filename, options).await?;
        io::copy(&mut file, &mut entry).await?;
        entry.close().await?;
    }
    zip.finalize().await?;
    Ok(())
}

fn extract_cache_headers(meta: &Metadata) -> Option<(ETag, LastModified)> {
    let mtime = meta.modified().ok().or_else(|| meta.created().ok())?;
    let timestamp = to_timestamp(&mtime);
    let size = meta.len();
    let etag = format!(r#""{timestamp}-{size}""#).parse::<ETag>().ok()?;
    let last_modified = LastModified::from(mtime);
    Some((etag, last_modified))
}

fn status_forbid(res: &mut Response) {
    *res.status_mut() = StatusCode::FORBIDDEN;
    *res.body_mut() = body_full("Forbidden");
}

fn status_directory_password_required(res: &mut Response) {
    *res.status_mut() = StatusCode::UNAUTHORIZED;
    res.headers_mut().insert(
        "x-dufs-directory-password",
        HeaderValue::from_static("required"),
    );
    *res.body_mut() = body_full("Directory password required");
}

fn status_not_found(res: &mut Response) {
    *res.status_mut() = StatusCode::NOT_FOUND;
    *res.body_mut() = body_full("Not Found");
}

fn status_no_content(res: &mut Response) {
    *res.status_mut() = StatusCode::NO_CONTENT;
}

fn status_bad_request(res: &mut Response, body: &str) {
    *res.status_mut() = StatusCode::BAD_REQUEST;
    if !body.is_empty() {
        *res.body_mut() = body_full(body.to_string());
    }
}

fn set_content_disposition(res: &mut Response, inline: bool, filename: &str) -> Result<()> {
    let kind = if inline { "inline" } else { "attachment" };
    let filename: String = filename
        .chars()
        .map(|ch| {
            if ch.is_ascii_control() && ch != '\t' {
                ' '
            } else {
                ch
            }
        })
        .collect();
    let value = if filename.is_ascii() {
        HeaderValue::from_str(&format!("{kind}; filename=\"{filename}\"",))?
    } else {
        HeaderValue::from_str(&format!(
            "{kind}; filename=\"{}\"; filename*=UTF-8''{}",
            filename,
            encode_uri(&filename),
        ))?
    };
    res.headers_mut().insert(CONTENT_DISPOSITION, value);
    Ok(())
}

fn is_hidden(hidden: &[String], file_name: &str, is_dir: bool) -> bool {
    hidden.iter().any(|v| {
        if is_dir {
            if let Some(x) = v.strip_suffix('/') {
                return glob(x, file_name);
            }
        }
        glob(v, file_name)
    })
}

fn set_webdav_headers(res: &mut Response) {
    res.headers_mut().insert(
        "Allow",
        HeaderValue::from_static(
            "GET,HEAD,PUT,OPTIONS,DELETE,PATCH,PROPFIND,COPY,MOVE,CHECKAUTH,LOGOUT",
        ),
    );
    res.headers_mut()
        .insert("DAV", HeaderValue::from_static("1, 2, 3"));
}

async fn get_content_type(path: &Path) -> Result<String> {
    let mut buffer: Vec<u8> = vec![];
    fs::File::open(path)
        .await?
        .take(1024)
        .read_to_end(&mut buffer)
        .await?;
    let mime = mime_guess::from_path(path).first();
    let is_text = content_inspector::inspect(&buffer).is_text();
    let content_type = if is_text {
        let mut detector = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Allow);
        detector.feed(&buffer, buffer.len() < 1024);
        let enc = detector.guess(None, chardetng::Utf8Detection::Allow);
        let charset = format!("; charset={}", enc.name());
        match mime {
            Some(m) => format!("{m}{charset}"),
            None => format!("text/plain{charset}"),
        }
    } else {
        match mime {
            Some(m) => m.to_string(),
            None => "application/octet-stream".into(),
        }
    };
    Ok(content_type)
}

fn parse_upload_offset(headers: &HeaderMap<HeaderValue>, size: u64) -> Result<Option<u64>> {
    let value = match headers.get("x-update-range") {
        Some(v) => v,
        None => return Ok(None),
    };
    let err = || anyhow!("Invalid X-Update-Range Header");
    let value = value.to_str().map_err(|_| err())?;
    if value == "append" {
        return Ok(Some(size));
    }
    // use the first range
    let ranges = parse_range(value, size).ok_or_else(err)?;
    let (start, _) = ranges.first().ok_or_else(err)?;
    Ok(Some(*start))
}

async fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];

    loop {
        let bytes_read = file.read(&mut buffer).await?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }

    let result = hasher.finalize();
    Ok(hex::encode(result))
}

fn has_query_flag(query_params: &HashMap<String, String>, name: &str) -> bool {
    query_params
        .get(name)
        .map(|v| v.is_empty())
        .unwrap_or_default()
}

fn directory_password_cookies(headers: &HeaderMap<HeaderValue>) -> Vec<(String, String)> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|header| header.to_str().ok())
        .flat_map(|header| header.split(';'))
        .filter_map(|item| item.trim().split_once('='))
        .filter(|(name, _)| *name == DIRECTORY_PASSWORD_COOKIE)
        .filter_map(|(_, value)| URL_SAFE_NO_PAD.decode(value).ok())
        .filter_map(|value| String::from_utf8(value).ok())
        .filter_map(|value| {
            let (scope, password) = value.split_once('\0')?;
            Some((scope.to_string(), password.to_string()))
        })
        .collect()
}

fn is_directory_share_readonly_method(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) || method.as_str() == "PROPFIND"
}

async fn collect_dir_entries<F>(
    access_paths: AccessPaths,
    running: Arc<AtomicBool>,
    path: PathBuf,
    hidden: Arc<Vec<String>>,
    follow_symlinks: bool,
    serve_path: PathBuf,
    excluded_path: Option<PathBuf>,
    include_entry: F,
) -> Vec<PathBuf>
where
    F: Fn(&DirEntry) -> bool,
{
    let mut paths: Vec<PathBuf> = vec![];
    let excluded_path = match excluded_path {
        Some(path) => fs::canonicalize(path).await.ok(),
        None => None,
    };
    for dir in access_paths.entry_paths(&path) {
        let mut it = WalkDir::new(&dir).follow_links(true).into_iter();
        it.next();
        while let Some(entry) = it.next() {
            if !running.load(atomic::Ordering::SeqCst) {
                break;
            }
            let entry = match entry {
                Ok(v) => v,
                Err(_) => continue,
            };
            let entry_path = entry.path();
            let base_name = get_file_name(entry_path);
            let is_dir = entry.file_type().is_dir();
            if is_hidden(&hidden, base_name, is_dir) {
                if is_dir {
                    it.skip_current_dir();
                }
                continue;
            }

            let canonical_path = if excluded_path.is_some() || !follow_symlinks {
                fs::canonicalize(entry_path).await.ok()
            } else {
                None
            };
            if canonical_path
                .as_ref()
                .zip(excluded_path.as_ref())
                .map(|(path, excluded)| is_excluded_path(path, excluded))
                .unwrap_or(false)
            {
                if is_dir {
                    it.skip_current_dir();
                }
                continue;
            }

            if !follow_symlinks
                && !canonical_path
                    .map(|v| v.starts_with(&serve_path))
                    .unwrap_or_default()
            {
                // We walked outside the server's root. This could only have
                // happened if we followed a symlink, and hence we only allow it
                // if allow_symlink is enabled, otherwise we skip this entry.
                if is_dir {
                    it.skip_current_dir();
                }
                continue;
            }
            if !include_entry(&entry) {
                continue;
            }
            paths.push(entry_path.to_path_buf());
        }
    }
    paths
}

fn is_excluded_path(path: &Path, excluded: &Path) -> bool {
    if path == excluded {
        return true;
    }
    let Some(excluded_name) = excluded.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    path.parent() == excluded.parent()
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.starts_with(&format!("{excluded_name}.tmp-")))
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_fs::prelude::*;

    #[test]
    fn assets_revision_changes_with_content() {
        let assets = assert_fs::TempDir::new().unwrap();
        assets.child("index.html").write_str("index").unwrap();
        assets.child("index.js").write_str("old").unwrap();
        let old_revision = assets_revision(Some(assets.path())).unwrap();

        assets.child("index.js").write_str("new").unwrap();
        let new_revision = assets_revision(Some(assets.path())).unwrap();

        assert_ne!(old_revision, new_revision);
    }
}
