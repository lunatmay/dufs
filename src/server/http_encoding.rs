use super::*;

fn quality(value: &str) -> Option<u16> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if fraction.len() > 3 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n = fraction.parse::<u16>().unwrap_or(0) * 10u16.pow(3 - fraction.len() as u32);
    match whole {
        "0" => Some(n),
        "1" if n == 0 => Some(1000),
        _ => None,
    }
}

// None means neither supported representation is acceptable.
pub(super) fn select(headers: &HeaderMap, range: bool) -> Option<bool> {
    let (mut zstd, mut identity, mut wildcard) = (None, None, None);
    for field in headers.get_all("Accept-Encoding") {
        let Ok(field) = field.to_str() else { continue };
        for item in field.split(',') {
            let (coding, weight) = match item.split_once(';') {
                Some((coding, parameter)) => {
                    let Some((key, value)) = parameter.trim().split_once('=') else {
                        continue;
                    };
                    if !key.eq_ignore_ascii_case("q") {
                        continue;
                    }
                    let Some(weight) = quality(value.trim()) else {
                        continue;
                    };
                    (coding, weight)
                }
                None => (item, 1000),
            };
            match coding.trim().to_ascii_lowercase().as_str() {
                "zstd" => zstd = Some(weight),
                "identity" => identity = Some(weight),
                "*" => wildcard = Some(weight),
                _ => {}
            }
        }
    }
    let identity = identity.unwrap_or(if wildcard == Some(0) { 0 } else { 1000 });
    let zstd = zstd.or(wildcard).unwrap_or(0);
    // Preserve byte offsets and requested digests when identity is acceptable.
    if identity > 0
        && (range
            || headers.contains_key("want-content-digest")
            || headers.contains_key("want-repr-digest"))
    {
        return Some(false);
    }
    if zstd > 0 && zstd >= identity {
        Some(true)
    } else if identity > 0 {
        Some(false)
    } else {
        None
    }
}

// Distinguish the encoded representation from the original and from future
// codec versions, while keeping both validators tied to the same file revision.
pub(super) fn zstd_etag(etag: &ETag) -> Option<ETag> {
    if etag.is_weak() {
        return None;
    }
    let mut values = Vec::new();
    headers::Header::encode(etag, &mut values);
    let tag = values.first()?.to_str().ok()?.trim_matches('"');
    format!("\"zstd-{}-v1-{tag}\"", zstd_safe::version_number())
        .parse()
        .ok()
}
