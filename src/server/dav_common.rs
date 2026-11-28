use super::*;

// RFC 4331: report the volume's accounting domain, not the size of a subtree.
#[cfg(unix)]
fn quota(path: &Path) -> Option<(u64, u64)> {
    let stat = rustix::fs::statvfs(path).ok()?;
    Some((
        stat.f_bavail.saturating_mul(stat.f_frsize),
        stat.f_blocks
            .saturating_sub(stat.f_bfree)
            .saturating_mul(stat.f_frsize),
    ))
}

#[cfg(windows)]
fn quota(path: &Path) -> Option<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let (mut available, mut total, mut free) = (0, 0, 0);
    let ok = unsafe { GetDiskFreeSpaceExW(name.as_ptr(), &mut available, &mut total, &mut free) };
    (ok != 0).then(|| (available, total.saturating_sub(available)))
}

#[cfg(not(any(unix, windows)))]
fn quota(_path: &Path) -> Option<(u64, u64)> {
    None
}

pub(super) async fn quota_properties(path: &Path) -> Vec<(&'static str, String)> {
    let path = path.to_owned();
    match tokio::task::spawn_blocking(move || quota(&path))
        .await
        .ok()
        .flatten()
    {
        Some((available, used)) => vec![
            ("quota-available-bytes", available.to_string()),
            ("quota-used-bytes", used.to_string()),
        ],
        None => vec![],
    }
}

pub(super) fn storage_precondition(error: &anyhow::Error) -> Option<&'static str> {
    error.chain().find_map(|cause| {
        let error = cause.downcast_ref::<io::Error>()?;
        match error.kind() {
            io::ErrorKind::StorageFull => Some("sufficient-disk-space"),
            io::ErrorKind::QuotaExceeded => Some("quota-not-exceeded"),
            _ => None,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_error_preconditions() {
        assert_eq!(
            storage_precondition(&io::Error::from(io::ErrorKind::StorageFull).into()),
            Some("sufficient-disk-space")
        );
        assert_eq!(
            storage_precondition(&io::Error::from(io::ErrorKind::QuotaExceeded).into()),
            Some("quota-not-exceeded")
        );
        assert_eq!(
            storage_precondition(&io::Error::from(io::ErrorKind::PermissionDenied).into()),
            None
        );
    }
}

#[derive(Default, Clone, Copy)]
pub(super) struct DavPrefer {
    pub(super) minimal: bool,
    pub(super) noroot: bool,
    pub(super) representation: bool,
}

impl DavPrefer {
    pub(super) fn parse(headers: &HeaderMap<HeaderValue>) -> Self {
        let mut preference = Self::default();
        let mut returns = false;
        let mut noroot_seen = false;
        // Preference tokens can be split across fields, and extension values may be quoted.
        for value in headers
            .get_all("Prefer")
            .iter()
            .filter_map(|v| v.to_str().ok())
        {
            let mut start = 0;
            let mut quoted = false;
            let mut escaped = false;
            for (index, char) in value
                .char_indices()
                .chain(std::iter::once((value.len(), ',')))
            {
                if escaped {
                    escaped = false;
                    continue;
                }
                if char == '\\' && quoted {
                    escaped = true;
                    continue;
                }
                if char == '"' {
                    quoted = !quoted;
                }
                if char != ',' || quoted {
                    continue;
                }
                let item = value[start..index].trim();
                start = index + 1;
                let Some((key, value)) = parse_preference(item) else {
                    continue;
                };
                if key.eq_ignore_ascii_case("return") && !returns {
                    preference.minimal = value == "minimal";
                    preference.representation = value == "representation";
                    returns = true;
                } else if key.eq_ignore_ascii_case("depth-noroot") && !noroot_seen {
                    preference.noroot = value.is_empty();
                    noroot_seen = true;
                }
            }
        }
        preference
    }

    pub(super) fn applied(
        self,
        res: &mut Response,
        minimal: bool,
        noroot: bool,
        representation: bool,
    ) {
        let mut values = Vec::new();
        if self.minimal && minimal {
            values.push("return=minimal");
        }
        if self.noroot && noroot {
            values.push("depth-noroot");
        }
        if self.representation && representation {
            values.push("return=representation");
        }
        if !values.is_empty() {
            res.headers_mut().insert(
                "Preference-Applied",
                HeaderValue::from_str(&values.join(", ")).unwrap(),
            );
        }
    }
}

// Compare decoded quoted-string values case-sensitively (RFC 7240 §2).
fn parse_preference(item: &str) -> Option<(&str, String)> {
    let end = item.find(['=', ';']).unwrap_or(item.len());
    let key = item[..end].trim();
    let Some(value) = item[end..].strip_prefix('=') else {
        return Some((key, String::new()));
    };
    let value = value.trim_start();
    if let Some(quoted) = value.strip_prefix('"') {
        let mut decoded = String::new();
        let mut chars = quoted.char_indices();
        while let Some((index, ch)) = chars.next() {
            match ch {
                '\\' => decoded.push(chars.next()?.1),
                '"' => {
                    let tail = quoted[index + 1..].trim_start();
                    return (tail.is_empty() || tail.starts_with(';')).then_some((key, decoded));
                }
                _ => decoded.push(ch),
            }
        }
        None
    } else {
        Some((key, value.split(';').next()?.trim().to_owned()))
    }
}

// MKCOL and PROPPATCH use the same protected-property policy. We do not persist
// arbitrary dead properties, so only the standard resource type can be set here.
pub(super) fn extended_mkcol(body: &[u8], res: &mut Response) -> bool {
    let parsed = (|| -> Result<Vec<(OwnedName, bool)>> {
        let mut stack = Vec::<OwnedName>::new();
        let mut props = Vec::<(OwnedName, bool)>::new();
        let mut collection = false;
        let mut unsupported_type = false;
        for event in dav_xml_reader(body) {
            match event? {
                XmlEvent::StartElement { name, .. } => {
                    match stack.len() {
                        0 if !dav_name(&name, "mkcol") => {
                            return Err(anyhow!("Invalid extended MKCOL root"))
                        }
                        1 if !dav_name(&name, "set") => {
                            return Err(anyhow!("Invalid MKCOL action"))
                        }
                        2 if !dav_name(&name, "prop") => {
                            return Err(anyhow!("Invalid MKCOL property"))
                        }
                        3 => {
                            collection = false;
                            unsupported_type = false;
                        }
                        4 if dav_name(&stack[3], "resourcetype") => {
                            if dav_name(&name, "collection") && !collection {
                                collection = true;
                            } else {
                                unsupported_type = true;
                            }
                        }
                        n if n > 4 && dav_name(&stack[3], "resourcetype") => {
                            unsupported_type = true
                        }
                        _ => {}
                    }
                    stack.push(name);
                }
                XmlEvent::Characters(text) | XmlEvent::CData(text) if !text.trim().is_empty() => {
                    if stack.len() < 4 {
                        return Err(anyhow!("Invalid MKCOL content"));
                    }
                    if dav_name(&stack[3], "resourcetype") {
                        unsupported_type = true;
                    }
                }
                XmlEvent::EndElement { .. } => {
                    if stack.len() == 4 {
                        let name = stack.last().unwrap().clone();
                        let allowed =
                            dav_name(&name, "resourcetype") && collection && !unsupported_type;
                        props.push((name, allowed));
                    }
                    stack.pop();
                }
                _ => {}
            }
        }
        Ok(props)
    })();
    let props = match parsed {
        Ok(props) => props,
        Err(_) => {
            *res.status_mut() = StatusCode::BAD_REQUEST;
            return false;
        }
    };
    if props.iter().all(|(_, allowed)| *allowed) {
        return true;
    }
    let mut output = String::new();
    for (name, allowed) in props {
        let property = dav_empty_property(&name);
        let mut stat = dav_propstat(
            &property,
            if allowed {
                "424 Failed Dependency"
            } else {
                "403 Forbidden"
            },
        );
        if !allowed && dav_name(&name, "resourcetype") {
            stat = stat.replace(
                "</D:propstat>",
                "<D:error><D:valid-resourcetype/></D:error></D:propstat>",
            );
        }
        output.push_str(&stat);
    }
    *res.status_mut() = StatusCode::FORBIDDEN;
    res.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/xml; charset=utf-8"),
    );
    *res.body_mut() = body_full(format!(
        "<D:mkcol-response xmlns:D=\"DAV:\">{output}</D:mkcol-response>"
    ));
    false
}
