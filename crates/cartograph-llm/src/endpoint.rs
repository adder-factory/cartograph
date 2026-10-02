use url::Url;

const MAXIMUM_ENDPOINT_BYTES: usize = 4_096;

#[derive(Clone, Copy)]
pub(crate) enum EndpointPath {
    ChatCompletions,
    AnthropicMessages,
    Embeddings,
    Rerank,
    Models,
}

impl EndpointPath {
    const fn full(self) -> &'static str {
        match self {
            Self::ChatCompletions => "/v1/chat/completions",
            Self::AnthropicMessages => "/v1/messages",
            Self::Embeddings => "/v1/embeddings",
            Self::Rerank => "/v1/rerank",
            Self::Models => "/v1/models",
        }
    }

    const fn after_v1(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat/completions",
            Self::AnthropicMessages => "messages",
            Self::Embeddings => "embeddings",
            Self::Rerank => "rerank",
            Self::Models => "models",
        }
    }
}

pub(crate) fn normalize_endpoint(raw: &str, target: EndpointPath) -> Result<Url, ()> {
    let mut endpoint = parse_endpoint(raw)?;
    let path = endpoint.path().trim_end_matches('/');
    endpoint.set_path(&normalized_endpoint_path(path, target));
    Ok(endpoint)
}

pub(crate) fn validate_endpoint(raw: &str) -> Result<(), ()> {
    parse_endpoint(raw).map(|_| ())
}

fn parse_endpoint(raw: &str) -> Result<Url, ()> {
    if raw.is_empty() || raw.len() > MAXIMUM_ENDPOINT_BYTES || raw.chars().any(char::is_control) {
        return Err(());
    }
    let endpoint = Url::parse(raw).map_err(|_| ())?;
    if endpoint.username() != ""
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
        || endpoint.host_str().is_none()
    {
        return Err(());
    }
    let permitted_scheme = endpoint.scheme() == "https"
        || (endpoint.scheme() == "http" && endpoint.host_str().is_some_and(is_loopback_host));
    permitted_scheme.then_some(endpoint).ok_or(())
}

fn normalized_endpoint_path(path: &str, target: EndpointPath) -> String {
    if path.ends_with(target.full()) {
        path.to_owned()
    } else {
        let base = [
            EndpointPath::ChatCompletions,
            EndpointPath::AnthropicMessages,
            EndpointPath::Embeddings,
            EndpointPath::Rerank,
            EndpointPath::Models,
        ]
        .iter()
        .find_map(|known| path.strip_suffix(known.full()))
        .or_else(|| path.strip_suffix("/v1"))
        .unwrap_or(path);
        if base.is_empty() {
            target.full().to_owned()
        } else if path.ends_with("/v1") {
            format!("{base}/v1/{}", target.after_v1())
        } else {
            format!("{base}{}", target.full())
        }
    }
}

fn is_loopback_host(host: &str) -> bool {
    let host = host.strip_circumfix('[', ']').unwrap_or(host);
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}
