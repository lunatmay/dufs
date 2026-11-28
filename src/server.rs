#![allow(clippy::too_many_arguments)]

mod dav_common;
mod dav_search;
mod dav_sync;
mod http_digest;
mod http_encoding;
use dav_common::DavPrefer;

use crate::auth::{www_authenticate, AccessControl, AccessPaths, AccessPerm, EntryAccess};
use crate::error::DufsErrorCode;
use crate::http_utils::{body_full, LengthLimitedStream};
use crate::noscript::{detect_noscript, generate_noscript_html};
use crate::utils::{
    decode_uri, encode_uri, get_file_name, glob, join_within, parse_range, try_get_file_name,
};
use crate::Args;

use anyhow::{anyhow, Result};
use async_deflate_zip::{CompressionLevel, EntryOptions, ZipWriter};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use bytes::Bytes;
use futures_util::TryStreamExt;
use headers::{
    AcceptRanges, AccessControlAllowOrigin, CacheControl, ContentLength, ContentType, ETag,
    HeaderMap, HeaderMapExt, IfMatch, IfModifiedSince, IfNoneMatch, IfRange, IfUnmodifiedSince,
    LastModified,
};
use http_body_util::{combinators::BoxBody, BodyExt, Limited, StreamBody};
use hyper::body::Frame;
use hyper::{
    body::Incoming,
    header::{
        HeaderValue, AUTHORIZATION, CONNECTION, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE,
        CONTENT_TYPE, RANGE,
    },
    Method, StatusCode, Uri,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::fs::Metadata;
use std::io::SeekFrom;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf, MAIN_SEPARATOR};
use std::sync::atomic::{self, AtomicBool};
use std::sync::{Arc, Mutex, Weak};
use std::time::SystemTime;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};
use tokio::{fs, io};

use tokio_util::io::ReaderStream;
use uuid::Uuid;
use walkdir::{DirEntry, WalkDir};
use xml::escape::{escape_str_attribute, escape_str_pcdata};
use xml::name::OwnedName;
use xml::reader::{EventReader, ParserConfig, XmlEvent};

pub type Request = hyper::Request<Incoming>;
pub type Response = hyper::Response<BoxBody<Bytes, anyhow::Error>>;

const INDEX_HTML: &str = include_str!("../assets/index.html");
const INDEX_CSS: &str = include_str!("../assets/index.css");
const INDEX_JS: &str = include_str!("../assets/index.js");
const FAVICON_ICO: &[u8] = include_bytes!("../assets/favicon.ico");
const INDEX_NAME: &str = "index.html";
const BUF_SIZE: usize = 65536;
const EDITABLE_TEXT_MAX_SIZE: u64 = 4194304; // 4M
const UPLOAD_TEMP_PREFIX: &str = ".dufs-upload-";
const HEALTH_CHECK_PATH: &str = "__dufs__/health";
pub const MAX_SUBPATHS_COUNT: u64 = 1000;

pub struct Server {
    args: Args,
    assets_prefix: String,
    html: Cow<'static, str>,
    single_file_req_paths: Vec<String>,
    running: Arc<AtomicBool>,
    write_locks: Mutex<HashMap<PathBuf, Weak<RwLock<()>>>>,
    sync_history: dav_sync::SyncHistory,
}

impl Server {
    pub fn init(args: Args, running: Arc<AtomicBool>) -> Result<Self> {
        let assets_prefix = format!("__dufs_v{}__/", env!("CARGO_PKG_VERSION"));
        let single_file_req_paths = if args.path_is_file {
            vec![
                args.uri_prefix.to_string(),
                args.uri_prefix
                    .strip_suffix('/')
                    .unwrap_or(&args.uri_prefix)
                    .to_string(),
                encode_uri(&format!(
                    "{}{}",
                    args.uri_prefix,
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
            running,
            single_file_req_paths,
            assets_prefix,
            html,
            write_locks: Mutex::default(),
            sync_history: dav_sync::SyncHistory::default(),
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
        let vary_prefer = is_write_method(req.method())
            || matches!(req.method().as_str(), "PROPFIND" | "PROPPATCH" | "REPORT" | "SEARCH");
        let mut http_log_data = self.args.http_logger.data(&req);
        if let Some(addr) = addr {
            http_log_data.insert("remote_addr".to_string(), addr.ip().to_string());
        }

        let result = if is_write_method(req.method()) {
            // A disconnected client must not release a lock while an async filesystem
            // operation is still completing. Dropping the JoinHandle leaves it running.
            tokio::spawn(self.clone().handle(req))
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result)
        } else {
            self.clone().handle(req).await
        };
        let mut res = match result {
            Ok(res) => {
                http_log_data.insert("status".to_string(), res.status().as_u16().to_string());
                if !uri.path().starts_with(assets_prefix) {
                    self.args.http_logger.log(&http_log_data, None);
                }
                res
            }
            Err(err) => {
                let mut res = Response::default();
                let status = if let Some(precondition) = dav_common::storage_precondition(&err) {
                    res.headers_mut().insert(
                        CONTENT_TYPE,
                        HeaderValue::from_static("application/xml; charset=utf-8"),
                    );
                    *res.body_mut() = body_full(format!(
                        "<D:error xmlns:D=\"DAV:\"><D:{precondition}/></D:error>"
                    ));
                    StatusCode::INSUFFICIENT_STORAGE
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                };
                *res.status_mut() = status;
                http_log_data.insert("status".to_string(), status.as_u16().to_string());
                self.args
                    .http_logger
                    .log(&http_log_data, Some(err.to_string()));
                res
            }
        };

        if vary_prefer {
            res.headers_mut().append("Vary", HeaderValue::from_static("Prefer"));
        }
        if enable_cors {
            add_cors(&mut res);
        }
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
                DufsErrorCode::InvalidPath.respond(&mut res);
                return Ok(res);
            }
        };

        if (method == Method::GET || method == Method::HEAD)
            && self
                .handle_internal(&relative_path, method == Method::HEAD, headers, &mut res)
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
        let request_target = req.uri().to_string();

        let query = req.uri().query().unwrap_or_default();
        let mut query_params: HashMap<String, String> = form_urlencoded::parse(query.as_bytes())
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        let dav_if = match headers.get("If") {
            None => Ok(vec![]),
            Some(value) if headers.get_all("If").iter().count() == 1 => value
                .to_str()
                .map_err(anyhow::Error::from)
                .and_then(parse_dav_if),
            Some(_) => Err(anyhow!("Duplicate If header")),
        };

        // Hold source, destination, and tagged condition paths through filesystem changes.
        let _write_guards = if is_write_method(&method) || method.as_str() == "REPORT" {
            let mut paths = vec![];
            if matches!(method.as_str(), "MOVE" | "COPY") {
                paths.extend(
                    self.extract_destination_header(headers)
                        .and_then(|v| self.resolve_path(&v))
                        .and_then(|v| self.join_path(&v)),
                );
            }
            paths.extend(
                dav_if
                    .as_ref()
                    .ok()
                    .into_iter()
                    .flatten()
                    .filter_map(|group| group.resource.as_deref())
                    .filter_map(|uri| dav_uri_path(uri, headers))
                    .filter_map(|path| self.resolve_path(&path))
                    .filter_map(|path| self.join_path(&path)),
            );
            match self.join_path(&relative_path) {
                Some(path) => Some(self.lock_write_paths(&path, &paths).await),
                None => None,
            }
        } else {
            None
        };

        let (user, access_paths, via_token, stale) = self
            .args
            .auth
            .guard(
                &relative_path,
                &method,
                &request_target,
                authorization,
                query_params.get("token"),
                is_microsoft_webdav,
                &self.args.serve_path,
            )
            .await;

        let (user, access_paths) = match (user, access_paths) {
            (None, None) => {
                self.auth_reject(&mut res, stale)?;
                return Ok(res);
            }
            (Some(_), None) => {
                DufsErrorCode::Forbidden.respond(&mut res);
                return Ok(res);
            }
            (x, Some(y)) => (x, y),
        };

        if headers.get_all("Content-Encoding").iter().any(|v| {
            !v.to_str().is_ok_and(|v| v.split(',').all(|coding| coding.trim().eq_ignore_ascii_case("identity")))
        }) {
            *res.status_mut() = StatusCode::UNSUPPORTED_MEDIA_TYPE;
            res.headers_mut().insert("Accept-Encoding", HeaderValue::from_static("identity"));
            return Ok(res);
        }

        let dav_if = match dav_if {
            Ok(parsed) => parsed,
            Err(_) => {
                *res.status_mut() = StatusCode::BAD_REQUEST;
                return Ok(res);
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
                        self.auth_reject(&mut res, false)?
                    } else {
                        *res.body_mut() = body_full("");
                    }
                }
            }
            return Ok(res);
        } else if method.as_str() == "LOGOUT" {
            self.auth_reject(&mut res, false)?;
            return Ok(res);
        }

        let head_only = method == Method::HEAD || method == Method::OPTIONS;

        if has_query_flag(&query_params, "tokengen") {
            // A share token must not be able to mint another token, otherwise a
            // short-lived link could be renewed forever regardless of expiry.
            if via_token {
                self.auth_reject(&mut res, false)?;
                return Ok(res);
            }
            self.handle_tokengen(&relative_path, user, head_only, &mut res)
                .await?;
            return Ok(res);
        }

        if self.args.path_is_file {
            if self
                .single_file_req_paths
                .iter()
                .any(|v| v.as_str() == req_path)
            {
                if matches!(method, Method::GET | Method::HEAD) {
                    self.handle_send_file(&self.args.serve_path, headers, head_only, &mut res)
                        .await?;
                } else {
                    res.headers_mut().insert("Allow", HeaderValue::from_static("GET, HEAD, OPTIONS, CHECKAUTH, LOGOUT"));
                    if method != Method::OPTIONS {
                        DufsErrorCode::MethodNotAllowed.respond(&mut res);
                    }
                }
            } else {
                self.handle_not_found(&query_params, head_only, &mut res)
                    .await?;
            }
            return Ok(res);
        }
        let path = match self.join_path(&relative_path) {
            Some(v) => v,
            None => {
                DufsErrorCode::Forbidden.respond(&mut res);
                return Ok(res);
            }
        };

        let path = path.as_path();

        let meta = match fs::metadata(path).await {
            Ok(meta) => Some(meta),
            Err(err)
                if is_write_method(&method)
                    && !matches!(
                        err.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                    ) =>
            {
                return Err(err.into());
            }
            Err(_) => None,
        };
        let (is_miss, is_dir, is_file, size) = match &meta {
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
            self.handle_not_found(&query_params, head_only, &mut res)
                .await?;
            return Ok(res);
        }

        let destination = if matches!(method.as_str(), "COPY" | "MOVE") && allow_upload && !is_miss
        {
            match self.extract_dest(&req, &mut res).await {
                Some(dest) => Some(dest),
                None => return Ok(res),
            }
        } else {
            None
        };

        let validators = if !dav_if.is_empty()
            || [
                "If-Match",
                "If-None-Match",
                "If-Unmodified-Since",
                "If-Modified-Since",
            ]
            .iter()
            .any(|name| headers.contains_key(*name))
        {
            self.dav_validators(
                path,
                meta.as_ref(),
                user.clone(),
                Some(access_paths.clone()),
            )
            .await?
        } else {
            None
        };

        let prefer = DavPrefer::parse(headers);
        let mut response_headers = HeaderMap::new();
        if prefer.representation {
            for value in headers.get_all("Accept-Encoding") {
                response_headers.append("Accept-Encoding", value.clone());
            }
        }
        if !dav_if.is_empty() {
            let mut matched = false;
            for group in dav_if {
                let (etag, token) = if let Some(resource) = &group.resource {
                    let relative =
                        match dav_uri_path(resource, headers).and_then(|v| self.resolve_path(&v)) {
                            Some(relative) => relative,
                            None => {
                                *res.status_mut() = StatusCode::BAD_REQUEST;
                                return Ok(res);
                            }
                        };
                    if via_token && relative != relative_path {
                        DufsErrorCode::Forbidden.respond(&mut res);
                        return Ok(res);
                    }
                    let target_access = self
                        .args
                        .auth
                        .access_for(
                            user.as_deref(),
                            &relative,
                            &Method::GET,
                            &self.args.serve_path,
                        )
                        .await;
                    if target_access.is_none() {
                        DufsErrorCode::Forbidden.respond(&mut res);
                        return Ok(res);
                    }
                    let Some(target) = self.join_path(&relative) else {
                        DufsErrorCode::Forbidden.respond(&mut res);
                        return Ok(res);
                    };
                    if self.guard_root_contained(&target).await {
                        DufsErrorCode::Forbidden.respond(&mut res);
                        return Ok(res);
                    }
                    let target_meta = fs::metadata(&target).await.ok();
                    let token =
                        if group.needs_sync() && target_meta.as_ref().is_some_and(|m| m.is_dir()) {
                            self.sync_collection_token(
                                &target,
                                user.clone(),
                                target_access.clone().unwrap(),
                                via_token,
                            )
                            .await
                            .ok()
                        } else {
                            None
                        };
                    let etag = self
                        .dav_validators(&target, target_meta.as_ref(), user.clone(), target_access)
                        .await?
                        .map(|(etag, _)| etag);
                    (etag, token)
                } else {
                    let token = if group.needs_sync() && is_dir {
                        self.sync_collection_token(
                            path,
                            user.clone(),
                            access_paths.clone(),
                            via_token,
                        )
                        .await
                        .ok()
                    } else {
                        None
                    };
                    (validators.as_ref().map(|(etag, _)| etag.clone()), token)
                };
                matched |= group.matches(etag.as_ref(), token.as_deref());
            }
            if !matched {
                DufsErrorCode::PreconditionFailed.respond(&mut res);
                if prefer.representation
                    && !via_token
                    && matches!(method.as_str(), "PUT" | "PATCH" | "COPY" | "MOVE" | "DELETE")
                {
                    let _ = self
                        .preferred_representation(path, user.clone(), prefer, &response_headers, &mut res)
                        .await;
                }
                return Ok(res);
            }
        }

        let response_user = user.clone();
        let response_destination = destination.clone();
        match method.clone() {
            Method::GET | Method::HEAD => {
                if is_dir {
                    if !render_try_index
                        && !render_index
                        && !render_spa
                        && !check_preconditions(headers, None, true, true, &mut res)
                    {
                        return Ok(res);
                    }
                    if render_try_index {
                        if allow_archive && has_query_flag(&query_params, "zip") {
                            if !allow_archive {
                                self.handle_not_found(&query_params, head_only, &mut res)
                                    .await?;
                                return Ok(res);
                            }
                            self.handle_zip_dir(
                                path,
                                head_only,
                                user.clone(),
                                access_paths,
                                &mut res,
                            )
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
                            DufsErrorCode::ArchiveDisabled.respond(&mut res);
                            return Ok(res);
                        }
                        self.handle_zip_dir(path, head_only, user.clone(), access_paths, &mut res)
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
                        self.handle_file_json(
                            path,
                            head_only,
                            &access_paths,
                            user.as_deref(),
                            &mut res,
                        )
                        .await?;
                    } else if has_query_flag(&query_params, "edit") {
                        self.handle_edit_file(
                            path,
                            DataKind::Edit,
                            head_only,
                            user,
                            &access_paths,
                            &mut res,
                        )
                        .await?;
                    } else if has_query_flag(&query_params, "view") {
                        self.handle_edit_file(
                            path,
                            DataKind::View,
                            head_only,
                            user,
                            &access_paths,
                            &mut res,
                        )
                        .await?;
                    } else if has_query_flag(&query_params, "hash") {
                        if self.args.allow_hash {
                            self.handle_hash_file(path, head_only, &mut res).await?;
                        } else {
                            DufsErrorCode::HashDisabled.respond(&mut res);
                        }
                    } else {
                        self.handle_send_file(path, headers, head_only, &mut res)
                            .await?;
                    }
                } else if render_spa {
                    self.handle_render_spa(path, &query_params, headers, head_only, user, &mut res)
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
                    self.handle_not_found(&query_params, head_only, &mut res)
                        .await?;
                }
            }
            Method::OPTIONS => {
                set_webdav_headers(
                    &mut res,
                    allow_upload,
                    allow_delete,
                    is_file,
                    is_dir,
                    is_miss,
                    allow_search,
                );
            }
            Method::PUT => {
                if !allow_upload {
                    DufsErrorCode::UploadDisabled.respond(&mut res);
                } else if headers.contains_key("Content-Range") {
                    *res.status_mut() = StatusCode::BAD_REQUEST;
                } else if is_dir {
                    DufsErrorCode::IsADirectory.respond(&mut res);
                } else if !allow_delete && size > 0 {
                    DufsErrorCode::DeleteDisabled.respond(&mut res);
                } else if !has_directory_parent(path).await {
                    *res.status_mut() = StatusCode::CONFLICT;
                } else if check_preconditions(
                    headers,
                    validators.as_ref(),
                    meta.is_some(),
                    false,
                    &mut res,
                ) {
                    self.handle_upload(path, None, meta.as_ref(), req, &mut res)
                        .await?;
                }
            }
            Method::PATCH => {
                if !allow_upload {
                    DufsErrorCode::UploadDisabled.respond(&mut res);
                } else if is_miss {
                    status_not_found(&mut res);
                } else if is_dir {
                    DufsErrorCode::IsADirectory.respond(&mut res);
                } else if headers.get(CONTENT_TYPE).is_some_and(|v| {
                    !matches!(
                        v.to_str()
                            .ok()
                            .and_then(|s| s.split(';').next())
                            .map(str::trim),
                        Some(
                            "application/vnd.dufs.offset-patch"
                                | "application/octet-stream"
                                | "application/x-www-form-urlencoded"
                        )
                    )
                }) {
                    *res.status_mut() = StatusCode::UNSUPPORTED_MEDIA_TYPE;
                    res.headers_mut().insert(
                        "Accept-Patch",
                        HeaderValue::from_static("application/vnd.dufs.offset-patch"),
                    );
                } else {
                    let offset = match parse_upload_offset(headers, size) {
                        Ok(v) => v,
                        Err(err) => {
                            DufsErrorCode::InvalidUploadOffset
                                .respond_with(&mut res, &err.to_string());
                            return Ok(res);
                        }
                    };
                    match offset {
                        Some(offset) => {
                            if offset < size && !allow_delete {
                                DufsErrorCode::DeleteDisabled.respond(&mut res);
                                return Ok(res);
                            }
                            if check_preconditions(
                                headers,
                                validators.as_ref(),
                                meta.is_some(),
                                false,
                                &mut res,
                            ) {
                                self.handle_upload(
                                    path,
                                    Some(offset),
                                    meta.as_ref(),
                                    req,
                                    &mut res,
                                )
                                .await?;
                            }
                        }
                        None => {
                            DufsErrorCode::MissingUploadOffset.respond(&mut res);
                        }
                    }
                }
            }
            Method::DELETE => {
                if headers
                    .get("Depth")
                    .is_some_and(|v| v.as_bytes() != b"infinity")
                {
                    DufsErrorCode::InvalidDepth.respond(&mut res);
                } else if !allow_delete {
                    DufsErrorCode::DeleteDisabled.respond(&mut res);
                } else if is_dir && Self::has_restricted_descendant(&access_paths) {
                    DufsErrorCode::ReadonlyDescendant.respond(&mut res);
                } else if is_miss {
                    status_not_found(&mut res);
                } else {
                    let (parts, body) = req.into_parts();
                    if !reject_nonempty_body(body, &mut res).await
                        && check_preconditions(
                            &parts.headers,
                            validators.as_ref(),
                            meta.is_some(),
                            false,
                            &mut res,
                        )
                    {
                        self.handle_delete(path, is_dir, &mut res).await?;
                    }
                }
            }
            method => match method.as_str() {
                "REPORT" => {
                    if is_miss {
                        status_not_found(&mut res);
                    } else if check_preconditions(
                        headers,
                        validators.as_ref(),
                        meta.is_some(),
                        false,
                        &mut res,
                    ) {
                        let (parts, body) = req.into_parts();
                        match Limited::new(body, 65536).collect().await {
                            Ok(body) => {
                                self.handle_sync_report(
                                    path,
                                    &parts.headers,
                                    &body.to_bytes(),
                                    user,
                                    access_paths,
                                    via_token,
                                    &mut res,
                                )
                                .await?;
                                if res.status() == StatusCode::MULTI_STATUS {
                                    prefer.applied(&mut res, true, false, false);
                                }
                            }
                            Err(_) => *res.status_mut() = StatusCode::PAYLOAD_TOO_LARGE,
                        }
                    }
                }
                "SEARCH" if allow_search && is_dir => {
                    if !check_preconditions(
                        headers,
                        validators.as_ref(),
                        meta.is_some(),
                        false,
                        &mut res,
                    ) {
                        return Ok(res);
                    }
                    let req_path = req_path.to_owned();
                    let (parts, body) = req.into_parts();
                    match Limited::new(body, 65536).collect().await {
                        Ok(body) => {
                            self.handle_dav_search(
                                path,
                                &req_path,
                                &parts.headers,
                                &body.to_bytes(),
                                user,
                                access_paths,
                                via_token,
                                &mut res,
                            )
                            .await?;
                            if res.status() == StatusCode::MULTI_STATUS {
                                prefer.applied(&mut res, true, false, false);
                            }
                        }
                        Err(_) => *res.status_mut() = StatusCode::PAYLOAD_TOO_LARGE,
                    }
                }
                "PROPFIND" => {
                    let anonymous = authorization.is_none();
                    let (parts, body) = req.into_parts();
                    let headers = &parts.headers;
                    let body = match Limited::new(body, 65536).collect().await {
                        Ok(body) => body.to_bytes(),
                        Err(_) => {
                            *res.status_mut() = StatusCode::PAYLOAD_TOO_LARGE;
                            return Ok(res);
                        }
                    };
                    if headers
                        .get("depth")
                        .is_some_and(|v| !matches!(v.as_bytes(), b"0" | b"1" | b"infinity"))
                    {
                        DufsErrorCode::InvalidDepth.respond(&mut res);
                        return Ok(res);
                    }
                    let selection = match DavSelection::parse(&body) {
                        Ok(s) => s,
                        Err(_) => {
                            *res.status_mut() = StatusCode::BAD_REQUEST;
                            return Ok(res);
                        }
                    };
                    // Ordinary directory reads see the last complete file during an upload.
                    let _sync_guard = if is_dir
                        && !matches!(selection, DavSelection::Names)
                        && selection.extension("sync-token")
                    {
                        Some(self.lock_write_paths(path, &[]).await)
                    } else {
                        None
                    };
                    if is_dir {
                        if !check_preconditions(
                            headers,
                            validators.as_ref(),
                            meta.is_some(),
                            false,
                            &mut res,
                        ) {
                            return Ok(res);
                        }
                        self.handle_propfind_dir(
                            path,
                            headers,
                            user.clone(),
                            access_paths,
                            anonymous,
                            &selection,
                            via_token,
                            &mut res,
                        )
                        .await?;
                    } else if is_file {
                        if !check_preconditions(
                            headers,
                            validators.as_ref(),
                            meta.is_some(),
                            false,
                            &mut res,
                        ) {
                            return Ok(res);
                        }
                        self.handle_propfind_file(path, &selection, headers, &mut res)
                            .await?;
                    } else {
                        status_not_found(&mut res);
                    }
                    if res.status() == StatusCode::MULTI_STATUS {
                        prefer.applied(
                            &mut res,
                            true,
                            is_dir && headers.get("Depth").is_some_and(|v| v.as_bytes() == b"1"),
                            false,
                        );
                    }
                }
                "PROPPATCH" => {
                    if is_miss {
                        status_not_found(&mut res);
                    } else if check_preconditions(
                        headers,
                        validators.as_ref(),
                        meta.is_some(),
                        false,
                        &mut res,
                    ) {
                        if is_file || is_dir {
                            let req_path = req_path.to_owned();
                            let body = match Limited::new(req.into_body(), 65536).collect().await {
                                Ok(body) => body.to_bytes(),
                                Err(_) => {
                                    *res.status_mut() = StatusCode::PAYLOAD_TOO_LARGE;
                                    return Ok(res);
                                }
                            };
                            self.handle_proppatch(
                                &req_path,
                                path,
                                is_dir,
                                user.clone(),
                                access_paths,
                                &body,
                                prefer,
                                &mut res,
                            )
                            .await?;
                        } else {
                            status_not_found(&mut res);
                        }
                    }
                }
                "MKCOL" => {
                    if !allow_upload {
                        DufsErrorCode::UploadDisabled.respond(&mut res);
                    } else if !is_miss {
                        set_webdav_headers(
                            &mut res,
                            allow_upload,
                            allow_delete,
                            is_file,
                            is_dir,
                            is_miss,
                            allow_search,
                        );
                        DufsErrorCode::AlreadyExists.respond(&mut res);
                    } else if !has_directory_parent(path).await {
                        *res.status_mut() = StatusCode::CONFLICT;
                    } else {
                        let (parts, body) = req.into_parts();
                        let body = match Limited::new(body, 65536).collect().await {
                            Ok(body) => body.to_bytes(),
                            Err(_) => {
                                *res.status_mut() = StatusCode::UNSUPPORTED_MEDIA_TYPE;
                                return Ok(res);
                            }
                        };
                        let extended = !body.is_empty();
                        if extended
                            && !parts
                                .headers
                                .get(CONTENT_TYPE)
                                .and_then(|v| v.to_str().ok())
                                .is_some_and(|v| {
                                    matches!(
                                        v.split(';').next().unwrap_or("").trim(),
                                        "application/xml" | "text/xml"
                                    )
                                })
                        {
                            *res.status_mut() = StatusCode::UNSUPPORTED_MEDIA_TYPE;
                        } else if check_preconditions(
                            &parts.headers,
                            validators.as_ref(),
                            meta.is_some(),
                            false,
                            &mut res,
                        ) && (!extended || dav_common::extended_mkcol(&body, &mut res))
                        {
                            self.handle_mkcol(path, &mut res).await?;
                            if extended && res.status() == StatusCode::CREATED {
                                prefer.applied(&mut res, true, false, false);
                            }
                        }
                    }
                }
                "COPY" => {
                    if !allow_upload {
                        DufsErrorCode::UploadDisabled.respond(&mut res);
                    } else if is_miss {
                        status_not_found(&mut res);
                    } else {
                        let (parts, body) = req.into_parts();
                        if !reject_nonempty_body(body, &mut res).await {
                            self.handle_copy(
                                path,
                                destination.unwrap(),
                                &parts.headers,
                                validators.as_ref(),
                                user.as_deref(),
                                &mut res,
                            )
                            .await?;
                        }
                    }
                }
                "MOVE" => {
                    if !allow_upload {
                        DufsErrorCode::UploadDisabled.respond(&mut res);
                    } else if !allow_delete {
                        DufsErrorCode::DeleteDisabled.respond(&mut res);
                    } else if is_dir && Self::has_restricted_descendant(&access_paths) {
                        DufsErrorCode::ReadonlyDescendant.respond(&mut res);
                    } else if is_miss {
                        status_not_found(&mut res);
                    } else {
                        let (parts, body) = req.into_parts();
                        if !reject_nonempty_body(body, &mut res).await {
                            self.handle_move(
                                path,
                                destination.unwrap(),
                                &parts.headers,
                                validators.as_ref(),
                                &mut res,
                            )
                            .await?;
                        }
                    }
                }
                _ => {
                    set_webdav_headers(
                        &mut res,
                        allow_upload,
                        allow_delete,
                        is_file,
                        is_dir,
                        is_miss,
                        allow_search,
                    );
                    DufsErrorCode::MethodNotAllowed.respond(&mut res);
                }
            },
        }
        if is_write_method(&method) && res.status().is_success() {
            self.sync_observe_tracked(path, response_destination.as_deref())
                .await;
        }
        if prefer.minimal
            && matches!(method.as_str(), "PUT" | "PATCH" | "COPY" | "MOVE" | "DELETE")
            && matches!(res.status(), StatusCode::CREATED | StatusCode::NO_CONTENT)
        {
            prefer.applied(&mut res, true, false, false);
        }
        if prefer.representation
            && !via_token
            && (matches!(method.as_str(), "PUT" | "PATCH" | "COPY" | "MOVE")
                || (method == Method::DELETE && res.status() == StatusCode::PRECONDITION_FAILED))
            && matches!(
                res.status(),
                StatusCode::OK
                    | StatusCode::CREATED
                    | StatusCode::NO_CONTENT
                    | StatusCode::PRECONDITION_FAILED
            )
        {
            let target = if res.status() == StatusCode::PRECONDITION_FAILED {
                path
            } else {
                response_destination.as_deref().unwrap_or(path)
            };
            // A response preference must not turn an already committed write into an error.
            let _ = self
                .preferred_representation(target, response_user, prefer, &response_headers, &mut res)
                .await;
        }
        Ok(res)
    }

    async fn preferred_representation(
        &self,
        path: &Path,
        user: Option<String>,
        prefer: DavPrefer,
        headers: &HeaderMap,
        res: &mut Response,
    ) -> Result<()> {
        let Ok(relative) = path.strip_prefix(&self.args.serve_path) else {
            return Ok(());
        };
        let Some(access) = self
            .args
            .auth
            .access_for(
                user.as_deref(),
                &normalize_path(relative),
                &Method::GET,
                &self.args.serve_path,
            )
            .await
        else {
            return Ok(());
        };
        if self.guard_root_contained(path).await {
            return Ok(());
        }
        let Ok(meta) = fs::metadata(path).await else {
            return Ok(());
        };
        let mut representation = Response::default();
        if meta.is_file() {
            self.handle_send_file(path, headers, false, &mut representation)
                .await?;
        } else if meta.is_dir() {
            representation = self
                .dav_representation(path, true, user, Some(access))
                .await?;
        } else {
            return Ok(());
        }
        if !representation.status().is_success() {
            return Ok(());
        }
        let status = res.status();
        if let Some(code) = res.headers().get("X-Dufs-Error-Code").cloned() {
            representation
                .headers_mut()
                .insert("X-Dufs-Error-Code", code);
        }
        *representation.status_mut() = if status == StatusCode::NO_CONTENT {
            StatusCode::OK
        } else {
            status
        };
        representation.headers_mut().insert(
            "Content-Location",
            HeaderValue::from_str(&encode_uri(&format!(
                "{}{}",
                self.args.uri_prefix,
                normalize_path(relative)
            )))?,
        );
        prefer.applied(&mut representation, false, false, true);
        *res = representation;
        Ok(())
    }

    async fn lock_write_paths(
        &self,
        path: &Path,
        destinations: &[PathBuf],
    ) -> (
        Vec<OwnedRwLockReadGuard<()>>,
        Vec<OwnedRwLockWriteGuard<()>>,
    ) {
        loop {
            let keys = write_lock_keys(path, destinations).await;
            let locks = {
                let mut map = self.write_locks.lock().unwrap();
                map.retain(|_, lock| lock.strong_count() != 0);
                keys.iter()
                    .map(|(key, exclusive)| {
                        let lock = map.get(key).and_then(Weak::upgrade).unwrap_or_else(|| {
                            let lock = Arc::new(RwLock::new(()));
                            map.insert(key.clone(), Arc::downgrade(&lock));
                            lock
                        });
                        (lock, *exclusive)
                    })
                    .collect::<Vec<_>>()
            };
            let (mut reads, mut writes) = (vec![], vec![]);
            // All requests acquire locks in path order, including two-path operations.
            for (lock, exclusive) in locks {
                if exclusive {
                    writes.push(lock.write_owned().await);
                } else {
                    reads.push(lock.read_owned().await);
                }
            }
            if keys == write_lock_keys(path, destinations).await {
                return (reads, writes);
            }
            // A queued directory or symlink move may have changed the resolution.
        }
    }

    async fn handle_upload(
        &self,
        path: &Path,
        upload_offset: Option<u64>,
        old_meta: Option<&Metadata>,
        req: Request,
        res: &mut Response,
    ) -> Result<()> {
        let path = upload_target(path).await?;
        if !has_directory_parent(&path).await {
            *res.status_mut() = StatusCode::CONFLICT;
            return Ok(());
        }
        if let Some(meta) = old_meta {
            if !meta.is_file() {
                DufsErrorCode::IsADirectory.respond(res);
                return Ok(());
            }
            fs::OpenOptions::new().write(true).open(&path).await?;
        }
        let (file, temp) = upload_temp(&path)?;
        let mut file = fs::File::from_std(file);
        if let Some(offset) = upload_offset {
            let mut original = fs::File::open(&path).await?;
            io::copy(&mut original, &mut file).await?;
            file.seek(SeekFrom::Start(offset)).await?;
        }
        if !http_digest::copy_upload(req, &mut file, res).await? {
            return Ok(());
        }
        file.flush().await?;
        if let Some(meta) = old_meta {
            file.set_permissions(upload_permissions(meta)).await?;
        }
        finish_write(&file, old_meta).await?;
        drop(file);
        temp.persist(&path)?;
        set_written_etag(&path, res).await;
        *res.status_mut() = if upload_offset.is_some() || old_meta.is_some() {
            StatusCode::NO_CONTENT
        } else {
            StatusCode::CREATED
        };
        Ok(())
    }

    /// Whether a recursive write on the guarded path would also affect a path
    /// with a stricter rule.
    ///
    /// Recursive operations must be rejected then: `remove_dir_all` and
    /// `rename` act on the whole subtree, so they would remove or relocate a
    /// child that a direct request to that child is not allowed to touch.
    fn has_restricted_descendant(access_paths: &AccessPaths) -> bool {
        access_paths.has_stricter_descendant(AccessPerm::ReadWrite)
    }

    fn dav_status(&self, path: &Path, status: StatusCode) -> String {
        let relative = path.strip_prefix(&self.args.serve_path).unwrap_or(path);
        let mut href = encode_uri(&format!(
            "{}{}",
            self.args.uri_prefix,
            normalize_path(relative)
        ));
        if path.is_dir() && !href.ends_with('/') {
            href.push('/');
        }
        format!(
            "<D:response><D:href>{}</D:href><D:status>HTTP/1.1 {} {}</D:status></D:response>",
            escape_str_pcdata(&href),
            status.as_u16(),
            status.canonical_reason().unwrap_or("")
        )
    }

    fn copy_member_failed(
        &self,
        root: &Path,
        member: &Path,
        status: StatusCode,
        res: &mut Response,
    ) {
        // Copy is staged in a temporary tree. On a member failure the entire
        // destination remains unchanged; report both the failure and rollback.
        let output =
            self.dav_status(member, status) + &self.dav_status(root, StatusCode::FAILED_DEPENDENCY);
        res_multistatus(res, &output);
    }

    async fn handle_delete(&self, path: &Path, is_dir: bool, res: &mut Response) -> Result<()> {
        if !is_dir || fs::symlink_metadata(path).await?.is_symlink() {
            let result = if is_dir {
                fs::remove_dir_all(path).await
            } else {
                fs::remove_file(path).await
            };
            *res.status_mut() = match result {
                Ok(()) => StatusCode::NO_CONTENT,
                Err(error) => filesystem_status(&error),
            };
            return Ok(());
        }
        let root = path.to_path_buf();
        let failures = tokio::task::spawn_blocking(move || {
            let mut blocked = std::collections::HashSet::new();
            let mut failures = vec![];
            for entry in WalkDir::new(&root).follow_links(false).contents_first(true) {
                let (path, error) = match entry {
                    Err(error) => (
                        error.path().unwrap_or(&root).to_path_buf(),
                        error
                            .io_error()
                            .map(filesystem_status)
                            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                    ),
                    Ok(entry) => {
                        let path = entry.into_path();
                        if blocked.contains(&path) {
                            continue;
                        }
                        let result = match std::fs::symlink_metadata(&path) {
                            Ok(meta) if meta.is_dir() => std::fs::remove_dir(&path),
                            Ok(_) => std::fs::remove_file(&path),
                            Err(error) => Err(error),
                        };
                        match result {
                            Ok(()) => continue,
                            Err(error) => (path, filesystem_status(&error)),
                        }
                    }
                };
                blocked.extend(path.ancestors().skip(1).map(Path::to_path_buf));
                failures.push((path, error));
            }
            failures
        })
        .await?;
        if failures.is_empty() {
            status_no_content(res);
        } else {
            let output: String = failures
                .iter()
                .map(|(path, status)| self.dav_status(path, *status))
                .collect();
            res_multistatus(res, &output);
        }
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
        if exist {
            paths = match self
                .list_dir(path, path, &access_paths, user.as_deref())
                .await
            {
                Ok(paths) => paths,
                Err(_) => {
                    DufsErrorCode::InternalError.respond(res);
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
            let prefix = self.relative_parts(path);

            let search_paths = tokio::spawn(collect_dir_entries(
                access_paths.clone(),
                self.running.clone(),
                path_buf,
                hidden,
                self.args.allow_symlink,
                self.args.serve_path.clone(),
                self.args.auth.clone(),
                user.clone(),
                move |x| get_file_name(x.path()).to_lowercase().contains(&search),
            ))
            .await?;

            for search_path in search_paths.into_iter() {
                let Ok(Some(mut item)) = self.to_pathitem(search_path.as_path(), path).await else {
                    continue;
                };
                let Some(access) = self
                    .entry_access(
                        &access_paths,
                        &search_path,
                        &item.name,
                        &prefix,
                        user.as_deref(),
                    )
                    .await
                else {
                    continue;
                };
                self.set_item_access(&mut item, access);
                paths.push(item);
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
        user: Option<String>,
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
        let auth = self.args.auth.clone();
        tokio::spawn(async move {
            if let Err(e) = zip_dir(
                &mut writer,
                &path,
                access_paths,
                &hidden,
                compression,
                follow_symlinks,
                serve_path,
                auth,
                user,
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
        let index_readable = if fs::metadata(&index_path)
            .await
            .ok()
            .map(|v| v.is_file())
            .unwrap_or_default()
        {
            let prefix = self.relative_parts(path);
            let access = access_paths
                .entry_access(INDEX_NAME, &prefix, Some(&self.args.serve_path))
                .await;
            let index_rel = self.relative_parts(&index_path).join("/");
            !access.perm.indexonly()
                && !self.guard_root_contained(&index_path).await
                && self
                    .args
                    .auth
                    .can_access(
                        user.as_deref(),
                        &index_rel,
                        &Method::GET,
                        &self.args.serve_path,
                    )
                    .await
        } else {
            false
        };
        if index_readable {
            self.handle_send_file(&index_path, headers, head_only, res)
                .await?;
        } else if self.args.render_try_index {
            self.handle_ls_dir(path, true, query_params, head_only, user, access_paths, res)
                .await?;
        } else {
            self.handle_not_found(query_params, head_only, res).await?;
        }
        Ok(())
    }

    async fn handle_file_json(
        &self,
        path: &Path,
        head_only: bool,
        access_paths: &AccessPaths,
        user: Option<&str>,
        res: &mut Response,
    ) -> Result<()> {
        let mut pathitem = match self.to_pathitem(path, &self.args.serve_path).await? {
            Some(v) => v,
            None => {
                status_not_found(res);
                return Ok(());
            }
        };
        let readwrite = access_paths.perm().readwrite() && self.path_readwrite(path, user).await;
        self.set_item_access(
            &mut pathitem,
            EntryAccess {
                perm: if readwrite {
                    access_paths.perm()
                } else {
                    AccessPerm::ReadOnly
                },
                restricted: false,
            },
        );
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
        user: Option<String>,
        res: &mut Response,
    ) -> Result<()> {
        if path.extension().is_none() {
            let index_path = self.args.serve_path.join(INDEX_NAME);
            if self
                .args
                .auth
                .can_read(user.as_deref(), INDEX_NAME, &self.args.serve_path)
                .await
                && !self.guard_root_contained(&index_path).await
            {
                self.handle_send_file(&index_path, headers, head_only, res)
                    .await?;
                return Ok(());
            }
        }
        self.handle_not_found(query_params, head_only, res).await?;
        Ok(())
    }

    async fn handle_not_found(
        &self,
        query_params: &HashMap<String, String>,
        head_only: bool,
        res: &mut Response,
    ) -> Result<()> {
        if let Some(error_page) = &self.args.error_page {
            if !has_query_flag(query_params, "noscript") {
                self.handle_send_error_page(error_page, head_only, res)
                    .await?;
                *res.status_mut() = StatusCode::NOT_FOUND;
                return Ok(());
            }
        }
        status_not_found(res);
        Ok(())
    }

    async fn handle_send_error_page(
        &self,
        path: &Path,
        head_only: bool,
        res: &mut Response,
    ) -> Result<()> {
        let html = fs::read_to_string(path).await?;
        let output = self.render_html(&html);
        res.headers_mut()
            .typed_insert(ContentType::from(mime_guess::mime::TEXT_HTML_UTF_8));
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

    fn render_html(&self, html: &str) -> String {
        html.replace(
            "__ASSETS_PREFIX__",
            &format!("{}{}", self.args.uri_prefix, self.assets_prefix),
        )
        .replace("__PATH_PREFIX__", &self.args.uri_prefix)
    }

    async fn handle_internal(
        &self,
        req_path: &str,
        head_only: bool,
        headers: &HeaderMap<HeaderValue>,
        res: &mut Response,
    ) -> Result<bool> {
        if let Some(name) = req_path.strip_prefix(&self.assets_prefix) {
            match self.args.assets.as_ref() {
                Some(assets_path) => {
                    let path = match join_within(assets_path, name) {
                        Some(path) => path,
                        None => {
                            status_not_found(res);
                            return Ok(true);
                        }
                    };
                    if path.exists() {
                        self.handle_send_file(&path, headers, head_only, res)
                            .await?;
                    } else {
                        status_not_found(res);
                        return Ok(true);
                    }
                }
                None => match name {
                    "index.js" => {
                        res.headers_mut().insert(
                            "content-type",
                            HeaderValue::from_static("application/javascript; charset=UTF-8"),
                        );
                        res.headers_mut()
                            .typed_insert(ContentLength(INDEX_JS.len() as u64));
                        if !head_only {
                            *res.body_mut() = body_full(INDEX_JS);
                        }
                    }
                    "index.css" => {
                        res.headers_mut().insert(
                            "content-type",
                            HeaderValue::from_static("text/css; charset=UTF-8"),
                        );
                        res.headers_mut()
                            .typed_insert(ContentLength(INDEX_CSS.len() as u64));
                        if !head_only {
                            *res.body_mut() = body_full(INDEX_CSS);
                        }
                    }
                    "favicon.ico" => {
                        res.headers_mut()
                            .insert("content-type", HeaderValue::from_static("image/x-icon"));
                        res.headers_mut()
                            .typed_insert(ContentLength(FAVICON_ICO.len() as u64));
                        if !head_only {
                            *res.body_mut() = body_full(FAVICON_ICO);
                        }
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
            let body = r#"{"status":"OK"}"#;
            res.headers_mut()
                .typed_insert(ContentLength(body.len() as u64));
            if !head_only {
                *res.body_mut() = body_full(body);
            }
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
        let mut file = fs::File::open(path).await?;
        let meta = file.metadata().await?;
        let size = meta.len();
        res.headers_mut().append("Vary", HeaderValue::from_static("Accept-Encoding"));
        res.headers_mut().append("Vary", HeaderValue::from_static("Range, Want-Content-Digest, Want-Repr-Digest"));
        let range_requested = !head_only && headers.get(RANGE).is_some_and(|value| {
            value.to_str().ok().and_then(|v| v.split_once('='))
                .is_some_and(|(unit, _)| unit.eq_ignore_ascii_case("bytes"))
        });
        let coding = http_encoding::select(headers, range_requested);
        let zstd = match coding {
            Some(value) => value,
            None => {
                *res.status_mut() = StatusCode::NOT_ACCEPTABLE;
                return Ok(());
            }
        };
        let mut use_range = range_requested && !zstd;
        let mut cache_headers = extract_cache_headers(&meta);
        if zstd {
            cache_headers = cache_headers.and_then(|(etag, modified)| {
                http_encoding::zstd_etag(&etag).map(|tag| (tag, modified))
            });
        }
        if !check_preconditions(headers, cache_headers.as_ref(), true, true, res) {
            return Ok(());
        }
        if let Some((etag, last_modified)) = cache_headers {
            res.headers_mut()
                .typed_insert(CacheControl::new().with_no_cache());
            res.headers_mut().typed_insert(last_modified);
            res.headers_mut().typed_insert(etag.clone());

            if use_range && headers.contains_key("If-Range") {
                // If-Range dates use exact equality, not the earlier-than
                // comparison used by If-Unmodified-Since.
                use_range = if let Ok(date) = headers::Header::decode(&mut headers.get_all("If-Range").iter()) {
                    last_modified == date
                } else {
                    headers.typed_get::<IfRange>().is_some_and(|v| !v.is_modified(Some(&etag), None))
                };
            }
        } else if headers.contains_key("If-Range") {
            use_range = false;
        }

        // RFC 9110 §14.2: a server MAY ignore Range when the selected
        // representation has no content. A zero-length representation has no
        // non-empty byte range to serve and a 206 response cannot express an
        // empty one, so ignore Range and fall through to a 200 response.
        let ranges = if use_range && size != 0 {
            headers.get(RANGE).map(|range| {
                range
                    .to_str()
                    .ok()
                    .and_then(|range| parse_range(range, size, true))
            })
        } else {
            None
        };

        let content_type = get_content_type(path, &mut file).await?;
        res.headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_str(&content_type)?);

        let filename = try_get_file_name(path)?;
        set_content_disposition(res, true, filename)?;

        if zstd {
            res.headers_mut().insert("Content-Encoding", HeaderValue::from_static("zstd"));
            let encoder = async_compression::tokio::bufread::ZstdEncoder::with_quality_and_params(
                io::BufReader::new(file),
                async_compression::Level::Default,
                &[async_compression::zstd::CParameter::window_log(23)],
            );
            *res.body_mut() = StreamBody::new(
                ReaderStream::with_capacity(encoder, BUF_SIZE)
                    .map_ok(Frame::data)
                    .map_err(anyhow::Error::new),
            ).boxed();
            return Ok(());
        }
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
                    http_digest::response_file_digests(
                        &mut file,
                        headers,
                        res,
                        size,
                        head_only,
                        Some((start, range_size)),
                    )
                    .await?;
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
                    let footer = format!("--{boundary}--\r\n");

                    // Compute the exact length up front, so a HEAD request (and
                    // the Content-Length header) never has to read the file.
                    let mut content_length = footer.len();
                    for (start, end) in &ranges {
                        let range_size = end - start + 1;
                        let part_header = format!(
                            "--{boundary}\r\nContent-Type: {content_type}\r\nContent-Range: bytes {start}-{end}/{size}\r\n\r\n",
                        );
                        content_length = content_length
                            .saturating_add(part_header.len())
                            .saturating_add(usize::try_from(range_size).unwrap_or(usize::MAX))
                            .saturating_add(2);
                    }

                    res.headers_mut().insert(
                        CONTENT_TYPE,
                        format!("multipart/byteranges; boundary={boundary}").parse()?,
                    );
                    res.headers_mut()
                        .insert(CONTENT_LENGTH, format!("{content_length}").parse()?);
                    http_digest::response_file_digests(
                        &mut file, headers, res, size, head_only, None,
                    )
                    .await?;
                    if head_only {
                        return Ok(());
                    }

                    // Stream the parts instead of materializing the whole body
                    // in memory.
                    let stream = async_stream::try_stream! {
                        let mut buffer = vec![0; BUF_SIZE];
                        for (start, end) in ranges {
                            file.seek(SeekFrom::Start(start)).await?;
                            let part_header = format!(
                                "--{boundary}\r\nContent-Type: {content_type}\r\nContent-Range: bytes {start}-{end}/{size}\r\n\r\n",
                            );
                            yield Bytes::from(part_header);
                            let mut remaining = end - start + 1;
                            while remaining > 0 {
                                let want = remaining.min(BUF_SIZE as u64) as usize;
                                file.read_exact(&mut buffer[..want]).await?;
                                yield Bytes::copy_from_slice(&buffer[..want]);
                                remaining -= want as u64;
                            }
                            yield Bytes::from_static(b"\r\n");
                        }
                        yield Bytes::from(footer);
                    };
                    let stream_body = StreamBody::new(
                        stream
                            .map_ok(Frame::data)
                            .map_err(|err: io::Error| anyhow::Error::new(err)),
                    );
                    *res.body_mut() = stream_body.boxed();
                }
            } else {
                *res.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
                res.headers_mut()
                    .insert(CONTENT_RANGE, format!("bytes */{size}").parse()?);
            }
        } else {
            res.headers_mut()
                .insert(CONTENT_LENGTH, format!("{size}").parse()?);
            http_digest::response_file_digests(
                &mut file,
                headers,
                res,
                size,
                head_only,
                Some((0, size)),
            )
            .await?;
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
        access_paths: &AccessPaths,
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
        let readwrite =
            access_paths.perm().readwrite() && self.path_readwrite(path, user.as_deref()).await;
        let data = EditData {
            href,
            kind,
            uri_prefix: self.args.uri_prefix.clone(),
            allow_upload: self.args.allow_upload && readwrite,
            allow_delete: self.args.allow_delete && readwrite,
            auth: self.args.auth.has_users(),
            user,
            editable,
        };
        res.headers_mut()
            .typed_insert(ContentType::from(mime_guess::mime::TEXT_HTML_UTF_8));
        let index_data = STANDARD.encode(serde_json::to_string(&data)?);
        let output = self
            .render_html(&self.html)
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
        if !head_only {
            *res.body_mut() = body_full(output);
        }
        Ok(())
    }

    async fn handle_tokengen(
        &self,
        relative_path: &str,
        user: Option<String>,
        head_only: bool,
        res: &mut Response,
    ) -> Result<()> {
        let user = match user {
            Some(user) => user,
            None => {
                self.auth_reject(res, false)?;
                return Ok(());
            }
        };
        let output = self.args.auth.generate_token(relative_path, &user)?;
        res.headers_mut()
            .typed_insert(ContentType::from(mime_guess::mime::TEXT_PLAIN_UTF_8));
        res.headers_mut()
            .typed_insert(ContentLength(output.len() as u64));
        if !head_only {
            *res.body_mut() = body_full(output);
        }
        Ok(())
    }

    async fn handle_propfind_dir(
        &self,
        path: &Path,
        headers: &HeaderMap<HeaderValue>,
        user: Option<String>,
        access_paths: AccessPaths,
        anonymous: bool,
        selection: &DavSelection,
        via_token: bool,
        res: &mut Response,
    ) -> Result<()> {
        let depth: u32 = match headers.get("depth") {
            // RFC 4918 9.1: refusing infinite depth with DAV:propfind-finite-depth
            // lets clients fall back to walking the tree with Depth: 1.
            Some(v) if v.as_bytes().eq_ignore_ascii_case(b"infinity") => {
                *res.status_mut() = StatusCode::FORBIDDEN;
                res.headers_mut().insert(
                    "content-type",
                    HeaderValue::from_static("application/xml; charset=utf-8"),
                );
                *res.body_mut() = body_full(
                    r#"<?xml version="1.0" encoding="utf-8" ?>
<D:error xmlns:D="DAV:"><D:propfind-finite-depth/></D:error>"#,
                );
                return Ok(());
            }
            Some(v) => match v.as_bytes() {
                b"0" => 0,
                b"1" => 1,
                _ => {
                    DufsErrorCode::InvalidDepth.respond(res);
                    return Ok(());
                }
            },
            None => {
                *res.status_mut() = StatusCode::FORBIDDEN;
                res.headers_mut().insert(
                    CONTENT_TYPE,
                    HeaderValue::from_static("application/xml; charset=utf-8"),
                );
                *res.body_mut() =
                    body_full(r#"<D:error xmlns:D="DAV:"><D:propfind-finite-depth/></D:error>"#);
                return Ok(());
            }
        };
        let mut paths = match self.to_pathitem(path, &self.args.serve_path).await? {
            Some(v) => vec![v],
            None => vec![],
        };
        if depth == 1 && DavPrefer::parse(headers).noroot {
            paths.clear();
        }
        if depth == 1 {
            // Preserve anonymous collection-name discovery for legacy WebDAV
            // clients, but evaluate each resource's live properties with its
            // actual access rules.
            let listing_access = if access_paths.perm().indexonly() && anonymous {
                AccessPaths::new(AccessPerm::ReadOnly)
            } else {
                access_paths.clone()
            };
            match self
                .list_dir(
                    path,
                    &self.args.serve_path,
                    &listing_access,
                    user.as_deref(),
                )
                .await
            {
                Ok(child) => paths.extend(child),
                Err(_) => {
                    DufsErrorCode::InternalError.respond(res);
                    return Ok(());
                }
            }
        }
        let mut output = String::new();
        for item in paths {
            let item_path = self.args.serve_path.join(&item.name);
            let relative = item_path.strip_prefix(path).unwrap_or(Path::new(""));
            let item_access = if relative.as_os_str().is_empty() {
                Some(access_paths.clone())
            } else {
                access_paths
                    .guard(&normalize_path(relative), &Method::GET, Some(path))
                    .await
            };
            if item_access.is_none()
                || !self
                    .path_can_access(&item_path, &Method::GET, user.as_deref())
                    .await
            {
                output.push_str(&self.dav_status(&item_path, StatusCode::FORBIDDEN));
                continue;
            }
            match self
                .dav_item_xml(
                    &item,
                    selection,
                    user.clone(),
                    item_access,
                    headers,
                    via_token,
                )
                .await
            {
                Ok(xml) => output.push_str(&xml),
                Err(error) => {
                    let status = error
                        .downcast_ref::<io::Error>()
                        .map(filesystem_status)
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                    output.push_str(&self.dav_status(&item_path, status));
                }
            }
        }
        res_multistatus(res, &output);
        Ok(())
    }

    async fn handle_propfind_file(
        &self,
        path: &Path,
        selection: &DavSelection,
        headers: &HeaderMap<HeaderValue>,
        res: &mut Response,
    ) -> Result<()> {
        if let Some(pathitem) = self.to_pathitem(path, &self.args.serve_path).await? {
            res_multistatus(
                res,
                &self
                    .dav_item_xml(&pathitem, selection, None, None, headers, false)
                    .await?,
            );
        } else {
            status_not_found(res);
        }
        Ok(())
    }

    pub(self) async fn dav_item_xml(
        &self,
        item: &PathItem,
        selection: &DavSelection,
        user: Option<String>,
        access: Option<AccessPaths>,
        headers: &HeaderMap<HeaderValue>,
        via_token: bool,
    ) -> Result<String> {
        let path = self.args.serve_path.join(&item.name);
        let mut extra = Vec::new();
        if item.is_dir()
            && (selection.extension("quota-available-bytes")
                || selection.extension("quota-used-bytes"))
        {
            extra.extend(dav_common::quota_properties(&path).await);
        }
        if item.is_dir() {
            if selection.extension("supported-report-set") {
                extra.push(("supported-report-set", "<D:supported-report><D:report><D:sync-collection/></D:report></D:supported-report>".into()));
            }
            if selection.extension("sync-token") {
                if matches!(selection, DavSelection::Names) {
                    extra.push(("sync-token", String::new()));
                } else if let Some(access) = access.clone() {
                    if let Ok(token) = self
                        .sync_collection_token(&path, user.clone(), access, via_token)
                        .await
                    {
                        extra.push(("sync-token", escape_str_pcdata(&token).to_string()));
                    }
                }
            }
        }
        let representation = if selection.needs_representation() {
            self.dav_representation(&path, item.is_dir(), user, access)
                .await?
        } else {
            Response::default()
        };
        Ok(item.to_dav_xml(
            self.args.uri_prefix.as_str(),
            selection,
            &representation,
            &extra,
            DavPrefer::parse(headers).minimal,
        ))
    }

    // Live HTTP properties are derived from the same representation as GET.
    async fn dav_validators(
        &self,
        path: &Path,
        meta: Option<&Metadata>,
        user: Option<String>,
        access: Option<AccessPaths>,
    ) -> Result<Option<(ETag, LastModified)>> {
        let Some(meta) = meta else {
            return Ok(None);
        };
        if meta.is_file() {
            return Ok(extract_cache_headers(meta));
        }
        if meta.is_dir()
            && (self.args.render_index || self.args.render_spa || self.args.render_try_index)
        {
            let representation = self.dav_representation(path, true, user, access).await?;
            return Ok(representation
                .headers()
                .typed_get::<ETag>()
                .zip(representation.headers().typed_get::<LastModified>()));
        }
        Ok(None)
    }

    async fn dav_representation(
        &self,
        path: &Path,
        directory: bool,
        user: Option<String>,
        access: Option<AccessPaths>,
    ) -> Result<Response> {
        let mut response = Response::default();
        if directory {
            if let Some(access) = access {
                if self.args.render_index || self.args.render_spa || self.args.render_try_index {
                    self.handle_render_index(
                        path,
                        &HashMap::new(),
                        &HeaderMap::new(),
                        false,
                        user,
                        access,
                        &mut response,
                    )
                    .await?;
                } else {
                    self.handle_ls_dir(
                        path,
                        true,
                        &HashMap::new(),
                        false,
                        user,
                        access,
                        &mut response,
                    )
                    .await?;
                }
            }
        } else if fs::metadata(path).await.is_ok_and(|meta| meta.is_file()) {
            self.handle_send_file(path, &HeaderMap::new(), true, &mut response)
                .await?;
        }
        Ok(response)
    }

    async fn handle_mkcol(&self, path: &Path, res: &mut Response) -> Result<()> {
        if !has_directory_parent(path).await {
            *res.status_mut() = StatusCode::CONFLICT;
            return Ok(());
        }
        fs::create_dir(path).await?;
        *res.status_mut() = StatusCode::CREATED;
        Ok(())
    }

    async fn handle_copy(
        &self,
        path: &Path,
        dest: PathBuf,
        headers: &HeaderMap<HeaderValue>,
        validators: Option<&(ETag, LastModified)>,
        user: Option<&str>,
        res: &mut Response,
    ) -> Result<()> {
        let meta = fs::metadata(path).await?;
        let shallow = match headers.get("Depth").map(|v| v.as_bytes()) {
            Some(b"0") => true,
            None | Some(b"infinity") => false,
            _ => {
                DufsErrorCode::InvalidDepth.respond(res);
                return Ok(());
            }
        };

        if self.guard_root_contained(&dest).await {
            DufsErrorCode::InvalidDestination.respond(res);
            return Ok(());
        }

        let source_real = canonical_write_path(path).await;
        let dest_real = canonical_write_path(&dest).await;
        if source_real.starts_with(&dest_real) || dest_real.starts_with(&source_real) {
            DufsErrorCode::Forbidden.respond(res);
            return Ok(());
        }
        let existed = fs::symlink_metadata(&dest).await.is_ok();
        let overwrite = match headers.get("Overwrite").map(|v| v.as_bytes()) {
            None | Some(b"T") => true,
            Some(b"F") => false,
            _ => {
                *res.status_mut() = StatusCode::BAD_REQUEST;
                return Ok(());
            }
        };
        if fs::symlink_metadata(&dest).await.is_ok() {
            if !overwrite {
                DufsErrorCode::DestinationExists.respond(res);
                return Ok(());
            }
            if !self.args.allow_delete {
                DufsErrorCode::DeleteDisabled.respond(res);
                return Ok(());
            }
        }

        if !has_directory_parent(&dest).await {
            *res.status_mut() = StatusCode::CONFLICT;
            return Ok(());
        }

        if self
            .args
            .auth
            .target_has_restricted_descendant(
                user,
                &normalize_path(dest.strip_prefix(&self.args.serve_path)?),
                &self.args.serve_path,
            )
            .await
        {
            DufsErrorCode::ReadonlyDescendant.respond(res);
            return Ok(());
        }
        if !check_preconditions(headers, validators, true, false, res) {
            return Ok(());
        }
        if meta.is_dir() {
            if !shallow && fs::symlink_metadata(path).await?.is_symlink() {
                DufsErrorCode::Forbidden.respond(res);
                return Ok(());
            }
            let parent = dest.parent().ok_or_else(|| anyhow!("Missing parent"))?;
            let staging = tempfile::Builder::new()
                .prefix(UPLOAD_TEMP_PREFIX)
                .tempdir_in(parent)?;
            let tree = staging.path().join("tree");
            fs::create_dir(&tree).await?;
            if !shallow {
                let source = path.to_path_buf();
                let entries = tokio::task::spawn_blocking(move || {
                    WalkDir::new(source)
                        .follow_links(false)
                        .min_depth(1)
                        .into_iter()
                        .filter_entry(|entry| {
                            !entry.file_name().to_str().is_some_and(is_upload_temp)
                        })
                        .collect::<Vec<_>>()
                })
                .await?;
                for entry in entries {
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(error) => {
                            let member = error.path().unwrap_or(path);
                            let status = error
                                .io_error()
                                .map(filesystem_status)
                                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                            self.copy_member_failed(path, member, status, res);
                            return Ok(());
                        }
                    };
                    let from = entry.path();
                    if !self.path_can_access(from, &Method::GET, user).await
                        || self.guard_root_contained(from).await
                    {
                        DufsErrorCode::Forbidden.respond(res);
                        return Ok(());
                    }
                    let to = tree.join(from.strip_prefix(path)?);
                    let result = if entry.file_type().is_dir() {
                        fs::create_dir(&to).await
                    } else if entry.file_type().is_file() {
                        fs::copy(from, &to).await.map(|_| ())
                    } else {
                        self.copy_member_failed(path, from, StatusCode::FORBIDDEN, res);
                        return Ok(());
                    };
                    if let Err(error) = result {
                        self.copy_member_failed(path, from, filesystem_status(&error), res);
                        return Ok(());
                    }
                }
            }
            replace_destination(&tree, &dest, existed).await?;
            *res.status_mut() = if existed {
                StatusCode::NO_CONTENT
            } else {
                StatusCode::CREATED
            };
            return Ok(());
        }

        let dest = upload_target(&dest).await?;
        let old_meta = fs::metadata(&dest).await.ok();
        if let Some(meta) = &old_meta {
            if meta.is_file() {
                fs::OpenOptions::new().write(true).open(&dest).await?;
            } else if !meta.is_dir() {
                return Err(anyhow!("Cannot replace a non-regular file"));
            }
        }
        let mut source = fs::File::open(path).await?;
        let (file, temp) = upload_temp(&dest)?;
        let mut file = fs::File::from_std(file);
        io::copy(&mut source, &mut file).await?;
        file.flush().await?;
        file.set_permissions(upload_permissions(&source.metadata().await?))
            .await?;
        finish_write(&file, old_meta.as_ref()).await?;
        drop(file);
        if old_meta.as_ref().is_some_and(|m| m.is_dir()) {
            replace_destination(temp.as_ref(), &dest, true).await?;
        } else {
            temp.persist(&dest)?;
        }
        set_written_etag(&dest, res).await;

        *res.status_mut() = if existed {
            StatusCode::NO_CONTENT
        } else {
            StatusCode::CREATED
        };
        Ok(())
    }

    async fn handle_move(
        &self,
        path: &Path,
        dest: PathBuf,
        headers: &HeaderMap<HeaderValue>,
        validators: Option<&(ETag, LastModified)>,
        res: &mut Response,
    ) -> Result<()> {
        // A relative symlink resolves against its own parent directory, so moving
        // it to a different parent changes what it points at without changing the
        // request path. The target check below would catch the new target, but
        // refusing the move as well closes the race between that check and the
        // following filesystem operation. Renaming within the same parent
        // preserves every relative target, so only the target permission check
        // applies to it.
        if self.args.auth.has_rules()
            && path.parent() != dest.parent()
            && contains_symlink(path).await
        {
            DufsErrorCode::Forbidden.respond_with(
                res,
                "Forbidden: cannot move symlinks when access rules are enabled",
            );
            return Ok(());
        }

        if self.guard_root_contained(&dest).await {
            DufsErrorCode::InvalidDestination.respond(res);
            return Ok(());
        }

        let source_real = canonical_write_path(path).await;
        let dest_real = canonical_write_path(&dest).await;
        if source_real.starts_with(&dest_real) || dest_real.starts_with(&source_real) {
            DufsErrorCode::Forbidden.respond(res);
            return Ok(());
        }
        let existed = fs::symlink_metadata(&dest).await.is_ok();
        let overwrite = match headers.get("Overwrite").map(|v| v.as_bytes()) {
            None | Some(b"T") => true,
            Some(b"F") => false,
            _ => {
                *res.status_mut() = StatusCode::BAD_REQUEST;
                return Ok(());
            }
        };
        if fs::symlink_metadata(&dest).await.is_ok() && !overwrite {
            DufsErrorCode::DestinationExists.respond(res);
            return Ok(());
        }

        if !has_directory_parent(&dest).await {
            *res.status_mut() = StatusCode::CONFLICT;
            return Ok(());
        }

        if headers
            .get("Depth")
            .is_some_and(|v| v.as_bytes() != b"infinity")
        {
            DufsErrorCode::InvalidDepth.respond(res);
            return Ok(());
        }
        if !check_preconditions(headers, validators, true, false, res) {
            return Ok(());
        }
        replace_destination(path, &dest, existed).await?;
        set_written_etag(&dest, res).await;

        *res.status_mut() = if existed {
            StatusCode::NO_CONTENT
        } else {
            StatusCode::CREATED
        };
        Ok(())
    }

    async fn handle_proppatch(
        &self,
        req_path: &str,
        path: &Path,
        directory: bool,
        user: Option<String>,
        access: AccessPaths,
        body: &[u8],
        prefer: DavPrefer,
        res: &mut Response,
    ) -> Result<()> {
        let properties = match parse_propertyupdate(body) {
            Ok(properties) => properties,
            Err(_) => {
                *res.status_mut() = StatusCode::BAD_REQUEST;
                return Ok(());
            }
        };
        let representation = self
            .dav_representation(path, directory, user, Some(access))
            .await?;
        let protected = |name: &OwnedName| {
            if name.namespace.as_deref() != Some("DAV:") {
                return false;
            }
            match name.local_name.as_str() {
                "resourcetype"
                | "displayname"
                | "quota-available-bytes"
                | "quota-used-bytes"
                | "sync-token"
                | "supported-report-set" => true,
                "getcontenttype" => representation.headers().contains_key("content-type"),
                "getcontentlength" => representation.headers().contains_key("content-length"),
                "getlastmodified" => representation.headers().contains_key("last-modified"),
                "getetag" => representation.headers().contains_key("etag"),
                "getcontentlanguage" => representation.headers().contains_key("content-language"),
                _ => false,
            }
        };
        // No dead-property store. Remove of an absent property is a valid
        // no-op; any rejected instruction rolls back every other instruction.
        let mut denied = String::new();
        let mut protected_denied = String::new();
        let mut absent = String::new();
        for (name, set) in properties {
            let xml = dav_empty_property(&name);
            if protected(&name) {
                protected_denied.push_str(&xml);
            } else if set {
                denied.push_str(&xml);
            } else {
                absent.push_str(&xml);
            }
        }
        let rejected = !denied.is_empty() || !protected_denied.is_empty();
        if !rejected && prefer.minimal {
            status_no_content(res);
            prefer.applied(res, true, false, false);
            return Ok(());
        }
        let mut output = format!(
            "<D:response><D:href>{}</D:href>",
            escape_str_pcdata(req_path)
        );
        if !denied.is_empty() {
            output.push_str(&dav_propstat(&denied, "403 Forbidden"));
        }
        if !protected_denied.is_empty() {
            let mut stat = dav_propstat(&protected_denied, "403 Forbidden");
            stat = stat.replace(
                "</D:propstat>",
                "<D:error><D:cannot-modify-protected-property/></D:error></D:propstat>",
            );
            output.push_str(&stat);
        }
        if !absent.is_empty() {
            output.push_str(&dav_propstat(
                &absent,
                if rejected {
                    "424 Failed Dependency"
                } else {
                    "200 OK"
                },
            ));
        }
        output.push_str("</D:response>");
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
                paths.reverse();
                // keep directories grouped on top, in reverse order within the group
                paths.sort_by_key(|v| !v.path_type.is_dir());
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
                        format!("{}/\n", displayname)
                    } else {
                        format!("{}\n", displayname)
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
        let readwrite =
            access_paths.perm().readwrite() && self.path_readwrite(path, user.as_deref()).await;
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
            self.render_html(&self.html)
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

    fn auth_reject(&self, res: &mut Response, stale: bool) -> Result<()> {
        set_webdav_headers(
            res,
            self.args.allow_upload,
            self.args.allow_delete,
            false,
            false,
            false,
            false,
        );

        www_authenticate(res, &self.args, stale)?;
        *res.status_mut() = StatusCode::UNAUTHORIZED;
        Ok(())
    }

    async fn guard_root_contained(&self, path: &Path) -> bool {
        if self.args.allow_symlink {
            return false;
        }
        let mut check_path = path.to_path_buf();
        while fs::symlink_metadata(&check_path).await.is_err() {
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

    async fn extract_dest(&self, req: &Request, res: &mut Response) -> Option<PathBuf> {
        let headers = req.headers();
        let dest_path = match self
            .extract_destination_header(headers)
            .and_then(|dest| self.resolve_path(&dest))
        {
            Some(dest) => dest,
            None => {
                DufsErrorCode::InvalidDestination.respond(res);
                return None;
            }
        };

        let authorization = headers.get(AUTHORIZATION);
        let request_target = req.uri().to_string();
        let guard = self
            .args
            .auth
            .guard(
                &dest_path,
                req.method(),
                &request_target,
                authorization,
                None,
                false,
                &self.args.serve_path,
            )
            .await;

        match guard {
            (_, Some(access), _, _) if !Self::has_restricted_descendant(&access) => {}
            (_, Some(_), _, _) => {
                DufsErrorCode::ReadonlyDescendant.respond(res);
                return None;
            }
            _ => {
                DufsErrorCode::Forbidden.respond(res);
                return None;
            }
        };

        let dest = match self.join_path(&dest_path) {
            Some(dest) => dest,
            None => {
                DufsErrorCode::InvalidDestination.respond(res);
                return None;
            }
        };

        Some(dest)
    }

    fn extract_destination_header(&self, headers: &HeaderMap<HeaderValue>) -> Option<String> {
        let dest = headers.get("Destination")?.to_str().ok()?;
        dav_uri_path(dest, headers)
    }

    fn resolve_path(&self, path: &str) -> Option<String> {
        let path = decode_uri(path)?;
        let path = path.trim_matches('/');
        let mut parts = vec![];
        for comp in Path::new(path).components() {
            if let Component::Normal(v) = comp {
                let v = v.to_string_lossy();
                if is_upload_temp(&v) {
                    return None;
                }
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
        if new_path == path_prefix {
            return Some(String::new());
        }
        new_path
            .strip_prefix(&format!("{path_prefix}/"))
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
        join_within(&self.args.serve_path, &path)
    }

    /// Path components of `path` relative to the served root, used to resolve
    /// auth rules and filesystem aliases of configured names.
    fn relative_parts<'a>(&self, path: &'a Path) -> Vec<&'a str> {
        path.strip_prefix(&self.args.serve_path)
            .unwrap_or(Path::new(""))
            .components()
            .filter_map(|component| match component {
                Component::Normal(name) => name.to_str(),
                _ => None,
            })
            .collect()
    }

    fn set_item_access(&self, item: &mut PathItem, access: EntryAccess) {
        let readwrite = access.perm.readwrite();
        let restricted = item.is_dir() && access.restricted;
        item.allow_upload = self.args.allow_upload && readwrite;
        item.allow_delete = self.args.allow_delete && readwrite && !restricted;
        item.readonly_descendant = restricted;
    }

    async fn list_dir(
        &self,
        entry_path: &Path,
        base_path: &Path,
        access_paths: &AccessPaths,
        user: Option<&str>,
    ) -> Result<Vec<PathItem>> {
        let mut paths: Vec<PathItem> = vec![];
        let prefix = self.relative_parts(entry_path);
        if access_paths.perm().indexonly() {
            for name in access_paths.child_names() {
                let entry_path = entry_path.join(name);
                self.add_pathitem(
                    &mut paths,
                    base_path,
                    &entry_path,
                    access_paths,
                    &prefix,
                    user,
                )
                .await;
            }
        } else {
            let mut rd = fs::read_dir(entry_path).await?;
            while let Ok(Some(entry)) = rd.next_entry().await {
                let entry_path = entry.path();
                self.add_pathitem(
                    &mut paths,
                    base_path,
                    &entry_path,
                    access_paths,
                    &prefix,
                    user,
                )
                .await;
            }
        }
        Ok(paths)
    }

    async fn add_pathitem(
        &self,
        paths: &mut Vec<PathItem>,
        base_path: &Path,
        entry_path: &Path,
        access_paths: &AccessPaths,
        prefix: &[&str],
        user: Option<&str>,
    ) {
        let base_name = get_file_name(entry_path);
        if let Ok(Some(mut item)) = self.to_pathitem(entry_path, base_path).await {
            if is_hidden(&self.args.hidden, base_name, item.is_dir()) {
                return;
            }
            let Some(access) = self
                .entry_access(access_paths, entry_path, base_name, prefix, user)
                .await
            else {
                return;
            };
            self.set_item_access(&mut item, access);
            paths.push(item);
        }
    }

    /// Resolve the displayed access of an entry from the request path, then
    /// from its canonical target when the entry is reached through a link.
    ///
    /// Returns `None` when the target is not readable, so listings and search
    /// results hide the same entries.
    async fn entry_access(
        &self,
        access_paths: &AccessPaths,
        entry_path: &Path,
        entry_name: &str,
        prefix: &[&str],
        user: Option<&str>,
    ) -> Option<EntryAccess> {
        let mut access = access_paths
            .entry_access(entry_name, prefix, Some(&self.args.serve_path))
            .await;
        if self.args.auth.has_rules() && self.path_resolves_elsewhere(entry_path).await {
            // The entry is reached through a link, either as the entry itself
            // or as a parent component. Check the canonical target so a
            // listing cannot expose entries the target rule denies.
            if !self.path_can_access(entry_path, &Method::GET, user).await {
                return None;
            }
            // Display permission follows both the request path and the
            // canonical target, so write controls are not advertised for
            // entries that the target rule makes read-only.
            if access.perm.readwrite() && !self.path_readwrite(entry_path, user).await {
                access.perm = AccessPerm::ReadOnly;
            }
            // Recursive delete follows the target subtree as well, so a
            // read-only rule below the target blocks it even when the request
            // path has no such rule. A symlink entry itself can still be
            // removed, since that only unlinks it.
            if access.perm.readwrite()
                && !access.restricted
                && !self.path_is_symlink(entry_path).await
                && self
                    .args
                    .auth
                    .target_has_restricted_descendant(
                        user,
                        &self.relative_parts(entry_path).join("/"),
                        &self.args.serve_path,
                    )
                    .await
            {
                access.restricted = true;
            }
        }
        Some(access)
    }

    /// Whether `path` does not name its canonical target directly, for example
    /// because it is a symbolic link or a parent component is one.
    ///
    /// Dangling links do not canonicalize and are reported as aliases so the
    /// authorization check still runs for them.
    async fn path_resolves_elsewhere(&self, path: &Path) -> bool {
        match fs::canonicalize(path).await {
            Ok(canonical) => canonical != path,
            Err(_) => true,
        }
    }

    /// Whether the final component of `path` is a symbolic link. Intermediate
    /// links are followed, since they make the path name a real target entry.
    async fn path_is_symlink(&self, path: &Path) -> bool {
        fs::symlink_metadata(path)
            .await
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or_default()
    }

    /// Whether `user` may perform `method` on `path`, accounting for the
    /// canonical target of any symbolic link on the way.
    async fn path_can_access(&self, path: &Path, method: &Method, user: Option<&str>) -> bool {
        let rel_path = self.relative_parts(path).join("/");
        self.args
            .auth
            .can_access(user, &rel_path, method, &self.args.serve_path)
            .await
    }

    /// Whether `path` grants write permission, including its canonical target.
    async fn path_readwrite(&self, path: &Path, user: Option<&str>) -> bool {
        self.path_can_access(path, &Method::PUT, user).await
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
        let etag = match path_type {
            PathType::File | PathType::SymlinkFile => etag_from_metadata(&meta),
            PathType::Dir | PathType::SymlinkDir => None,
        };
        let rel_path = path.strip_prefix(base_path)?;
        let name = normalize_path(rel_path);
        Ok(Some(PathItem {
            path_type,
            name,
            mtime,
            size,
            etag,
            allow_upload: false,
            allow_delete: false,
            readonly_descendant: false,
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
    pub paths: Vec<PathItem>,
}

#[derive(Debug, Serialize, Eq, PartialEq, Ord, PartialOrd)]
pub struct PathItem {
    pub path_type: PathType,
    pub name: String,
    pub mtime: u64,
    pub size: u64,
    pub etag: Option<String>,
    pub allow_upload: bool,
    pub allow_delete: bool,
    pub readonly_descendant: bool,
}

impl PathItem {
    pub fn is_dir(&self) -> bool {
        self.path_type == PathType::Dir || self.path_type == PathType::SymlinkDir
    }

    fn to_dav_xml(
        &self,
        prefix: &str,
        selection: &DavSelection,
        representation: &Response,
        extra: &[(&str, String)],
        minimal: bool,
    ) -> String {
        let mut href = encode_uri(&format!("{}{}", prefix, self.name));
        if self.is_dir() && !href.ends_with('/') {
            href.push('/');
        }
        let mut live = vec![
            (
                "displayname",
                escape_str_pcdata(self.base_name()).to_string(),
            ),
            (
                "resourcetype",
                if self.is_dir() {
                    "<D:collection/>".into()
                } else {
                    String::new()
                },
            ),
        ];
        if representation.status().is_success() {
            for (property, header) in [
                ("getcontenttype", "content-type"),
                ("getcontentlength", "content-length"),
                ("getlastmodified", "last-modified"),
                ("getetag", "etag"),
                ("getcontentlanguage", "content-language"),
            ] {
                if let Some(value) = representation
                    .headers()
                    .get(header)
                    .and_then(|v| v.to_str().ok())
                {
                    live.push((property, escape_str_pcdata(value).to_string()));
                }
            }
        }
        let mut ok = String::new();
        let mut missing = String::new();
        if !matches!(selection, DavSelection::Props(_)) {
            for (name, value) in &live {
                if matches!(selection, DavSelection::Names) {
                    ok.push_str(&format!("<D:{name}/>\n"));
                } else {
                    ok.push_str(&format!("<D:{name}>{value}</D:{name}>\n"));
                }
            }
            if matches!(selection, DavSelection::Names) {
                for (name, _) in extra {
                    ok.push_str(&format!("<D:{name}/>\n"));
                }
            }
        }
        if let DavSelection::Props(names) | DavSelection::All(names) = selection {
            for name in names {
                let value = if name.namespace.as_deref() == Some("DAV:") {
                    live.iter().find(|(key, _)| *key == name.local_name)
                } else {
                    None
                };
                if let Some((key, value)) = value {
                    if matches!(selection, DavSelection::Props(_)) {
                        ok.push_str(&format!("<D:{key}>{value}</D:{key}>\n"));
                    }
                } else if let Some((key, value)) = extra.iter().find(|(key, _)| dav_name(name, key))
                {
                    ok.push_str(&format!("<D:{key}>{value}</D:{key}>\n"));
                } else if !minimal {
                    missing.push_str(&dav_empty_property(name));
                }
            }
        }
        let mut output = format!(
            "<D:response>\n<D:href>{}</D:href>\n",
            escape_str_pcdata(&href)
        );
        if !ok.is_empty() || missing.is_empty() {
            output.push_str(&dav_propstat(&ok, "200 OK"));
        }
        if !missing.is_empty() {
            output.push_str(&dav_propstat(&missing, "404 Not Found"));
        }
        output.push_str("</D:response>");
        output
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
}

fn to_timestamp(time: &SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Build an ETag from file metadata without reading the file contents.
/// Nanosecond mtime and, on Unix, device/inode reduce collisions between files.
fn etag_from_metadata(meta: &Metadata) -> Option<String> {
    let mtime = meta.modified().ok()?;
    let timestamp = mtime
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let size = meta.len();

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(format!(
            r#""{timestamp}-{size}-{:x}-{:x}-{:x}-{:x}""#,
            meta.dev(),
            meta.ino(),
            meta.ctime(),
            meta.ctime_nsec()
        ))
    }

    #[cfg(not(unix))]
    {
        let created = meta
            .created()
            .ok()
            .and_then(|v| v.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|v| v.as_nanos())
            .unwrap_or_default();
        Some(format!(r#""{timestamp}-{size}-{created}""#))
    }
}

fn normalize_path<P: AsRef<Path>>(path: P) -> String {
    let path = path.as_ref().to_str().unwrap_or_default();
    if cfg!(windows) {
        path.replace('\\', "/")
    } else {
        path.to_string()
    }
}

fn is_write_method(method: &Method) -> bool {
    matches!(
        method.as_str(),
        "PUT" | "PATCH" | "DELETE" | "MKCOL" | "MOVE" | "COPY"
    )
}

// Canonicalize the existing prefix even when the final name is not present.
async fn canonical_write_path(path: &Path) -> PathBuf {
    let mut ancestor = path;
    let mut missing = vec![];
    loop {
        if let Ok(mut canonical) = fs::canonicalize(ancestor).await {
            for name in missing.into_iter().rev() {
                canonical.push(name);
            }
            return canonical;
        }
        match (ancestor.parent(), ancestor.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name);
                ancestor = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

async fn write_lock_keys(path: &Path, dest: &[PathBuf]) -> BTreeMap<PathBuf, bool> {
    let mut keys = BTreeMap::new();
    for path in std::iter::once(path).chain(dest.iter().map(PathBuf::as_path)) {
        let canonical = canonical_write_path(path).await;
        // Keep the directory entry locked as well as its resolved target.
        let entry = match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) => canonical_write_path(parent).await.join(name),
            _ => canonical.clone(),
        };
        for key in [path.to_path_buf(), canonical, entry] {
            #[cfg(windows)]
            let key = PathBuf::from(key.to_string_lossy().to_lowercase());
            keys.insert(key.clone(), true);
            for ancestor in key.ancestors().skip(1) {
                keys.entry(ancestor.to_path_buf()).or_insert(false);
            }
        }
    }
    keys
}

async fn upload_target(path: &Path) -> Result<PathBuf> {
    // Preserve PUT/COPY's existing behavior of following a final symlink.
    if fs::symlink_metadata(path)
        .await
        .is_ok_and(|m| m.is_symlink())
    {
        Ok(fs::canonicalize(path).await?)
    } else {
        Ok(path.to_path_buf())
    }
}

fn upload_permissions(meta: &Metadata) -> std::fs::Permissions {
    let permissions = meta.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return std::fs::Permissions::from_mode(permissions.mode() & 0o777);
    }
    #[cfg(not(unix))]
    permissions
}

fn upload_temp(path: &Path) -> Result<(std::fs::File, tempfile::TempPath)> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(UPLOAD_TEMP_PREFIX);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o666));
    }
    Ok(builder
        .tempfile_in(path.parent().ok_or_else(|| anyhow!("Missing parent"))?)?
        .into_parts())
}

async fn finish_write(file: &fs::File, old_meta: Option<&Metadata>) -> Result<()> {
    // PATCH keeps its inode; non-Unix filesystems may not expose file identity.
    // If metadata would produce the same validator, advance mtime and verify it.
    if let Some(old) = old_meta {
        let unchanged = |meta: &Metadata| {
            #[cfg(unix)]
            {
                etag_from_metadata(meta) == etag_from_metadata(old)
            }
            #[cfg(not(unix))]
            {
                meta.modified().ok() <= old.modified().ok()
            }
        };
        if unchanged(&file.metadata().await?) {
            let time = old.modified()?;
            let std_file = file.try_clone().await?.into_std().await;
            for nanos in [1, 1_000_000, 1_000_000_000, 2_000_000_000] {
                std_file.set_modified(time + std::time::Duration::from_nanos(nanos))?;
                if !unchanged(&file.metadata().await?) {
                    break;
                }
            }
            if unchanged(&file.metadata().await?) {
                return Err(anyhow!("Could not change file validator"));
            }
        }
    }
    file.sync_all().await?;
    Ok(())
}

async fn set_written_etag(path: &Path, res: &mut Response) {
    if let Ok(meta) = fs::metadata(path).await {
        if meta.is_file() {
            if let Some((etag, _)) = extract_cache_headers(&meta) {
                res.headers_mut().typed_insert(etag);
            }
        }
    }
}

// An overwritten collection must lose its previous members, not merge them.
fn filesystem_status(error: &std::io::Error) -> StatusCode {
    match error.kind() {
        io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
        io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
        io::ErrorKind::AlreadyExists
        | io::ErrorKind::DirectoryNotEmpty
        | io::ErrorKind::NotADirectory => StatusCode::CONFLICT,
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded => {
            StatusCode::INSUFFICIENT_STORAGE
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

async fn replace_destination(source: &Path, dest: &Path, existed: bool) -> Result<()> {
    if !existed
        || (fs::symlink_metadata(source).await?.is_file()
            && fs::symlink_metadata(dest).await?.is_file())
    {
        fs::rename(source, dest).await?;
        return Ok(());
    }
    let parent = dest.parent().ok_or_else(|| anyhow!("Missing parent"))?;
    let backup = tempfile::Builder::new()
        .prefix(UPLOAD_TEMP_PREFIX)
        .tempdir_in(parent)?;
    let old = backup.path().join("previous");
    fs::rename(dest, &old).await?;
    if let Err(error) = fs::rename(source, dest).await {
        if let Err(restore) = fs::rename(&old, dest).await {
            let retained = backup.keep();
            return Err(anyhow!("Could not replace destination: {error}; could not restore: {restore}; previous data retained at {}", retained.display()));
        }
        return Err(error.into());
    }
    Ok(())
}

async fn has_directory_parent(path: &Path) -> bool {
    match path.parent() {
        Some(parent) => fs::metadata(parent).await.is_ok_and(|meta| meta.is_dir()),
        None => false,
    }
}

/// Whether `path` is a symlink, or a directory containing one at any depth.
///
/// `MOVE` relocates a link without changing its text, so a relative target can
/// begin resolving somewhere else after the move. This check lets the caller
/// refuse such moves when path authorization is enabled.
async fn contains_symlink(path: &Path) -> bool {
    match fs::symlink_metadata(path).await {
        Ok(meta) if meta.is_symlink() => true,
        Ok(meta) if meta.is_dir() => {
            let path = path.to_path_buf();
            tokio::task::spawn_blocking(move || {
                WalkDir::new(path)
                    .follow_links(false)
                    .into_iter()
                    .any(|entry| match entry {
                        Ok(entry) => entry.file_type().is_symlink(),
                        // An unreadable subtree could hide a link; fail closed.
                        Err(_) => true,
                    })
            })
            .await
            // Fail closed if the blocking task cannot run.
            .unwrap_or(true)
        }
        _ => false,
    }
}

fn add_cors(res: &mut Response) {
    // A wildcard origin cannot be combined with credentialed requests, so the
    // credentials header is deliberately omitted. dufs authenticates with
    // Basic/Digest rather than cookies, and CORS stays as permissive as
    // possible for every other request.
    res.headers_mut()
        .typed_insert(AccessControlAllowOrigin::ANY);
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
        HeaderValue::from_static("*"),
    );
}

struct DavIfGroup {
    resource: Option<String>,
    lists: Vec<Vec<(bool, DavCondition)>>,
}

enum DavCondition {
    Tag(ETag),
    Token(String),
}

impl DavIfGroup {
    fn needs_sync(&self) -> bool {
        self.lists.iter().flatten().any(|(_, cond)| matches!(cond, DavCondition::Token(token) if token.starts_with("urn:uuid:")))
    }
    fn matches(&self, current: Option<&ETag>, sync: Option<&str>) -> bool {
        self.lists.iter().any(|list| {
            list.iter().all(|(negated, tag)| {
                let matches = match tag {
                    DavCondition::Tag(tag) => current.is_some_and(|current| {
                        let condition = IfNoneMatch::from(tag.clone());
                        !condition.precondition_passes(current)
                            || http_encoding::zstd_etag(current).is_some_and(|encoded| !condition.precondition_passes(&encoded))
                    }),
                    DavCondition::Token(token) => sync == Some(token.as_str()),
                };
                matches != *negated
            })
        })
    }
}

fn dav_uri_path(value: &str, headers: &HeaderMap<HeaderValue>) -> Option<String> {
    let uri: Uri = value.parse().ok()?;
    if let Some(authority) = uri.authority() {
        let host = headers.get("Host")?.to_str().ok()?;
        if !authority.as_str().eq_ignore_ascii_case(host)
            || !matches!(uri.scheme_str(), Some("http" | "https"))
        {
            return None;
        }
    }
    if !uri.path().starts_with('/') || uri.query().is_some() {
        return None;
    }
    Some(uri.path().to_string())
}

fn parse_dav_if(mut input: &str) -> Result<Vec<DavIfGroup>> {
    input = input.trim_start();
    let tagged = input.starts_with('<');
    let mut groups = vec![];
    while !input.is_empty() {
        let resource = if tagged {
            let (uri, rest) = input
                .strip_prefix('<')
                .and_then(|s| s.split_once('>'))
                .ok_or_else(|| anyhow!("Invalid resource tag"))?;
            input = rest.trim_start();
            Some(uri.to_string())
        } else {
            None
        };
        let mut lists = vec![];
        while let Some(rest) = input.strip_prefix('(') {
            input = rest;
            let mut conditions = vec![];
            loop {
                input = input.trim_start();
                if let Some(rest) = input.strip_prefix(')') {
                    input = rest.trim_start();
                    break;
                }
                let negated = input.strip_prefix("Not").is_some_and(|rest| {
                    rest.starts_with(char::is_whitespace) || rest.starts_with(['<', '['])
                });
                if negated {
                    input = input[3..].trim_start();
                }
                let tag = if let Some(rest) = input.strip_prefix('<') {
                    let (value, rest) = rest
                        .split_once('>')
                        .ok_or_else(|| anyhow!("Unclosed state token"))?;
                    if !value.contains(':') || value.contains(char::is_whitespace) {
                        return Err(anyhow!("Invalid state token"));
                    }
                    input = rest;
                    DavCondition::Token(value.to_owned())
                } else if let Some(rest) = input.strip_prefix('[') {
                    let first = rest
                        .find('"')
                        .ok_or_else(|| anyhow!("Invalid entity tag"))?;
                    let end = rest[first + 1..]
                        .find('"')
                        .ok_or_else(|| anyhow!("Invalid entity tag"))?
                        + first
                        + 1;
                    input = rest[end + 1..]
                        .strip_prefix(']')
                        .ok_or_else(|| anyhow!("Invalid entity tag"))?;
                    DavCondition::Tag(rest[..=end].parse::<ETag>()?)
                } else {
                    return Err(anyhow!("Invalid If condition"));
                };
                conditions.push((negated, tag));
            }
            if conditions.is_empty() {
                return Err(anyhow!("Empty If list"));
            }
            lists.push(conditions);
        }
        if lists.is_empty() || (!tagged && !input.is_empty()) {
            return Err(anyhow!("Invalid If header"));
        }
        groups.push(DavIfGroup { resource, lists });
    }
    if groups.is_empty() {
        return Err(anyhow!("Empty If header"));
    }
    Ok(groups)
}

fn dav_xml_reader(body: &[u8]) -> EventReader<&[u8]> {
    ParserConfig::new()
        .allow_multiple_root_elements(false)
        .max_data_length(65536)
        .max_entity_expansion_length(65536)
        .create_reader(body)
}

enum DavSelection {
    All(Vec<OwnedName>),
    Names,
    Props(Vec<OwnedName>),
}

fn dav_name(name: &OwnedName, local: &str) -> bool {
    name.namespace.as_deref() == Some("DAV:") && name.local_name == local
}

impl DavSelection {
    fn needs_representation(&self) -> bool {
        match self {
            Self::Props(names) => names.iter().any(|name| {
                name.namespace.as_deref() == Some("DAV:")
                    && matches!(
                        name.local_name.as_str(),
                        "getcontenttype"
                            | "getcontentlength"
                            | "getlastmodified"
                            | "getetag"
                            | "getcontentlanguage"
                    )
            }),
            _ => true,
        }
    }

    fn extension(&self, property: &str) -> bool {
        match self {
            Self::Names => true,
            Self::All(names) | Self::Props(names) => {
                names.iter().any(|name| dav_name(name, property))
            }
        }
    }

    fn parse(body: &[u8]) -> Result<Self> {
        if body.is_empty() {
            return Ok(Self::All(vec![]));
        }
        let mut stack: Vec<Option<OwnedName>> = vec![];
        let mut mode = None;
        let mut props = vec![];
        let mut included = false;
        for event in dav_xml_reader(body) {
            match event? {
                XmlEvent::StartElement { name, .. } => {
                    let understood = if stack.last().is_some_and(Option::is_none) {
                        false
                    } else {
                        match stack.len() {
                            0 => {
                                if !dav_name(&name, "propfind") {
                                    return Err(anyhow!("Invalid PROPFIND root"));
                                }
                                true
                            }
                            1 if dav_name(&name, "include") => {
                                if included {
                                    return Err(anyhow!("Duplicate include"));
                                }
                                included = true;
                                true
                            }
                            1 if name.namespace.as_deref() == Some("DAV:")
                                && matches!(
                                    name.local_name.as_str(),
                                    "allprop" | "propname" | "prop"
                                ) =>
                            {
                                if mode.is_some() {
                                    return Err(anyhow!("Duplicate PROPFIND selection"));
                                }
                                mode = Some(match name.local_name.as_str() {
                                    "allprop" => "all",
                                    "propname" => "names",
                                    _ => "props",
                                });
                                true
                            }
                            2 if stack[1].as_ref().is_some_and(|parent| {
                                dav_name(parent, "prop") || dav_name(parent, "include")
                            }) =>
                            {
                                props.push(name.clone());
                                true
                            }
                            _ => false, // RFC 4918 §17: ignore extension elements.
                        }
                    };
                    stack.push(understood.then_some(name));
                }
                XmlEvent::EndElement { .. } => {
                    stack.pop();
                }
                XmlEvent::Characters(text) | XmlEvent::CData(text)
                    if !text.trim().is_empty() && !stack.last().is_some_and(Option::is_none) =>
                {
                    return Err(anyhow!("Invalid PROPFIND XML"))
                }
                _ => {}
            }
        }
        if included && mode != Some("all") {
            return Err(anyhow!("include requires allprop"));
        }
        match mode {
            Some("all") => Ok(Self::All(props)),
            Some("names") => Ok(Self::Names),
            Some("props") => Ok(Self::Props(props)),
            _ => Err(anyhow!("Missing PROPFIND selection")),
        }
    }
}

fn parse_propertyupdate(body: &[u8]) -> Result<Vec<(OwnedName, bool)>> {
    let mut stack: Vec<Option<OwnedName>> = vec![];
    let mut props = vec![];
    let mut action_props_start = 0;
    let mut action_has_prop = false;
    for event in dav_xml_reader(body) {
        match event? {
            XmlEvent::StartElement { name, .. } => {
                let understood = if stack.last().is_some_and(Option::is_none) {
                    false
                } else {
                    match stack.len() {
                        0 => {
                            if !dav_name(&name, "propertyupdate") {
                                return Err(anyhow!("Invalid PROPPATCH root"));
                            }
                            true
                        }
                        1 if dav_name(&name, "set") || dav_name(&name, "remove") => {
                            action_props_start = props.len();
                            action_has_prop = false;
                            true
                        }
                        2 if dav_name(&name, "prop") => {
                            if action_has_prop {
                                return Err(anyhow!("Duplicate prop"));
                            }
                            action_has_prop = true;
                            true
                        }
                        3 => {
                            props.push((
                                name.clone(),
                                stack[1]
                                    .as_ref()
                                    .is_some_and(|parent| dav_name(parent, "set")),
                            ));
                            true
                        }
                        _ => false,
                    }
                };
                stack.push(understood.then_some(name));
            }
            XmlEvent::EndElement { .. } => {
                if stack.len() == 2
                    && stack.last().is_some_and(Option::is_some)
                    && props.len() == action_props_start
                {
                    return Err(anyhow!("Empty property action"));
                }
                stack.pop();
            }
            XmlEvent::Characters(text) | XmlEvent::CData(text)
                if !text.trim().is_empty()
                    && !stack.last().is_some_and(Option::is_none)
                    && (stack.len() < 4
                        || !stack[1].as_ref().is_some_and(|name| dav_name(name, "set"))) =>
            {
                return Err(anyhow!("Invalid PROPPATCH XML"))
            }
            _ => {}
        }
    }
    if props.is_empty() {
        return Err(anyhow!("No properties"));
    }
    Ok(props)
}

fn dav_empty_property(name: &OwnedName) -> String {
    let namespace = name.namespace.as_deref().unwrap_or("");
    format!(
        "<{} xmlns=\"{}\"/>\n",
        name.local_name,
        escape_str_attribute(namespace)
    )
}

fn dav_propstat(props: &str, status: &str) -> String {
    format!("<D:propstat>\n<D:prop>\n{props}</D:prop>\n<D:status>HTTP/1.1 {status}</D:status>\n</D:propstat>\n")
}

async fn reject_nonempty_body(body: Incoming, res: &mut Response) -> bool {
    if Limited::new(body, 1)
        .collect()
        .await
        .is_ok_and(|body| body.to_bytes().is_empty())
    {
        false
    } else {
        *res.status_mut() = StatusCode::UNSUPPORTED_MEDIA_TYPE;
        true
    }
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
    compression: CompressionLevel,
    follow_symlinks: bool,
    serve_path: PathBuf,
    auth: AccessControl,
    user: Option<String>,
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
        auth,
        user,
        move |x| {
            // Include both files and directories. Symbolic links are already
            // resolved by `follow_links`, so `file_type` reports the target's
            // type.
            x.path().symlink_metadata().is_ok() && {
                let file_type = x.file_type();
                file_type.is_file() || file_type.is_dir()
            }
        },
    ))
    .await?;
    let mut zip = ZipWriter::new(&mut *writer).with_compression_level(compression);
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
        // Follow symlinks so that a symlinked entry records the permissions
        // and modification time of its target.
        let meta = match tokio::fs::metadata(&zip_path).await {
            Ok(v) => v,
            Err(_) => continue,
        };
        if meta.is_dir() {
            let options = EntryOptions::from_path_following(&zip_path).await?;
            zip.add_directory(&filename, &options).await?;
        } else {
            zip.add_file(&filename, &zip_path).await?;
        }
    }
    zip.finish().await?;
    Ok(())
}

fn extract_cache_headers(meta: &Metadata) -> Option<(ETag, LastModified)> {
    let mtime = meta.modified().ok().or_else(|| meta.created().ok())?;
    let etag = etag_from_metadata(meta)?.parse::<ETag>().ok()?;
    let last_modified = LastModified::from(mtime);
    Some((etag, last_modified))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Precondition {
    Passed,
    NotModified,
    Failed,
}

/// Evaluate the conditional request headers against the current representation.
///
/// `allow_not_modified` is true for GET/HEAD, where a matching `If-None-Match`
/// or an unchanged `If-Modified-Since` produces `304 Not Modified`. For all
/// other methods it produces `412 Precondition Failed` instead.
fn evaluate_preconditions(
    headers: &HeaderMap<HeaderValue>,
    cache_headers: Option<&(ETag, LastModified)>,
    exists: bool,
    allow_not_modified: bool,
) -> Precondition {
    // State-changing requests accept either strong validator for the same
    // file revision, so negotiated GETs remain usable for conditional writes.
    let encoded = if allow_not_modified {
        None
    } else {
        cache_headers.and_then(|(etag, _)| http_encoding::zstd_etag(etag))
    };
    let if_match = headers.typed_get::<IfMatch>();
    if let Some(if_match) = &if_match {
        if !exists
            || (!if_match.is_any()
                && !cache_headers.is_some_and(|(etag, _)| if_match.precondition_passes(etag))
                && !encoded.as_ref().is_some_and(|etag| if_match.precondition_passes(etag)))
        {
            return Precondition::Failed;
        }
    }

    // If-Match takes precedence over If-Unmodified-Since.
    if if_match.is_none() {
        if let (Some(if_unmodified_since), Some((_, last_modified))) =
            (headers.typed_get::<IfUnmodifiedSince>(), cache_headers)
        {
            if !if_unmodified_since.precondition_passes((*last_modified).into()) {
                return Precondition::Failed;
            }
        }
    }

    let if_none_match = headers.typed_get::<IfNoneMatch>();
    if let Some(if_none_match) = &if_none_match {
        if exists
            && (if_none_match == &IfNoneMatch::any()
                || cache_headers.is_some_and(|(etag, _)| !if_none_match.precondition_passes(etag))
                || encoded.as_ref().is_some_and(|etag| !if_none_match.precondition_passes(etag)))
        {
            return if allow_not_modified {
                Precondition::NotModified
            } else {
                Precondition::Failed
            };
        }
    }

    // If-None-Match takes precedence over If-Modified-Since.
    if if_none_match.is_none() && allow_not_modified {
        if let (Some(if_modified_since), Some((_, last_modified))) =
            (headers.typed_get::<IfModifiedSince>(), cache_headers)
        {
            if !if_modified_since.is_modified((*last_modified).into()) {
                return Precondition::NotModified;
            }
        }
    }

    Precondition::Passed
}

/// Check conditional headers, writing a `304` or `412` response when the
/// request must stop. Returns whether the request may proceed.
fn check_preconditions(
    headers: &HeaderMap<HeaderValue>,
    cache_headers: Option<&(ETag, LastModified)>,
    exists: bool,
    allow_not_modified: bool,
    res: &mut Response,
) -> bool {
    match evaluate_preconditions(headers, cache_headers, exists, allow_not_modified) {
        Precondition::Passed => true,
        Precondition::NotModified => {
            if let Some((etag, _)) = cache_headers {
                res.headers_mut().typed_insert(etag.clone());
                res.headers_mut()
                    .typed_insert(CacheControl::new().with_no_cache());
            }
            *res.status_mut() = StatusCode::NOT_MODIFIED;
            false
        }
        Precondition::Failed => {
            DufsErrorCode::PreconditionFailed.respond(res);
            false
        }
    }
}

fn status_not_found(res: &mut Response) {
    *res.status_mut() = StatusCode::NOT_FOUND;
    *res.body_mut() = body_full("Not Found");
}

fn status_no_content(res: &mut Response) {
    *res.status_mut() = StatusCode::NO_CONTENT;
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

fn is_upload_temp(name: &str) -> bool {
    if cfg!(windows) {
        name.to_ascii_lowercase().starts_with(UPLOAD_TEMP_PREFIX)
    } else {
        name.starts_with(UPLOAD_TEMP_PREFIX)
    }
}

fn is_hidden(hidden: &[String], file_name: &str, is_dir: bool) -> bool {
    is_upload_temp(file_name)
        || hidden.iter().any(|v| {
            if is_dir {
                if let Some(x) = v.strip_suffix('/') {
                    return glob(x, file_name);
                }
            }
            glob(v, file_name)
        })
}

fn set_webdav_headers(
    res: &mut Response,
    upload: bool,
    delete: bool,
    file: bool,
    dir: bool,
    missing: bool,
    search: bool,
) {
    let mut methods = vec!["GET", "HEAD", "OPTIONS", "CHECKAUTH", "LOGOUT"];
    if file || dir {
        methods.extend(["PROPFIND", "PROPPATCH"]);
    }
    if upload && !dir {
        methods.push("PUT");
    }
    if upload && file {
        methods.push("PATCH");
        res.headers_mut().insert(
            "Accept-Patch",
            HeaderValue::from_static("application/vnd.dufs.offset-patch"),
        );
    }
    if upload && missing {
        methods.push("MKCOL");
    }
    if dir && search {
        methods.push("SEARCH");
        res.headers_mut()
            .insert("DASL", HeaderValue::from_static("<DAV:basicsearch>"));
    }
    if dir {
        methods.push("REPORT");
    }
    if upload && (file || dir) {
        methods.push("COPY");
    }
    if delete && (file || dir) {
        methods.push("DELETE");
    }
    if upload && delete && (file || dir) {
        methods.push("MOVE");
    }
    res.headers_mut()
        .insert("Allow", HeaderValue::from_str(&methods.join(", ")).unwrap());
    if file || dir || (missing && upload) {
        res.headers_mut().insert(
            "DAV",
            HeaderValue::from_static(if upload && (dir || missing) {
                "1, 3, extended-mkcol"
            } else {
                "1, 3"
            }),
        );
    }
}

async fn get_content_type(path: &Path, file: &mut fs::File) -> Result<String> {
    let mut buffer: Vec<u8> = vec![];
    file.take(1024).read_to_end(&mut buffer).await?;
    file.seek(SeekFrom::Start(0)).await?;
    if path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("zst")) {
        return Ok("application/zstd".into());
    }
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
    let ranges = parse_range(value, size, false).ok_or_else(err)?;
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

async fn collect_dir_entries<F>(
    access_paths: AccessPaths,
    running: Arc<AtomicBool>,
    path: PathBuf,
    hidden: Arc<Vec<String>>,
    follow_symlinks: bool,
    serve_path: PathBuf,
    auth: AccessControl,
    user: Option<String>,
    include_entry: F,
) -> Vec<PathBuf>
where
    F: Fn(&DirEntry) -> bool,
{
    let mut paths: Vec<PathBuf> = vec![];
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

            if !follow_symlinks
                && !fs::canonicalize(entry_path)
                    .await
                    .ok()
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

            // Check every entry, not only links: a junction or another
            // filesystem alias may not report as a symlink, and a link below a
            // symlinked directory changes the target of paths that are not
            // links themselves. Without this, an archive or search could read
            // through the alias and bypass path rules.
            if auth.has_rules() {
                let rel_path = entry_path
                    .strip_prefix(&serve_path)
                    .ok()
                    .map(normalize_path)
                    .unwrap_or_default();
                if !auth
                    .can_access(user.as_deref(), &rel_path, &Method::GET, &serve_path)
                    .await
                {
                    if is_dir {
                        it.skip_current_dir();
                    }
                    continue;
                }
            }

            if !include_entry(&entry) {
                continue;
            }
            paths.push(entry_path.to_path_buf());
        }
    }
    paths
}
