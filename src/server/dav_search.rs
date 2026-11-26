use super::*;
use std::cmp::Ordering;
use std::collections::HashSet;

const MAX_VISITED: usize = 10_000;
const MAX_RESULTS: usize = 1_000;
const MAX_OUTPUT: usize = 8 * 1024 * 1024;

#[derive(Debug)]
struct SearchError(StatusCode, Option<&'static str>);
type SearchResult<T> = std::result::Result<T, SearchError>;

fn malformed() -> SearchError {
    SearchError(StatusCode::BAD_REQUEST, None)
}
fn unsupported() -> SearchError {
    SearchError(StatusCode::UNPROCESSABLE_ENTITY, None)
}
fn invalid_scope() -> SearchError {
    SearchError(StatusCode::CONFLICT, Some("search-scope-valid"))
}
impl SearchError {
    fn respond(self, res: &mut Response) {
        *res.status_mut() = self.0;
        if let Some(condition) = self.1 {
            res.headers_mut().insert(
                CONTENT_TYPE,
                HeaderValue::from_static("application/xml; charset=utf-8"),
            );
            *res.body_mut() = body_full(format!(
                "<D:error xmlns:D=\"DAV:\"><D:{condition}/></D:error>"
            ));
        }
    }
}

struct Element {
    name: OwnedName,
    text: String,
    children: Vec<Element>,
}
impl Element {
    fn named(&self, name: &str) -> bool {
        dav_name(&self.name, name)
    }
    fn element(&self, name: &str) -> SearchResult<&Self> {
        self.optional(name)?.ok_or_else(malformed)
    }
    fn optional(&self, name: &str) -> SearchResult<Option<&Self>> {
        let mut matches = self.children.iter().filter(|c| c.named(name));
        let child = matches.next();
        if matches.next().is_some() {
            return Err(malformed());
        }
        Ok(child)
    }
    fn only(&self) -> SearchResult<&Self> {
        if self.children.len() != 1 || !self.text.trim().is_empty() {
            return Err(malformed());
        }
        Ok(&self.children[0])
    }
    fn leaf(&self) -> SearchResult<&str> {
        if !self.children.is_empty() {
            return Err(malformed());
        }
        Ok(&self.text)
    }
    fn container(&self) -> SearchResult<()> {
        if !self.text.trim().is_empty() {
            return Err(malformed());
        }
        Ok(())
    }
    fn empty(&self) -> SearchResult<()> {
        if !self.leaf()?.trim().is_empty() {
            return Err(malformed());
        }
        Ok(())
    }
}

fn parse_xml(body: &[u8], query: bool) -> SearchResult<Element> {
    let mut stack: Vec<Element> = Vec::new();
    let mut root = None;
    let mut nodes = 0;
    for event in dav_xml_reader(body) {
        match event.map_err(|_| malformed())? {
            XmlEvent::StartElement {
                name, attributes, ..
            } => {
                nodes += 1;
                if stack.len() >= 32 || (query && nodes > 2048) {
                    return Err(malformed());
                }
                if query {
                    for attr in attributes {
                        if attr.name.namespace.is_none() && attr.name.local_name == "caseless" {
                            match attr.value.as_str() {
                                "no" => {}
                                // Unicode default case folding is optional in RFC 5323.
                                "yes" => return Err(unsupported()),
                                _ => return Err(malformed()),
                            }
                        }
                    }
                }
                stack.push(Element {
                    name,
                    text: String::new(),
                    children: vec![],
                });
            }
            XmlEvent::EndElement { .. } => {
                let el = stack.pop().ok_or_else(malformed)?;
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(el);
                } else {
                    root = Some(el);
                }
            }
            XmlEvent::Characters(s) | XmlEvent::Whitespace(s) | XmlEvent::CData(s) => {
                if let Some(el) = stack.last_mut() {
                    el.text.push_str(&s);
                }
            }
            _ => {}
        }
    }
    root.ok_or_else(malformed)
}

#[derive(Clone, Copy)]
enum Op {
    Eq,
    Lt,
    Lte,
    Gt,
    Gte,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Property(Option<String>, String);
impl From<&OwnedName> for Property {
    fn from(n: &OwnedName) -> Self {
        Self(n.namespace.clone(), n.local_name.clone())
    }
}
fn property(el: &Element) -> SearchResult<OwnedName> {
    if !el.named("prop") {
        return Err(malformed());
    }
    let name = el.only()?;
    name.empty()?;
    Ok(name.name.clone())
}

// None means UNKNOWN, distinct from an empty or absent property.
#[derive(Clone)]
enum Expr {
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    Collection,
    Defined(Property),
    Compare(Op, Property, String),
    Like(Property, Vec<Wildcard>),
}
impl Expr {
    fn parse(el: &Element, props: &mut Vec<OwnedName>) -> SearchResult<Self> {
        if el.named("and") || el.named("or") {
            el.container()?;
            if el.children.is_empty() {
                return Err(malformed());
            }
            let children = el
                .children
                .iter()
                .map(|c| Self::parse(c, props))
                .collect::<SearchResult<_>>()?;
            return Ok(if el.named("and") {
                Self::And(children)
            } else {
                Self::Or(children)
            });
        }
        if el.named("not") {
            return Ok(Self::Not(Box::new(Self::parse(el.only()?, props)?)));
        }
        if el.named("is-collection") {
            el.empty()?;
            return Ok(Self::Collection);
        }
        if el.named("is-defined") {
            let p = property(el.only()?)?;
            props.push(p.clone());
            return Ok(Self::Defined(Property::from(&p)));
        }
        let op = if el.name.namespace.as_deref() == Some("DAV:") {
            match el.name.local_name.as_str() {
                "eq" => Some(Op::Eq),
                "lt" => Some(Op::Lt),
                "lte" => Some(Op::Lte),
                "gt" => Some(Op::Gt),
                "gte" => Some(Op::Gte),
                "like" => None,
                _ => return Err(unsupported()),
            }
        } else {
            return Err(unsupported());
        };
        el.container()?;
        if el.children.iter().any(|c| c.named("typed-literal")) {
            return Err(unsupported());
        }
        if el.children.len() != 2 {
            return Err(malformed());
        }
        let p = property(el.element("prop")?)?;
        let literal = el.element("literal")?.leaf()?.to_string();
        if literal.len() > 4096 {
            return Err(unsupported());
        }
        props.push(p.clone());
        let Some(op) = op else {
            if literal.len() > 512 {
                return Err(unsupported());
            }
            return Ok(Self::Like(Property::from(&p), like_pattern(&literal)?));
        };
        Ok(Self::Compare(op, Property::from(&p), literal))
    }
    fn eval(&self, values: &HashMap<Property, Option<String>>, is_dir: bool) -> Option<bool> {
        match self {
            Self::And(children) | Self::Or(children) => {
                let decisive = matches!(self, Self::Or(_));
                let mut unknown = false;
                for child in children {
                    match child.eval(values, is_dir) {
                        Some(v) if v == decisive => return Some(v),
                        None => unknown = true,
                        _ => {}
                    }
                }
                if unknown {
                    None
                } else {
                    Some(!decisive)
                }
            }
            Self::Not(child) => child.eval(values, is_dir).map(|v| !v),
            Self::Collection => Some(is_dir),
            Self::Defined(p) => Some(values.contains_key(p)),
            Self::Like(prop, pattern) => Some(like_matches(values.get(prop)?.as_deref()?, pattern)),
            Self::Compare(op, prop, literal) => {
                let value = values.get(prop)?.as_deref()?;
                let cmp = compare(prop, value, literal)?;
                Some(match op {
                    Op::Eq => cmp.is_eq(),
                    Op::Lt => cmp.is_lt(),
                    Op::Lte => !cmp.is_gt(),
                    Op::Gt => cmp.is_gt(),
                    Op::Gte => !cmp.is_lt(),
                })
            }
        }
    }
}

fn compare(p: &Property, a: &str, b: &str) -> Option<Ordering> {
    if p.0.as_deref() == Some("DAV:") {
        match p.1.as_str() {
            "getcontentlength" | "quota-used-bytes" | "quota-available-bytes" => {
                return Some(a.parse::<u64>().ok()?.cmp(&b.parse::<u64>().ok()?))
            }
            "creationdate" | "getlastmodified" => {
                let parse = |s: &str| {
                    chrono::DateTime::parse_from_rfc3339(s)
                        .or_else(|_| chrono::DateTime::parse_from_rfc2822(s))
                        .ok()
                };
                return Some(parse(a)?.cmp(&parse(b)?));
            }
            _ => {}
        }
    }
    Some(a.cmp(b))
}

#[derive(Clone, Copy)]
enum Wildcard {
    Many,
    One,
    Char(char),
}
fn like_pattern(s: &str) -> SearchResult<Vec<Wildcard>> {
    let mut chars = s.chars();
    let mut tokens = vec![];
    while let Some(c) = chars.next() {
        tokens.push(match c {
            '%' => Wildcard::Many,
            '_' => Wildcard::One,
            '\\' => match chars.next() {
                Some(c @ ('%' | '_' | '\\')) => Wildcard::Char(c),
                _ => return Err(malformed()),
            },
            c => Wildcard::Char(c),
        });
    }
    Ok(tokens)
}
fn like_matches(s: &str, pattern: &[Wildcard]) -> bool {
    // Dynamic programming bounds wildcard matching to O(pattern * text).
    let mut matched = vec![false; pattern.len() + 1];
    matched[0] = true;
    for (i, token) in pattern.iter().enumerate() {
        matched[i + 1] = matched[i] && matches!(token, Wildcard::Many);
    }
    for c in s.chars() {
        let mut previous = matched[0];
        matched[0] = false;
        for (i, token) in pattern.iter().enumerate() {
            let old = matched[i + 1];
            matched[i + 1] = match token {
                Wildcard::Many => old || matched[i],
                Wildcard::One => previous,
                Wildcard::Char(v) => previous && *v == c,
            };
            previous = old;
        }
    }
    matched[pattern.len()]
}

struct Search {
    select: DavSelection,
    scope: String,
    depth: usize,
    condition: Option<Expr>,
    properties: Vec<OwnedName>,
    order: Vec<(Property, bool)>,
    limit: usize,
}
impl Search {
    fn parse(body: &[u8]) -> SearchResult<Self> {
        let root = parse_xml(body, true)?;
        if root.named("query-schema-discovery") {
            return Err(SearchError(
                StatusCode::FORBIDDEN,
                Some("search-grammar-discovery-supported"),
            ));
        }
        if !root.named("searchrequest") {
            return Err(malformed());
        }
        let basic = root.only()?;
        if !basic.named("basicsearch") {
            return Err(SearchError(
                StatusCode::FORBIDDEN,
                Some("search-grammar-supported"),
            ));
        }
        basic.container()?;
        let select = basic.element("select")?.only()?;
        let select = if select.named("allprop") {
            select.empty()?;
            DavSelection::All(vec![])
        } else if select.named("prop") {
            select.container()?;
            let names = select
                .children
                .iter()
                .map(|el| {
                    el.empty()?;
                    Ok(el.name.clone())
                })
                .collect::<SearchResult<_>>()?;
            DavSelection::Props(names)
        } else {
            return Err(malformed());
        };
        let from = basic.element("from")?;
        from.container()?;
        if from.children.iter().filter(|c| c.named("scope")).count() > 1 {
            return Err(SearchError(
                StatusCode::FORBIDDEN,
                Some("search-multiple-scope-supported"),
            ));
        }
        let scope = from.element("scope")?;
        scope.container()?;
        if scope.optional("include-versions")?.is_some() {
            return Err(invalid_scope());
        }
        let href = scope.element("href")?.leaf()?.trim().to_string();
        let depth = match scope.element("depth")?.leaf()?.trim() {
            "0" => 0,
            "1" => 1,
            "infinity" => usize::MAX,
            _ => return Err(invalid_scope()),
        };
        let mut properties = vec![];
        let condition = basic
            .optional("where")?
            .map(|el| Expr::parse(el.only()?, &mut properties))
            .transpose()?;
        let mut order = vec![];
        if let Some(orderby) = basic.optional("orderby")? {
            orderby.container()?;
            if orderby.children.is_empty() {
                return Err(malformed());
            }
            for el in &orderby.children {
                if !el.named("order") {
                    return Err(unsupported());
                }
                el.container()?;
                if el.optional("score")?.is_some() {
                    return Err(unsupported());
                }
                let p = property(el.element("prop")?)?;
                let asc = el.optional("ascending")?;
                let desc = el.optional("descending")?;
                if asc.is_some() && desc.is_some() {
                    return Err(malformed());
                }
                if let Some(direction) = asc.or(desc) {
                    direction.empty()?;
                }
                properties.push(p.clone());
                order.push((Property::from(&p), desc.is_some()));
            }
        }
        let limit = if let Some(limit) = basic.optional("limit")? {
            limit.container()?;
            let value = limit.element("nresults")?.leaf()?.trim();
            if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(malformed());
            }
            value
                .parse::<usize>()
                .unwrap_or(usize::MAX)
                .min(MAX_RESULTS)
        } else {
            MAX_RESULTS
        };
        let mut seen = HashSet::new();
        properties.retain(|p| seen.insert(Property::from(p)));
        Ok(Self {
            select,
            scope: href,
            depth,
            condition,
            properties,
            order,
            limit,
        })
    }
}

fn response_properties(xml: &str) -> SearchResult<HashMap<Property, Option<String>>> {
    let root = parse_xml(
        format!("<D:multistatus xmlns:D=\"DAV:\">{xml}</D:multistatus>").as_bytes(),
        false,
    )?;
    let response = root.element("response")?;
    let mut result = HashMap::new();
    for stat in response.children.iter().filter(|c| c.named("propstat")) {
        let status = stat.element("status")?.leaf()?;
        if status
            .split_whitespace()
            .nth(1)
            .is_some_and(|v| v.starts_with('2'))
        {
            for p in &stat.element("prop")?.children {
                result.insert(
                    Property::from(&p.name),
                    p.children.is_empty().then(|| p.text.clone()),
                );
            }
        }
    }
    Ok(result)
}

// RFC 3986 local path references are resolved against the arbiter URI, not the filesystem path.
fn resolve_scope(scope: &str, req_path: &str, headers: &HeaderMap<HeaderValue>) -> Option<String> {
    if scope.is_empty() {
        return Some(req_path.to_owned());
    }
    if scope.contains(['?', '#']) || scope.starts_with("//") {
        return None;
    }
    let reference = if scope.starts_with('/') || scope.split('/').next()?.contains(':') {
        dav_uri_path(scope, headers)?
    } else {
        dav_uri_path(
            &format!("{}/{}", req_path.rsplit_once('/')?.0, scope),
            headers,
        )?
    };
    let mut segments = Vec::new();
    // Resolve only URI dot segments here; the decoded path is still validated by Server::resolve_path.
    for segment in reference.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            segment => segments.push(segment),
        }
    }
    Some(format!("/{}", segments.join("/")))
}

impl Server {
    pub(super) async fn handle_dav_search(
        &self,
        path: &Path,
        req_path: &str,
        headers: &HeaderMap<HeaderValue>,
        body: &[u8],
        user: Option<String>,
        access: AccessPaths,
        via_token: bool,
        res: &mut Response,
    ) -> Result<()> {
        let query = match Search::parse(body) {
            Ok(v) => v,
            Err(e) => {
                e.respond(res);
                return Ok(());
            }
        };
        let scope = resolve_scope(&query.scope, req_path, headers)
            .and_then(|p| self.resolve_path(&p))
            .and_then(|p| self.join_path(&p));
        let Some(scope) = scope else {
            invalid_scope().respond(res);
            return Ok(());
        };
        if via_token && (scope != path || query.depth != 0) {
            invalid_scope().respond(res);
            return Ok(());
        }
        let root = &self.args.serve_path;
        let mut eval_headers = headers.clone();
        eval_headers.remove("prefer");
        let eval_selection = DavSelection::Props(query.properties);
        let mut pending = vec![(scope.clone(), query.depth)];
        let mut visited = HashSet::new();
        let mut matched = vec![];
        let mut count = 0;
        let mut truncated = false;
        let mut bytes = 0;
        while let Some((candidate, depth)) = pending.pop() {
            count += 1;
            if count > MAX_VISITED {
                truncated = true;
                break;
            }
            let relative = candidate.strip_prefix(root)?;
            if relative.to_str().is_none() {
                continue;
            }
            let candidate_access = if via_token {
                Some(access.clone())
            } else {
                self.args
                    .auth
                    .access_for(
                        user.as_deref(),
                        &normalize_path(relative),
                        &Method::GET,
                        root,
                    )
                    .await
            };
            let canonical = fs::canonicalize(&candidate).await.ok();
            let Some((candidate_access, canonical)) = candidate_access.zip(canonical) else {
                if candidate == scope {
                    invalid_scope().respond(res);
                    return Ok(());
                }
                continue;
            };
            // SEARCH never leaves the served filesystem tree, and canonical identities bound symlink cycles.
            if !canonical.starts_with(root) || self.guard_root_contained(&candidate).await {
                if candidate == scope {
                    invalid_scope().respond(res);
                    return Ok(());
                }
                continue;
            }
            if !fs::metadata(&candidate)
                .await
                .is_ok_and(|m| m.is_dir() || m.is_file())
            {
                if candidate == scope {
                    invalid_scope().respond(res);
                    return Ok(());
                }
                continue;
            }
            if !visited.insert(canonical) {
                continue;
            }
            let item = match self.to_pathitem(&candidate, root).await {
                Ok(Some(item)) => item,
                _ if candidate == scope => {
                    invalid_scope().respond(res);
                    return Ok(());
                }
                _ => continue,
            };
            if candidate != scope
                && is_hidden(&self.args.hidden, get_file_name(&candidate), item.is_dir())
            {
                continue;
            }
            let values = if let DavSelection::Props(props) = &eval_selection {
                if props.is_empty() {
                    HashMap::new()
                } else {
                    let xml = self
                        .dav_item_xml(
                            &item,
                            &eval_selection,
                            user.clone(),
                            Some(candidate_access.clone()),
                            &eval_headers,
                            via_token,
                        )
                        .await?;
                    response_properties(&xml)
                        .map_err(|_| anyhow!("Invalid DAV property response"))?
                }
            } else {
                unreachable!()
            };
            if query
                .condition
                .as_ref()
                .is_none_or(|expr| expr.eval(&values, item.is_dir()) == Some(true))
            {
                let xml = self
                    .dav_item_xml(
                        &item,
                        &query.select,
                        user.clone(),
                        Some(candidate_access.clone()),
                        headers,
                        via_token,
                    )
                    .await?;
                bytes += xml.len()
                    + values
                        .iter()
                        .map(|(p, v)| {
                            p.0.as_ref().map_or(0, String::len)
                                + p.1.len()
                                + v.as_ref().map_or(0, String::len)
                        })
                        .sum::<usize>();
                if bytes > MAX_OUTPUT {
                    truncated = true;
                    break;
                }
                matched.push((xml, values));
                if query.order.is_empty() && matched.len() > query.limit {
                    truncated = true;
                    break;
                }
            }
            if item.is_dir() && depth != 0 {
                if candidate_access.perm().indexonly() {
                    for name in candidate_access.child_names() {
                        if pending.len() + count >= MAX_VISITED {
                            truncated = true;
                            break;
                        }
                        pending.push((candidate.join(name), depth - 1));
                    }
                } else {
                    match fs::read_dir(&candidate).await {
                        Ok(mut entries) => loop {
                            match entries.next_entry().await {
                                Ok(Some(entry)) if pending.len() + count < MAX_VISITED => {
                                    pending.push((entry.path(), depth - 1))
                                }
                                Ok(None) => break,
                                _ => {
                                    truncated = true;
                                    break;
                                }
                            }
                        },
                        Err(_) => truncated = true,
                    }
                }
            }
        }
        matched.sort_by(|(_, a), (_, b)| {
            for (p, descending) in &query.order {
                let a = a.get(p).and_then(|s| s.as_deref());
                let b = b.get(p).and_then(|s| s.as_deref());
                let c = match (a, b) {
                    (Some(a), Some(b)) => compare(p, a, b).unwrap_or_else(|| a.cmp(b)),
                    _ => a.cmp(&b),
                };
                if !c.is_eq() {
                    return if *descending { c.reverse() } else { c };
                }
            }
            Ordering::Equal
        });
        truncated |= matched.len() > query.limit;
        let mut result = matched
            .into_iter()
            .take(query.limit)
            .map(|(xml, _)| xml)
            .collect::<String>();
        if truncated {
            result.push_str(&self.dav_status(path, StatusCode::INSUFFICIENT_STORAGE));
        }
        res_multistatus(res, &result);
        res.headers_mut()
            .insert("cache-control", HeaderValue::from_static("no-store"));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparison_and_null_logic() {
        let root = parse_xml(br#"<and xmlns="DAV:"><gt><prop><getcontentlength/></prop><literal>9</literal></gt><not><eq><prop><unknown/></prop><literal>x</literal></eq></not></and>"#, true).map_err(|e| format!("{e:?}")).unwrap();
        let expr = Expr::parse(&root, &mut vec![]).unwrap();
        let mut values = HashMap::from([(
            Property(Some("DAV:".into()), "getcontentlength".into()),
            Some("10".into()),
        )]);
        assert_eq!(expr.eval(&values, false), None);
        values.insert(
            Property(Some("DAV:".into()), "unknown".into()),
            Some("y".into()),
        );
        assert_eq!(expr.eval(&values, false), Some(true));
        values.insert(
            Property(Some("DAV:".into()), "getcontentlength".into()),
            Some("2".into()),
        );
        assert_eq!(expr.eval(&values, false), Some(false));
    }

    #[test]
    fn like_matches_unicode_and_escape() {
        let pattern = like_pattern(r"_\_%.txt").unwrap();
        assert!(like_matches("中_foo.txt", &pattern));
        assert!(!like_matches("中文_foo.txt", &pattern));
        assert!(like_pattern(r"foo\x").is_err());
    }

    #[test]
    fn relative_reference_uses_arbiter_uri() {
        let headers = HeaderMap::new();
        assert_eq!(
            resolve_scope("a", "/dir/", &headers).as_deref(),
            Some("/dir/a")
        );
        assert_eq!(
            resolve_scope("../a", "/dir/", &headers).as_deref(),
            Some("/a")
        );
        assert_eq!(resolve_scope("a", "/dir", &headers).as_deref(), Some("/a"));
    }
}
