use super::*;
use sha2::Sha512;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Algorithm {
    Sha256,
    Sha512,
}

impl Algorithm {
    fn key(self) -> &'static str {
        match self {
            Self::Sha256 => "sha-256",
            Self::Sha512 => "sha-512",
        }
    }

    fn field(self, bytes: &[u8]) -> String {
        format!("{}=:{}:", self.key(), STANDARD.encode(bytes))
    }
}

const ALGORITHMS: [Algorithm; 2] = [Algorithm::Sha256, Algorithm::Sha512];

fn members(value: &str) -> Option<Vec<&str>> {
    if value.trim().is_empty() {
        return Some(vec![]);
    }
    let mut result = Vec::new();
    let (mut quoted, mut escaped, mut start) = (false, false, 0);
    for (index, ch) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if quoted && ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == '"' {
            quoted = !quoted;
            continue;
        }
        if ch == ',' && !quoted {
            result.push(value[start..index].trim());
            start = index + 1;
        }
    }
    if quoted || escaped {
        return None;
    }
    result.push(value[start..].trim());
    (!result.iter().any(|item| item.is_empty())).then_some(result)
}

fn preferred(headers: &HeaderMap, name: &str) -> Option<Algorithm> {
    let mut weights = [0u8; 2];
    for value in headers.get_all(name) {
        for entry in members(value.to_str().ok()?)? {
            let (key, value) = entry.trim().split_once('=')?;
            let weight = value.split(';').next()?.parse::<u8>().ok()?;
            if weight > 10 {
                return None;
            }
            if let Some(index) = ALGORITHMS.iter().position(|a| a.key() == key) {
                weights[index] = weight;
            }
        }
    }
    ALGORITHMS
        .into_iter()
        .zip(weights)
        .filter(|(_, w)| *w > 0)
        .max_by_key(|(_, w)| *w)
        .map(|(a, _)| a)
}

async fn hash_extent(
    file: &mut fs::File,
    start: u64,
    length: u64,
    algorithm: Algorithm,
) -> Result<String> {
    let previous = file.stream_position().await?;
    file.seek(SeekFrom::Start(start)).await?;
    let mut buffer = vec![0; BUF_SIZE];
    let mut remaining = length;
    let mut sha256 = Sha256::new();
    let mut sha512 = Sha512::new();
    while remaining != 0 {
        let n = remaining.min(buffer.len() as u64) as usize;
        file.read_exact(&mut buffer[..n]).await?;
        match algorithm {
            Algorithm::Sha256 => sha256.update(&buffer[..n]),
            Algorithm::Sha512 => sha512.update(&buffer[..n]),
        }
        remaining -= n as u64;
    }
    file.seek(SeekFrom::Start(previous)).await?;
    Ok(match algorithm {
        Algorithm::Sha256 => algorithm.field(&sha256.finalize()),
        Algorithm::Sha512 => algorithm.field(&sha512.finalize()),
    })
}

pub(super) async fn response_file_digests(
    file: &mut fs::File,
    headers: &HeaderMap,
    res: &mut Response,
    size: u64,
    head_only: bool,
    extent: Option<(u64, u64)>,
) -> Result<()> {
    let repr = preferred(headers, "want-repr-digest");
    let content = preferred(headers, "want-content-digest");
    let repr_value = if let Some(algorithm) = repr {
        let value = hash_extent(file, 0, size, algorithm).await?;
        res.headers_mut().insert("repr-digest", value.parse()?);
        Some(value)
    } else {
        None
    };
    if let (Some(algorithm), Some((start, length))) =
        (content, if head_only { Some((0, 0)) } else { extent })
    {
        let value = if repr == content && start == 0 && length == size {
            repr_value.unwrap()
        } else {
            hash_extent(file, start, length, algorithm).await?
        };
        res.headers_mut().insert("content-digest", value.parse()?);
    }
    Ok(())
}

// RFC 9530 defines no digest parameters. Ignore extension parameters while
// respecting quoted delimiters when splitting the Structured Fields dictionary.
fn matches_digests(headers: &HeaderMap, sha256: &[u8], sha512: &[u8]) -> bool {
    for name in ["content-digest", "repr-digest"] {
        for value in headers.get_all(name) {
            let Ok(value) = value.to_str() else {
                return false;
            };
            let Some(entries) = members(value) else {
                return false;
            };
            for member in entries {
                let Some((key, value)) = member.trim().split_once('=') else {
                    return false;
                };
                if !key
                    .as_bytes()
                    .first()
                    .is_some_and(|b| b.is_ascii_lowercase() || *b == b'*')
                    || !key.bytes().all(|b| {
                        b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-.*".contains(&b)
                    })
                {
                    return false;
                }
                let Some(value) = value
                    .split(';')
                    .next()
                    .and_then(|v| v.strip_prefix(':'))
                    .and_then(|v| v.strip_suffix(':'))
                else {
                    return false;
                };
                let Ok(bytes) = STANDARD.decode(value) else {
                    return false;
                };
                match key {
                    "sha-256" if bytes != sha256 => return false,
                    "sha-512" if bytes != sha512 => return false,
                    _ => {}
                }
            }
        }
    }
    true
}

pub(super) async fn copy_upload(
    req: Request,
    file: &mut fs::File,
    res: &mut Response,
) -> Result<bool> {
    let (parts, mut body) = req.into_parts();
    // A fixed-length request without a digest cannot carry trailers. Avoid
    // hashing large ordinary uploads unless the client requested validation.
    let hash = parts.headers.contains_key("content-digest")
        || parts.headers.contains_key("repr-digest")
        || !parts.headers.contains_key(CONTENT_LENGTH);
    let mut sha256 = Sha256::new();
    let mut sha512 = Sha512::new();
    let mut trailers = HeaderMap::new();
    while let Some(frame) = body.frame().await {
        let frame = frame?;
        if let Some(data) = frame.data_ref() {
            if hash {
                sha256.update(data);
                sha512.update(data);
            }
            file.write_all(data).await?;
        }
        if let Some(fields) = frame.trailers_ref() {
            for (key, value) in fields {
                trailers.append(key, value.clone());
            }
        }
    }
    let sha256 = sha256.finalize();
    let sha512 = sha512.finalize();
    if !matches_digests(&parts.headers, &sha256, &sha512)
        || !matches_digests(&trailers, &sha256, &sha512)
    {
        *res.status_mut() = StatusCode::BAD_REQUEST;
        return Ok(false);
    }
    Ok(true)
}
