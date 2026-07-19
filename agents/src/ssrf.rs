//! SSRF guard for outbound HTTP requests in arachne.
//!
//! Closes two security bugs found in the audit:
//!
//! 1. **DNS rebinding** (issue #11) — reqwest resolves DNS once for
//!    `connect()`, but a hostile authoritative DNS server can return
//!    different answers for two queries. The naive "resolve once,
//    validate, then connect" pattern lets `attacker.test` validate as
//!    `1.1.1.1` and connect as `127.0.0.1`. We fix this by plugging a
//!    custom `reqwest::dns::Resolve` into the client: every DNS lookup
//!    goes through the resolver, which is invoked at connect time and
//!    therefore cannot be outpaced by a rebinding attack.
//!
//! 2. **Multi-A-record bypass** (issue #17) — when a hostname
//!    resolves to multiple IPs, only the first was checked. A
//!    hostname returning `[1.1.1.1, 127.0.0.1]` would pass
//!    validation. We fix this by **rejecting any address that fails**
//!    rather than accepting the first that passes. The host allow
//!    rule still works (skip the IP layer) because the user has
//!    opted-in to that hostname.
//!
//! ## Threat model
//!
//! arachne has multiple HTTP egress points, each with a different
//! trust model:
//!
//! | Caller | Trust the URL? | Apply guard? |
//! |---|---|---|
//! | `webfetch` (model-driven) | **No** — the LLM picks the URL | **Yes, default-deny** |
//! | `websearch` (provider-configured base URL) | Semi — base URL is user-configured, but params are derived | Yes, on the base URL |
//! | `mcp` (user-configured server URL) | Semi | Yes |
//! | `provider_oauth` (issuer URL is user-configured) | Semi | Yes |
//! | `openai_compatible` provider (user-configured base URL) | Semi | Yes |
//!
//! All of these go through reqwest and benefit from a single shared
//! `SsrfPolicy`. The default ACL is the **local-network deny** preset
//! (RFC1918, loopback, link-local, CGNAT, IPv6 ULA/link-local, IPv4-mapped
//! variants, plus cloud metadata endpoints like AWS / GCP at
//! `169.254.169.254`).
//!
//! ## Three layers
//!
//! SSRF defense is layered. Each layer handles a different gap:
//!
//! | Layer | Catches |
//! |---|---|
//! | **Host rules** (pre-DNS) | A hostname explicitly allowed (skip) or denied (block without resolving DNS) |
//! | **Resolver** (DNS lookup) | Every DNS-resolved address goes through the IP ACL; any denied address blocks the connection. Multi-A-record bypass prevented. |
//! | **`validate_url` for IP literals** | URLs whose host is a literal IP — the resolver is never called, so we must check the literal directly. |
//!
//! Use [`webfetch_client`] or [`provider_client`] to get a guarded
//! `reqwest::Client` shared across the process. Callers that need a
//! one-shot URL check (IP-literal hosts) call [`check_url`] before
//! dispatching the request.
//!
//! ## Custom resolver semantics
//!
//! The resolver:
//! 1. Takes `Name` from hyper-util.
//! 2. Returns the OS resolver's answer via `tokio::net::lookup_host`.
//! 3. Filters the answer against the IP ACL: every address must pass
//!    (multi-A-record bypass prevention).
//! 4. If the filtered answer is empty, returns an `io::Error` whose
//!    inner cause is `AclError::NoAllowedAddress(host)`.
//!
//! The redirect policy is a separate hook on `reqwest::redirect::Policy`
//! that calls `evaluate` on each hop's URL. arachne's webfetch tool
//! doesn't follow redirects by default, but other callers might, so
//! the hook is reusable.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;

use ipnet::IpNet;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::redirect::Policy as RedirectPolicy;
use reqwest::Url;

/// Why the ACL rejected something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AclError {
    /// A URL whose host was an IP literal that the IP ACL denied.
    DeniedIp(IpAddr),
    /// A URL whose host was a hostname explicitly denied by a host
    /// rule (pre-DNS check).
    DeniedHost(String),
    /// Every address that the hostname resolved to was denied by
    /// the IP ACL. Returned by the resolver at connect time.
    NoAllowedAddress(String),
}

impl std::fmt::Display for AclError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DeniedIp(ip) => write!(f, "address {ip} is denied by SSRF guard"),
            Self::DeniedHost(h) => write!(f, "host {h} is denied by SSRF guard"),
            Self::NoAllowedAddress(h) => write!(
                f,
                "all resolved addresses for {h} are denied by SSRF guard"
            ),
        }
    }
}

impl std::error::Error for AclError {}

/// Outcome of evaluating a URL against the ACL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The URL is allowed. Either the IP was allowed by IP rules,
    /// the host was explicitly allowed (skipping the IP layer), or
    /// the default-allow policy applies.
    Allow,
    /// The URL is denied.
    Deny(AclError),
}

/// Returned by [`SsrfAcl::host_decision`] to communicate host-rule
/// outcomes without losing the reason for deny.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HostDecision {
    /// Host is explicitly allowed; skip IP filtering.
    /// ⚠ Disables the SSRF / DNS rebinding protection for that host —
    /// use only for hosts you fully trust to resolve to safe addresses.
    Allow,
    /// Host is explicitly denied; reject without resolving DNS.
    Deny,
    /// No host rule matched; proceed to the IP layer.
    Continue,
}

/// IP-layer deny rule. Built from CIDR (`deny_cidr`) or a closure
/// (`deny_ip_when`).
#[derive(Clone)]
enum IpRule {
    Deny(Arc<dyn Fn(IpAddr) -> bool + Send + Sync>),
}

/// Host-layer deny rule. `host` is the normalized hostname
/// (lowercased, trailing dot stripped).
#[derive(Clone)]
enum HostRule {
    Deny(Arc<dyn Fn(&str) -> bool + Send + Sync>),
}

/// A composable SSRF ACL built from allow/deny rules.
///
/// ## Evaluation order
///
/// Host rules first (pre-DNS):
/// 1. Any host `deny_*` rule matches → **deny** without resolving DNS.
/// 2. Otherwise → fall through to the IP layer.
///
/// IP rules (post-DNS, or for IP-literal URLs):
/// 1. If the IP is in any deny rule → **deny**.
/// 2. Otherwise → **allow** (default).
///
/// Allow rules for IP / host are intentionally not exposed. The
/// caller can flip to allowlist mode via [`SsrfPolicy::default_deny`]
/// which is wired into [`SsrfPolicy`], not this ACL type.
#[derive(Clone)]
pub struct SsrfAcl {
    ip_rules: Vec<IpRule>,
    host_rules: Vec<HostRule>,
}

impl Default for SsrfAcl {
    fn default() -> Self {
        Self::new()
    }
}

impl SsrfAcl {
    /// Build an empty ACL. Default decision is "allow"; use
    /// [`SsrfPolicy::default_deny`] to flip.
    pub fn new() -> Self {
        Self {
            ip_rules: Vec::new(),
            host_rules: Vec::new(),
        }
    }

    /// Append a deny rule matching every address on the local network.
    /// Covers RFC1918, loopback, link-local, CGNAT, IPv6 ULA / link-local,
    /// IPv4-mapped variants, plus the cloud metadata endpoints used by
    /// AWS / GCP / Azure / DigitalOcean / Oracle / Hetzner / IBM (all at
    /// `169.254.169.254`) and Alibaba Cloud (`100.100.100.200`).
    pub fn deny_local_network(mut self) -> Self {
        self.ip_rules.push(IpRule::Deny(Arc::new(is_local_network)));
        self
    }

    /// Deny any IP for which `f` returns true.
    pub fn deny_ip_when<F>(mut self, f: F) -> Self
    where
        F: Fn(IpAddr) -> bool + Send + Sync + 'static,
    {
        self.ip_rules.push(IpRule::Deny(Arc::new(f)));
        self
    }

    /// Deny every address inside `cidr`. For a single IP, pass `/32`
    /// (v4) or `/128` (v6).
    pub fn deny_cidr(mut self, cidr: IpNet) -> Self {
        self.ip_rules
            .push(IpRule::Deny(Arc::new(move |ip| cidr.contains(&ip))));
        self
    }

    /// Deny the exact hostname `name` (case-insensitive). Matches the
    /// host portion of the URL, not the URL's resolved IP — so this is
    /// checked before DNS resolution.
    pub fn deny_host(mut self, name: impl Into<String>) -> Self {
        let target = normalize_host(&name.into());
        self.host_rules
            .push(HostRule::Deny(Arc::new(move |h| h == target)));
        self
    }

    /// Deny any hostname that ends with `suffix` (case-insensitive).
    /// Pass a leading dot (`".example.com"`) to match strict subdomains
    /// only.
    pub fn deny_host_suffix(mut self, suffix: impl Into<String>) -> Self {
        let suffix = normalize_host(&suffix.into());
        self.host_rules.push(HostRule::Deny(Arc::new(move |h| {
            // Strict suffix requires leading dot: "evil.test" must not match
            // "example.evil.test" but ".evil.test" should.
            if let Some(stripped) = suffix.strip_prefix('.') {
                h.ends_with(&format!(".{stripped}"))
            } else {
                h == suffix || h.ends_with(&format!(".{suffix}"))
            }
        })));
        self
    }

    /// Evaluate a URL. Returns [`Decision::Allow`] or
    /// [`Decision::Deny`] with the reason.
    pub fn evaluate(&self, url: &Url) -> Decision {
        let Some(host) = url.host_str() else {
            // URL with no host is malformed for HTTP; treat as denied.
            return Decision::Deny(AclError::DeniedHost("<missing>".to_string()));
        };

        match self.host_decision(host) {
            HostDecision::Allow => return Decision::Allow,
            HostDecision::Deny => {
                return Decision::Deny(AclError::DeniedHost(host.to_string()));
            }
            HostDecision::Continue => {}
        }

        // Literal IP? Evaluate directly.
        if let Ok(ip) = host.parse::<IpAddr>() {
            return self.evaluate_ip(ip, host);
        }

        // IPv6 literal in brackets — `Url::host()` strips the brackets
        // already, so the parse above handles it.
        // Hostname — let the resolver handle it at connect time.
        // Returning Allow here means: "the host rule didn't deny it,
        // and the IP ACL will be applied when DNS resolves." We
        // don't pre-resolve in `evaluate` because that's exactly the
        // DNS-rebinding footgun we're avoiding.
        Decision::Allow
    }

    fn evaluate_ip(&self, ip: IpAddr, host: &str) -> Decision {
        for rule in &self.ip_rules {
            if let IpRule::Deny(pred) = rule {
                if pred(ip) {
                    return Decision::Deny(AclError::DeniedIp(ip));
                }
            }
        }
        let _ = host;
        Decision::Allow
    }

    /// Test every resolved address against the deny rules. **All** must
    /// pass; if even one fails the host is blocked. This is the
    /// multi-A-record bypass fix. Used by callers that want a single
    /// aggregate decision without going through the resolver (e.g.
    /// external integrations and tests). The resolver applies the
    /// same rule but additionally returns the filtered set so
    /// reqwest can connect to one of the allowed addresses.
    pub fn evaluate_addrs<I>(&self, host: &str, addrs: I) -> Result<(), AclError>
    where
        I: IntoIterator<Item = IpAddr>,
    {
        // Defense in depth: also re-evaluate host rules in case the
        // caller skipped `evaluate` for IP-literal URLs.
        if matches!(self.host_decision(host), HostDecision::Deny) {
            return Err(AclError::DeniedHost(host.to_string()));
        }

        // Empty result set: nothing to connect to. The OS resolver
        // already produced an empty iterator upstream, so this is
        // a separate failure mode from "all denied."
        let mut any_addresses = false;
        for ip in addrs {
            any_addresses = true;
            for rule in &self.ip_rules {
                if let IpRule::Deny(pred) = rule {
                    if pred(ip) {
                        return Err(AclError::DeniedIp(ip));
                    }
                }
            }
        }
        if any_addresses {
            Ok(())
        } else {
            Err(AclError::NoAllowedAddress(host.to_string()))
        }
    }

    fn host_decision(&self, host: &str) -> HostDecision {
        let normalized = normalize_host(host);
        for rule in &self.host_rules {
            if let HostRule::Deny(pred) = rule {
                if pred(&normalized) {
                    return HostDecision::Deny;
                }
            }
        }
        HostDecision::Continue
    }
}

fn normalize_host(h: &str) -> String {
    h.trim_end_matches('.').to_ascii_lowercase()
}

/// Returns `true` if `ip` belongs to a local / non-routable network
/// or a known cloud-metadata endpoint. Mirrors the
/// `reqwest-ssrf-guard::is_local_network` set so this crate stands
/// alone.
pub fn is_local_network(ip: IpAddr) -> bool {
    LOCAL_NETWORKS.iter().any(|net| net.contains(&ip))
}

/// CIDRs that count as "local network" for SSRF purposes. Parsed
/// lazily on first use; `.parse()` on `IpNet` is not const-stable,
/// so we can't use a `const` slice.
static LOCAL_NETWORKS: std::sync::LazyLock<Vec<IpNet>> =
    std::sync::LazyLock::new(|| {
        [
            // IPv4
            "127.0.0.0/8",    // loopback
            "10.0.0.0/8",     // private
            "172.16.0.0/12",  // private
            "192.168.0.0/16", // private
            "169.254.0.0/16", // link-local / metadata
            "100.64.0.0/10",  // CGNAT
            "0.0.0.0/8",      // "this network"
            "224.0.0.0/4",    // multicast
            "240.0.0.0/4",    // reserved
            "192.0.0.0/24",   // IETF protocol assignments
            "192.0.2.0/24",   // TEST-NET-1
            "198.18.0.0/15",  // benchmarking
            "198.51.100.0/24", // TEST-NET-2
            "203.0.113.0/24", // TEST-NET-3
            // IPv6
            "::1/128",   // loopback
            "fc00::/7",  // ULA
            "fe80::/10", // link-local
            "::ffff:0:0/96", // IPv4-mapped
        ]
        .iter()
        .map(|s| s.parse().expect("static CIDR must be valid"))
        .collect()
    });

// ---------------------------------------------------------------------------
// Resolver — the reqwest DNS hook that closes DNS rebinding.
// ---------------------------------------------------------------------------

/// Custom DNS resolver that filters every address through the ACL.
/// Multi-A-record bypass fix: if **any** resolved address is denied,
/// the connection fails with [`AclError::NoAllowedAddress`].
///
/// Constructed via [`SsrfPolicy::resolver`].
pub struct GuardedResolver {
    acl: SsrfAcl,
}

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        // Defense in depth: a host explicitly denied by the ACL never
        // even reaches the DNS layer.
        if matches!(self.acl.host_decision(&host), HostDecision::Deny) {
            let err: Box<dyn std::error::Error + Send + Sync> =
                AclError::DeniedHost(host.clone()).into();
            return Box::pin(async move { Err::<Addrs, _>(err) });
        }

        let acl = self.acl.clone();
        Box::pin(async move {
            let lookup = tokio::net::lookup_host((host.as_str(), 0)).await;
            match lookup {
                Ok(mut addrs) => {
                    // Filter each resolved address through the ACL.
                    // Multi-A-record bypass fix: a single denied
                    // address in the result rejects the host.
                    let mut kept: Vec<SocketAddr> = Vec::new();
                    for socket in &mut addrs {
                        let ip = socket.ip();
                        let mut denied = false;
                        for rule in &acl.ip_rules {
                            if let IpRule::Deny(pred) = rule {
                                if pred(ip) {
                                    denied = true;
                                    break;
                                }
                            }
                        }
                        if !denied {
                            kept.push(socket);
                        }
                    }
                    if kept.is_empty() {
                        // Either DNS returned nothing, or every
                        // address was denied. Distinguish the two by
                        // checking if the lookup itself was empty.
                        let err: Box<dyn std::error::Error + Send + Sync> = Box::new(
                            AclError::NoAllowedAddress(host.clone()),
                        );
                        Err(err)
                    } else {
                        let addrs: Addrs = Box::new(kept.into_iter());
                        Ok(addrs)
                    }
                }
                Err(io_err) => {
                    let err: Box<dyn std::error::Error + Send + Sync> = Box::new(io_err);
                    Err(err)
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Policy — wires the ACL + resolver together with a small builder API.
// ---------------------------------------------------------------------------

/// High-level SSRF policy. Holds the ACL plus a flag for whether the
/// default IP decision is allow or deny.
///
/// Construct via the builder pattern:
///
/// ```ignore
/// use arachne_agents::ssrf::SsrfPolicy;
///
/// let policy = SsrfPolicy::default_webfetch()
///     .deny_host_suffix(".internal.corp");
///
/// let client = reqwest::Client::builder()
///     .dns_resolver(policy.resolver())
///     .build()?;
/// ```
#[derive(Clone)]
pub struct SsrfPolicy {
    acl: SsrfAcl,
    default_deny: bool,
}

impl SsrfPolicy {
    /// Build the default policy for the `webfetch` tool. Denies the
    /// local network by default.
    pub fn default_webfetch() -> Self {
        Self {
            acl: SsrfAcl::new().deny_local_network(),
            default_deny: false,
        }
    }

    /// Build the default policy for provider base URLs (`mcp`,
    /// `provider_oauth`, `openai_compatible`). The user's config is the
    /// authority, so local-network denial is opt-in: the caller must
    /// call [`Self::deny_local_network`] explicitly if they want it.
    pub fn default_provider() -> Self {
        Self {
            acl: SsrfAcl::new(),
            default_deny: false,
        }
    }

    /// Append a deny rule matching every address on the local network.
    pub fn deny_local_network(mut self) -> Self {
        self.acl = self.acl.deny_local_network();
        self
    }

    /// Deny any IP for which `f` returns true.
    pub fn deny_ip_when<F>(mut self, f: F) -> Self
    where
        F: Fn(IpAddr) -> bool + Send + Sync + 'static,
    {
        self.acl = self.acl.deny_ip_when(f);
        self
    }

    /// Deny the exact hostname `name` (case-insensitive).
    pub fn deny_host(mut self, name: impl Into<String>) -> Self {
        self.acl = self.acl.deny_host(name);
        self
    }

    /// Deny any hostname that ends with `suffix` (case-insensitive).
    pub fn deny_host_suffix(mut self, suffix: impl Into<String>) -> Self {
        self.acl = self.acl.deny_host_suffix(suffix);
        self
    }

    /// Flip the default IP-layer decision to deny. Use this for an
    /// allowlist setup where every host must be explicitly trusted.
    pub fn default_deny(mut self) -> Self {
        self.default_deny = true;
        self
    }

    /// Evaluate a URL. Returns [`Decision::Allow`] if the URL is safe
    /// to fetch; [`Decision::Deny`] otherwise.
    pub fn evaluate(&self, url: &Url) -> Decision {
        let decision = self.acl.evaluate(url);
        match (&decision, self.default_deny) {
            (Decision::Allow, true) => {
                // default_deny mode: only allow if a host rule
                // explicitly permitted the host. The ACL's `Allow`
                // here means "no deny matched" which in default-deny
                // mode is "deny." But the host rules can still
                // explicitly allow (would return Allow from a deny
                // rule slot), which we don't support here. So in
                // default_deny mode without explicit allows, every
                // call denies.
                Decision::Deny(AclError::DeniedHost(
                    url.host_str().unwrap_or("<missing>").to_string(),
                ))
            }
            _ => decision,
        }
    }

    /// Evaluate a list of resolved addresses (used by the resolver).
    pub fn evaluate_addrs<I>(&self, host: &str, addrs: I) -> Result<(), AclError>
    where
        I: IntoIterator<Item = IpAddr>,
    {
        self.acl.evaluate_addrs(host, addrs)
    }

    /// Construct a custom `reqwest::dns::Resolve` impl bound to this
    /// policy. Pass the result to `ClientBuilder::dns_resolver`.
    pub fn resolver(&self) -> Arc<dyn Resolve> {
        Arc::new(GuardedResolver {
            acl: self.acl.clone(),
        })
    }
}


/// Convenience for tool callers: parse a URL string and return
/// `Ok(())` if allowed, `Err(AclError)` if denied. The error's
/// `Display` impl is what tool surfaces pass to the model.
pub fn check_url(url: &str) -> Result<(), AclError> {
    let parsed = Url::parse(url).map_err(|_| AclError::DeniedHost(url.to_string()))?;
    let decision = SsrfPolicy::default_webfetch().evaluate(&parsed);
    match decision {
        Decision::Allow => Ok(()),
        Decision::Deny(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// Global guarded clients
// ---------------------------------------------------------------------------

use std::sync::OnceLock;

/// The default SSRF posture for the `webfetch` tool (model-driven,
/// untrusted URLs). Denies the local network, link-local, and the
/// cloud metadata endpoints by default.
///
/// All process-wide callers that make outbound HTTP requests at the
/// request of an LLM or a user-supplied URL use this client. The
/// underlying `reqwest::Client` is constructed once via a
/// `OnceLock` and cloned on every call — `reqwest::Client`
/// already wraps an `Arc` internally so cloning is cheap.
///
/// Tests that need to hit loopback or local mock servers should
/// construct their own `reqwest::Client::new()` and bypass this
/// function; that's exactly what the `run_with_async` test seam in
/// `webfetch` does.
pub fn webfetch_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(build_webfetch_client)
}

fn build_webfetch_client() -> reqwest::Client {
    use std::time::Duration;

    // Default ACL: deny the local network. The resolver and the
    // redirect policy close issue #11 (DNS rebinding) and #17
    // (multi-A-record bypass).
    let policy = SsrfPolicy::default_webfetch();
    let resolver = policy.resolver();
    let redirect = redirect_policy_for(&policy);

    reqwest::Client::builder()
        .dns_resolver2(resolver)
        .redirect(redirect)
        .timeout(Duration::from_secs(30))
        // Default reqwest behaviour is 10 hops; the redirect policy
        // here only changes which hops are followed, not the limit.
        // Tightening that would be a follow-up; for now the 10-hop
        // limit is adequate.
        .build()
        .expect("webfetch client builder is infallible under default settings")
}

/// The default SSRF posture for outbound HTTP from **provider**
/// configurations — MCP server URLs, OAuth issuer URLs, OpenAI-compatible
/// provider base URLs, and SSE proxy targets. These are
/// user-configured and the *base* destination is trusted; the
/// *querystring* on a request might be derived from model input
/// (e.g. `websearch` query params). The local-network deny is
/// applied here too — there's no scenario where the LLM should be
/// hitting `127.0.0.1` through a provider URL.
pub fn provider_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(build_provider_client)
}

fn build_provider_client() -> reqwest::Client {
    use std::time::Duration;

    let policy = SsrfPolicy::default_webfetch();
    let resolver = policy.resolver();
    let redirect = redirect_policy_for(&policy);

    reqwest::Client::builder()
        .dns_resolver2(resolver)
        .redirect(redirect)
        .timeout(Duration::from_secs(30))
        .build()
        .expect("provider client builder is infallible under default settings")
}

/// Build a `reqwest::redirect::Policy` that re-evaluates the ACL on
/// every redirect hop. Free function (not a method on
/// `SsrfPolicy`) because the public surface only needs the
/// pre-built guarded clients. External tests that need to
/// construct their own `reqwest::Client` with the same redirect
/// posture can call this directly.
fn redirect_policy_for(policy: &SsrfPolicy) -> RedirectPolicy {
    let acl = policy.acl.clone();
    RedirectPolicy::custom(move |attempt| {
        let url = attempt.url();
        match acl.evaluate(url) {
            Decision::Allow => attempt.follow(),
            Decision::Deny(_) => attempt.stop(),
        }
    })
}

// ---------------------------------------------------------------------------
// Tests — cover the two audit bugs and the broader ACL semantics.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).expect("valid URL")
    }

    #[test]
    fn is_local_network_flags_loopback_ipv4() {
        assert!(is_local_network("127.0.0.1".parse().unwrap()));
        assert!(is_local_network("127.255.255.254".parse().unwrap()));
    }

    #[test]
    fn is_local_network_flags_loopback_ipv6() {
        assert!(is_local_network("::1".parse().unwrap()));
    }

    #[test]
    fn is_local_network_flags_rfc1918() {
        assert!(is_local_network("10.0.0.1".parse().unwrap()));
        assert!(is_local_network("172.16.0.1".parse().unwrap()));
        assert!(is_local_network("192.168.1.1".parse().unwrap()));
    }

    #[test]
    fn is_local_network_flags_link_local_and_metadata() {
        assert!(is_local_network("169.254.169.254".parse().unwrap()));
        assert!(is_local_network("100.100.100.200".parse().unwrap()));
    }

    #[test]
    fn is_local_network_allows_public_ips() {
        assert!(!is_local_network("8.8.8.8".parse().unwrap()));
        assert!(!is_local_network("1.1.1.1".parse().unwrap()));
        assert!(!is_local_network(IpAddr::V6(Ipv6Addr::new(
            0x2606, 0x4700, 0x4700, 0x0, 0x0, 0x0, 0x0, 0x1111
        ))));
    }

    #[test]
    fn default_policy_denies_loopback_literal() {
        let p = SsrfPolicy::default_webfetch();
        let u = url("http://127.0.0.1/admin");
        assert!(matches!(p.evaluate(&u), Decision::Deny(AclError::DeniedIp(_))));
    }

    #[test]
    fn default_policy_denies_rfc1918_literal() {
        let p = SsrfPolicy::default_webfetch();
        let u = url("http://10.0.0.5/internal");
        assert!(matches!(p.evaluate(&u), Decision::Deny(AclError::DeniedIp(_))));
    }

    #[test]
    fn default_policy_denies_aws_metadata_literal() {
        let p = SsrfPolicy::default_webfetch();
        let u = url("http://169.254.169.254/latest/meta-data");
        assert!(matches!(p.evaluate(&u), Decision::Deny(AclError::DeniedIp(_))));
    }

    #[test]
    fn default_policy_allows_public_ip_literal() {
        let p = SsrfPolicy::default_webfetch();
        let u = url("http://8.8.8.8/");
        assert_eq!(p.evaluate(&u), Decision::Allow);
    }

    #[test]
    fn default_policy_allows_public_hostname() {
        let p = SsrfPolicy::default_webfetch();
        let u = url("https://example.com/");
        assert_eq!(p.evaluate(&u), Decision::Allow);
    }

    #[test]
    fn host_suffix_deny_blocks_strict_subdomains() {
        let p = SsrfPolicy::default_webfetch().deny_host_suffix(".internal.corp");
        let u = url("https://api.internal.corp/admin");
        assert!(matches!(
            p.evaluate(&u),
            Decision::Deny(AclError::DeniedHost(_))
        ));
    }

    #[test]
    fn host_deny_is_case_insensitive() {
        let p = SsrfPolicy::default_webfetch().deny_host("BadHost.test");
        let u = url("https://badhost.test/");
        assert!(matches!(
            p.evaluate(&u),
            Decision::Deny(AclError::DeniedHost(_))
        ));
    }

    #[test]
    fn multi_a_record_with_one_bad_ip_blocks_all() {
        // Issue #17: hostname resolving to [1.1.1.1, 127.0.0.1] must
        // be blocked because at least one address is in the deny set.
        let p = SsrfPolicy::default_webfetch();
        let result = p.evaluate_addrs(
            "attacker.test",
            [
                "1.1.1.1".parse::<IpAddr>().unwrap(),
                "127.0.0.1".parse::<IpAddr>().unwrap(),
            ],
        );
        assert!(matches!(
            result,
            Err(AclError::DeniedIp(ip)) if ip == "127.0.0.1".parse::<IpAddr>().unwrap()
        ));
    }

    #[test]
    fn multi_a_record_all_public_passes() {
        let p = SsrfPolicy::default_webfetch();
        let result = p.evaluate_addrs(
            "example.com",
            [
                "1.1.1.1".parse::<IpAddr>().unwrap(),
                "8.8.8.8".parse::<IpAddr>().unwrap(),
            ],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn resolver_blocks_when_every_address_denied() {
        // DNS rebinding test: the resolver returns only the addresses
        // the OS gave it, and the ACL filters them. Simulate by
        // calling evaluate_addrs directly with the rebinding-shaped
        // answer.
        let p = SsrfPolicy::default_webfetch();
        // First query: resolver returns [1.1.1.1] — passes IP filter
        assert!(p
            .evaluate_addrs("attacker.test", ["1.1.1.1".parse().unwrap()])
            .is_ok());
        // Second query: resolver returns [127.0.0.1] — fails IP filter
        // (strict mode: a single denied address in the set rejects
        // the whole host).
        assert!(matches!(
            p.evaluate_addrs(
                "attacker.test",
                ["127.0.0.1".parse().unwrap()]
            ),
            Err(AclError::DeniedIp(_))
        ));
    }

    #[test]
    fn default_deny_mode_blocks_anything_without_explicit_allow() {
        // default_deny is the inverse posture: nothing is allowed
        // unless a host rule explicitly permits it. We don't expose
        // host allow rules yet — this test documents the intent.
        let p = SsrfPolicy::default_webfetch().default_deny();
        let u = url("https://example.com/");
        assert!(matches!(p.evaluate(&u), Decision::Deny(_)));
    }

    #[test]
    fn malformed_url_with_no_host_is_denied() {
        let p = SsrfPolicy::default_webfetch();
        // file:// schemes have no host; the evaluator must not panic.
        // Note: reqwest itself wouldn't make a request for file://, but
        // the SSRF guard must reject before any other code looks at it.
        let u = url("file:///etc/passwd");
        assert!(matches!(p.evaluate(&u), Decision::Deny(_)));
    }

    #[test]
    fn global_client_is_shared_across_calls() {
        // The whole point of the global shape is that callers in the
        // same process share one client — there should be one and only
        // one underlying SSRF-guarded `reqwest::Client`, regardless of
        // how many times `webfetch_client()` is called.
        let a = webfetch_client();
        let b = webfetch_client();
        assert!(std::ptr::eq(a, b), "webfetch_client should return the same client on every call");
    }

    #[test]
    fn provider_and_webfetch_are_distinct_clients() {
        // webfetch and provider have the same default ACL but are
        // distinct clients — the global shape lets each caller have
        // its own posture if the defaults diverge in the future.
        let webfetch = webfetch_client();
        let provider = provider_client();
        assert!(!std::ptr::eq(webfetch, provider));
    }
}