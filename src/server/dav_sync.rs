// RFC 6578 collection synchronization. Only immediate members are supported.
use super::*;
use std::collections::{BTreeSet, VecDeque};

// History is intentionally process-local. Restart and eviction reject old tokens
// with DAV:valid-sync-token rather than treating an unknown state as current.
const MAX_VIEWS: usize = 16;
const MAX_MEMBERS: usize = 4096;
const MAX_EVENTS: usize = 8192;
const MAX_TOKENS: usize = 128;

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
struct ViewKey {
    path: PathBuf,
    user: Option<String>,
    via_token: bool,
}

type MemberStamp = [u8; 32];
type Members = BTreeMap<String, MemberStamp>;

struct Scan {
    identity: String,
    directory_stamp: String,
    members: Members,
    items: BTreeMap<String, PathItem>,
}

struct View {
    identity: String,
    directory_stamp: String,
    access: AccessPaths,
    members: Members,
    revision: u64,
    // Include tombstones even for members created and removed between reports.
    changed: BTreeMap<String, u64>,
    tokens: VecDeque<(String, u64)>,
}

impl View {
    fn new(scan: &Scan, access: &AccessPaths) -> Self {
        Self {
            identity: scan.identity.clone(),
            directory_stamp: scan.directory_stamp.clone(),
            access: access.clone(),
            members: scan.members.clone(),
            revision: 0,
            changed: BTreeMap::new(),
            tokens: VecDeque::from([(new_token(), 0)]),
        }
    }

    fn observe(&mut self, scan: &Scan, access: &AccessPaths, tracked_mutation: bool) {
        // An unobserved external namespace change could have involved a
        // transient member that is absent from both snapshots. Invalidate
        // rather than silently claiming that all changes are known.
        if self.identity != scan.identity
            || &self.access != access
            || (!tracked_mutation && self.directory_stamp != scan.directory_stamp)
        {
            *self = Self::new(scan, access);
            return;
        }
        self.directory_stamp.clone_from(&scan.directory_stamp);
        let changes: BTreeSet<_> = self
            .members
            .keys()
            .chain(scan.members.keys())
            .filter(|key| self.members.get(*key) != scan.members.get(*key))
            .cloned()
            .collect();
        if changes.is_empty() {
            return;
        }
        self.revision += 1;
        for key in changes {
            self.changed.insert(key, self.revision);
        }
        self.members.clone_from(&scan.members);
        self.tokens.push_back((new_token(), self.revision));
        while self.tokens.len() > MAX_TOKENS {
            self.tokens.pop_front();
        }
        let oldest = self.tokens.front().unwrap().1;
        self.changed.retain(|_, revision| *revision > oldest);
        if self.changed.len() > MAX_EVENTS {
            *self = Self::new(scan, access);
        }
    }

    fn token(&self) -> &str {
        &self.tokens.back().unwrap().0
    }

    fn changes_since(&self, token: &str) -> Option<Vec<String>> {
        if token.is_empty() {
            return Some(self.members.keys().cloned().collect());
        }
        let base = self.tokens.iter().find(|(value, _)| value == token)?.1;
        Some(
            self.changed
                .iter()
                .filter(|(_, revision)| **revision > base)
                .map(|(key, _)| key.clone())
                .collect(),
        )
    }
}

fn new_token() -> String {
    format!("urn:uuid:{}", Uuid::new_v4())
}

#[derive(Default)]
struct History {
    // Insertion order bounds the number of collection/principal views.
    views: VecDeque<(ViewKey, View)>,
}

impl History {
    fn observe(
        &mut self,
        key: ViewKey,
        scan: &Scan,
        access: &AccessPaths,
        tracked_mutation: bool,
    ) -> &View {
        let mut view = self
            .views
            .iter()
            .position(|(candidate, _)| candidate == &key)
            .and_then(|index| self.views.remove(index))
            .map(|(_, view)| view)
            .unwrap_or_else(|| View::new(scan, access));
        view.observe(scan, access, tracked_mutation);
        self.views.push_back((key, view));
        while self.views.len() > MAX_VIEWS {
            self.views.pop_front();
        }
        &self.views.back().unwrap().1
    }
}

#[derive(Default)]
pub(super) struct SyncHistory {
    history: tokio::sync::Mutex<History>,
}

#[derive(Debug)]
struct SyncTooLarge;
impl std::fmt::Display for SyncTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Collection exceeds the synchronization history limit")
    }
}
impl std::error::Error for SyncTooLarge {}

fn directory_identity(meta: &Metadata) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        format!("{}:{}", meta.dev(), meta.ino())
    }
    #[cfg(not(unix))]
    {
        format!("{:?}", meta.created().ok())
    }
}

fn directory_stamp(meta: &Metadata) -> Result<String> {
    etag_from_metadata(meta)
        .ok_or_else(|| anyhow!("Collection metadata does not provide a validator"))
}

impl Server {
    // Caller holds the collection's path lock during REPORT/property/If
    // evaluation. The mutation hook observes tracked views before releasing
    // the affected write lock.
    async fn scan_sync_collection(
        &self,
        path: &Path,
        user: Option<&str>,
        access: &AccessPaths,
    ) -> Result<Scan> {
        for _ in 0..3 {
            let meta = fs::metadata(path).await?;
            if !meta.is_dir() || self.guard_root_contained(path).await {
                return Err(anyhow!("Not an accessible collection"));
            }
            let identity = directory_identity(&meta);
            let start = directory_stamp(&meta)?;
            let mut members = Members::new();
            let mut items = BTreeMap::new();
            let mut entries = fs::read_dir(path).await?;
            let prefix = self.relative_parts(path);
            let mut scanned = 0;
            while let Some(entry) = entries.next_entry().await? {
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue };
                if is_upload_temp(name) {
                    continue;
                }
                scanned += 1;
                if scanned > MAX_MEMBERS {
                    return Err(SyncTooLarge.into());
                }
                let entry_path = entry.path();
                let Some(entry_access) = access.guard(name, &Method::GET, Some(path)).await else {
                    continue;
                };
                if !self.path_can_access(&entry_path, &Method::GET, user).await
                    || self.guard_root_contained(&entry_path).await
                {
                    continue;
                }
                let metadata = match fs::metadata(&entry_path).await {
                    Ok(meta) => meta,
                    // Dangling links have no mapped resource.
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                if !metadata.is_file() && !metadata.is_dir() {
                    continue;
                }
                if is_hidden(&self.args.hidden, name, metadata.is_dir()) {
                    continue;
                }
                let Some(mut item) = self.to_pathitem(&entry_path, &self.args.serve_path).await?
                else {
                    continue;
                };
                let Some(display_access) = self
                    .entry_access(access, &entry_path, name, &prefix, user)
                    .await
                else {
                    continue;
                };
                self.set_item_access(&mut item, display_access);
                let mut stamp = Sha256::new();
                stamp.update(directory_stamp(&metadata)?.as_bytes());
                stamp.update(serde_json::to_vec(&item)?);
                if metadata.is_dir()
                    && (self.args.render_index
                        || self.args.render_spa
                        || self.args.render_try_index)
                {
                    // A rendered collection can have an actual entity validator.
                    if let Some((etag, _)) = self
                        .dav_validators(
                            &entry_path,
                            Some(&metadata),
                            user.map(str::to_owned),
                            Some(entry_access),
                        )
                        .await?
                    {
                        stamp.update(format!("{etag:?}").as_bytes());
                    }
                }
                let mut key = name.to_string();
                if item.is_dir() {
                    key.push('/');
                }
                members.insert(key.clone(), stamp.finalize().into());
                items.insert(key, item);
            }
            let end = fs::metadata(path).await?;
            if directory_identity(&end) == identity && directory_stamp(&end)? == start {
                return Ok(Scan {
                    identity,
                    directory_stamp: start,
                    members,
                    items,
                });
            }
        }
        Err(anyhow!("Collection changed while being synchronized"))
    }

    pub(super) async fn sync_collection_token(
        &self,
        path: &Path,
        user: Option<String>,
        access: AccessPaths,
        via_token: bool,
    ) -> Result<String> {
        let mut history = self.sync_history.history.lock().await;
        let scan = self
            .scan_sync_collection(path, user.as_deref(), &access)
            .await?;
        let key = ViewKey {
            path: path.to_path_buf(),
            user,
            via_token,
        };
        Ok(history
            .observe(key, &scan, &access, false)
            .token()
            .to_owned())
    }

    pub(super) async fn sync_observe_tracked(&self, changed: &Path, destination: Option<&Path>) {
        let mut history = self.sync_history.history.lock().await;
        // Keep the cache order (recently used REPORTs first to survive eviction).
        let views = std::mem::take(&mut history.views);
        let changed_real = canonical_write_path(changed).await;
        let destination_real = match destination {
            Some(path) => Some(canonical_write_path(path).await),
            None => None,
        };
        for (key, mut view) in views {
            let view_real = canonical_write_path(&key.path).await;
            let related =
                |left: &Path, right: &Path| left.starts_with(right) || right.starts_with(left);
            if !related(&key.path, changed)
                && !related(&view_real, &changed_real)
                && !destination.is_some_and(|dest| related(&key.path, dest))
                && !destination_real
                    .as_ref()
                    .is_some_and(|dest| related(&view_real, dest))
            {
                history.views.push_back((key, view));
                continue;
            }
            let relative = self.relative_parts(&key.path).join("/");
            let Some(access) = self
                .args
                .auth
                .access_for(
                    key.user.as_deref(),
                    &relative,
                    &Method::GET,
                    &self.args.serve_path,
                )
                .await
            else {
                continue;
            };
            // A failed scan cannot prove that the old token is complete.
            if let Ok(scan) = self
                .scan_sync_collection(&key.path, key.user.as_deref(), &access)
                .await
            {
                view.observe(&scan, &access, true);
                history.views.push_back((key, view));
            }
        }
    }

    pub(super) async fn handle_sync_report(
        &self,
        path: &Path,
        headers: &HeaderMap<HeaderValue>,
        body: &[u8],
        user: Option<String>,
        access: AccessPaths,
        via_token: bool,
        res: &mut Response,
    ) -> Result<()> {
        if headers.get_all("Depth").iter().count() > 1
            || headers
                .get("Depth")
                .is_some_and(|value| value.as_bytes() != b"0")
        {
            *res.status_mut() = StatusCode::BAD_REQUEST;
            return Ok(());
        }
        let request = match SyncRequest::parse(body) {
            Ok(request) => request,
            Err(_) => {
                *res.status_mut() = StatusCode::BAD_REQUEST;
                return Ok(());
            }
        };
        if request.infinite {
            sync_error(res, StatusCode::FORBIDDEN, "sync-traversal-supported");
            return Ok(());
        }
        if !fs::metadata(path).await.is_ok_and(|meta| meta.is_dir()) {
            sync_error(res, StatusCode::FORBIDDEN, "supported-report");
            return Ok(());
        }
        let (scan, changes, token) = {
            let mut history = self.sync_history.history.lock().await;
            let scan = match self
                .scan_sync_collection(path, user.as_deref(), &access)
                .await
            {
                Ok(scan) => scan,
                Err(error) if error.is::<SyncTooLarge>() => {
                    sync_error(
                        res,
                        StatusCode::INSUFFICIENT_STORAGE,
                        "number-of-matches-within-limits",
                    );
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            let key = ViewKey {
                path: path.to_path_buf(),
                user: user.clone(),
                via_token,
            };
            let view = history.observe(key, &scan, &access, false);
            let Some(changes) = view.changes_since(&request.token) else {
                sync_error(res, StatusCode::FORBIDDEN, "valid-sync-token");
                return Ok(());
            };
            (scan, changes, view.token().to_owned())
        };
        // RFC 6578 §3.7 permits rejecting a requested result limit when a
        // consistent partial token cannot be generated. Do not emit an
        // incorrect token representing an incomplete result set.
        if request.limit.is_some_and(|limit| changes.len() > limit) {
            sync_error(
                res,
                StatusCode::INSUFFICIENT_STORAGE,
                "number-of-matches-within-limits",
            );
            return Ok(());
        }
        let selection = DavSelection::Props(request.properties);
        let mut output = String::new();
        for name in changes {
            let child = path.join(name.trim_end_matches('/'));
            if let Some(item) = scan.items.get(&name) {
                let child_access = access
                    .guard(name.trim_end_matches('/'), &Method::GET, Some(path))
                    .await;
                let xml = self
                    .dav_item_xml(
                        item,
                        &selection,
                        user.clone(),
                        child_access,
                        headers,
                        via_token,
                    )
                    .await?;
                output.push_str(&xml);
            } else {
                // Preserve the canonical collection trailing slash even after
                // the old mapping no longer exists on disk.
                let relative = child.strip_prefix(&self.args.serve_path)?;
                let mut href = encode_uri(&format!(
                    "{}{}",
                    self.args.uri_prefix,
                    normalize_path(relative)
                ));
                if name.ends_with('/') {
                    href.push('/');
                }
                output.push_str(&format!("<D:response><D:href>{}</D:href><D:status>HTTP/1.1 404 Not Found</D:status></D:response>", escape_str_pcdata(&href)));
            }
        }
        let end = fs::metadata(path).await?;
        if directory_identity(&end) != scan.identity
            || directory_stamp(&end)? != scan.directory_stamp
        {
            *res.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
            return Ok(());
        }
        {
            let history = self.sync_history.history.lock().await;
            let key = ViewKey {
                path: path.to_path_buf(),
                user,
                via_token,
            };
            if !history
                .views
                .iter()
                .any(|(candidate, view)| candidate == &key && view.token() == token)
            {
                // Asking for a sync-token on many child collections may fill
                // the bounded history while this response is being rendered.
                sync_error(
                    res,
                    StatusCode::INSUFFICIENT_STORAGE,
                    "number-of-matches-within-limits",
                );
                return Ok(());
            }
        }
        output.push_str(&format!(
            "<D:sync-token>{}</D:sync-token>",
            escape_str_pcdata(&token)
        ));
        res_multistatus(res, &output);
        Ok(())
    }
}

fn sync_error(res: &mut Response, status: StatusCode, name: &str) {
    *res.status_mut() = status;
    res.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/xml; charset=utf-8"),
    );
    *res.body_mut() = body_full(format!("<D:error xmlns:D=\"DAV:\"><D:{name}/></D:error>"));
}

struct SyncRequest {
    token: String,
    infinite: bool,
    limit: Option<usize>,
    properties: Vec<OwnedName>,
}

impl SyncRequest {
    fn parse(body: &[u8]) -> Result<Self> {
        let mut stack: Vec<Option<OwnedName>> = vec![];
        let mut token = None;
        let mut level = None;
        let mut limit = None;
        let mut properties = vec![];
        let mut has_prop = false;
        let mut has_limit = false;
        let mut text = String::new();
        for event in dav_xml_reader(body) {
            match event? {
                XmlEvent::StartElement { name, .. } => {
                    let known = if stack.last().is_some_and(Option::is_none) {
                        false
                    } else {
                        match stack.len() {
                            0 => {
                                if !dav_name(&name, "sync-collection") {
                                    return Err(anyhow!("Unsupported REPORT"));
                                }
                                true
                            }
                            1 if dav_name(&name, "sync-token") || dav_name(&name, "sync-level") => {
                                text.clear();
                                true
                            }
                            1 if dav_name(&name, "prop") => {
                                if has_prop {
                                    return Err(anyhow!("Duplicate prop"));
                                }
                                has_prop = true;
                                true
                            }
                            1 if dav_name(&name, "limit") => {
                                if has_limit {
                                    return Err(anyhow!("Duplicate limit"));
                                }
                                has_limit = true;
                                true
                            }
                            2 if stack[1]
                                .as_ref()
                                .is_some_and(|parent| dav_name(parent, "prop")) =>
                            {
                                properties.push(name.clone());
                                true
                            }
                            2 if stack[1]
                                .as_ref()
                                .is_some_and(|parent| dav_name(parent, "limit"))
                                && dav_name(&name, "nresults") =>
                            {
                                text.clear();
                                true
                            }
                            _ => false,
                        }
                    };
                    stack.push(known.then_some(name));
                }
                XmlEvent::EndElement { .. } => {
                    if let Some(Some(name)) = stack.pop() {
                        if dav_name(&name, "sync-token") && stack.len() == 1 {
                            if token.replace(text.trim().to_owned()).is_some() {
                                return Err(anyhow!("Duplicate sync-token"));
                            }
                        } else if dav_name(&name, "sync-level") && stack.len() == 1 {
                            if level.replace(text.trim().to_owned()).is_some() {
                                return Err(anyhow!("Duplicate sync-level"));
                            }
                        } else if dav_name(&name, "nresults")
                            && stack.len() == 2
                            && stack[1]
                                .as_ref()
                                .is_some_and(|parent| dav_name(parent, "limit"))
                        {
                            let count: usize = text.trim().parse()?;
                            if count == 0 || limit.replace(count).is_some() {
                                return Err(anyhow!("Invalid result limit"));
                            }
                        }
                    }
                }
                XmlEvent::Characters(value)
                | XmlEvent::CData(value)
                | XmlEvent::Whitespace(value) => {
                    if let Some(Some(name)) = stack.last() {
                        if (stack.len() == 2
                            && (dav_name(name, "sync-token") || dav_name(name, "sync-level")))
                            || (stack.len() == 3
                                && dav_name(name, "nresults")
                                && stack[1]
                                    .as_ref()
                                    .is_some_and(|parent| dav_name(parent, "limit")))
                        {
                            text.push_str(&value);
                        } else if !value.trim().is_empty() {
                            return Err(anyhow!("Invalid REPORT text"));
                        }
                    }
                }
                _ => {}
            }
        }
        if !has_prop || has_limit != limit.is_some() {
            return Err(anyhow!("Missing REPORT properties or limit"));
        }
        let infinite = match level.as_deref() {
            Some("1") => false,
            Some("infinite") => true,
            _ => return Err(anyhow!("Invalid sync-level")),
        };
        Ok(Self {
            token: token.ok_or_else(|| anyhow!("Missing sync-token"))?,
            infinite,
            limit,
            properties,
        })
    }
}
