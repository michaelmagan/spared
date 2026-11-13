use serde::Deserialize;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use wasmtime::Engine;
use wasmtime::component::Component;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: String,
    /// Which WASI the component targets. `wasip3-experimental` serves several requests at once in
    /// one instance; it is off unless the operator turns it on (`--experimental-wasip3`).
    #[serde(default)]
    pub runtime: Runtime,
    #[serde(default)]
    pub grants: Grants,
    #[serde(default)]
    pub limits: Limits,
}

/// The WASI an app is built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
pub enum Runtime {
    /// One request at a time per instance, through `wasi:http/incoming-handler@0.2`.
    #[default]
    #[serde(rename = "wasip2")]
    Wasip2,
    /// EXPERIMENTAL: `wasi:http/handler@0.3`, several requests in flight in one instance.
    #[serde(rename = "wasip3-experimental")]
    Wasip3Experimental,
}

impl Runtime {
    pub fn as_str(self) -> &'static str {
        match self {
            Runtime::Wasip2 => "wasip2",
            Runtime::Wasip3Experimental => "wasip3-experimental",
        }
    }

    pub fn is_p3(self) -> bool {
        self == Runtime::Wasip3Experimental
    }
}

/// Requests one wasip3-experimental instance may have in flight when the operator sets nothing.
pub const DEFAULT_CONCURRENCY: u32 = 4;
/// The highest `--max-app-concurrency` an operator may set.
pub const MAX_CONCURRENCY: u32 = 64;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grants {
    #[serde(default = "yes")]
    pub data: bool,
    #[serde(default)]
    pub fetch: Vec<String>,
}

impl Default for Grants {
    fn default() -> Self {
        Grants {
            data: true,
            fetch: vec![],
        }
    }
}

/// The manifest format this spared reads. `schema = 1` in spared.toml names it; leaving the line
/// out means the same. A larger number was written for a newer spared.
pub const MANIFEST_SCHEMA: u32 = 1;

/// The fix line for a spared.toml that fails to parse; unknown keys are rejected.
pub const FETCH_ENTRY_FIX: &str =
    "fetch takes host names, for example example.com or *.example.com";

pub const MANIFEST_KEYS_FIX: &str = "valid keys: schema, name, runtime, [grants] data, fetch, [limits] memory_mb, cpu_ms, deadline_ms, max_outbound, concurrency";

/// Why a spared.toml cannot be used, with the same kind, message and fix the CLI and the server
/// both report.
#[derive(Debug, Clone)]
pub struct ManifestError {
    pub kind: &'static str,
    pub message: String,
    pub fix: String,
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ManifestError {}

impl Manifest {
    /// Parse a spared.toml. A `schema` from the future and the reserved `[secrets]` table are
    /// refused first, by name, so a manifest written for a newer spared says so instead of
    /// failing on whichever new key comes first.
    pub fn parse(text: &str) -> Result<Manifest, ManifestError> {
        let manifest = Manifest::parse_unchecked(text)?;
        manifest.grants.check_fetch()?;
        manifest.check_runtime()?;
        Ok(manifest)
    }

    /// The rules of `runtime = "wasip3-experimental"`: no data, and `concurrency` only there.
    pub fn check_runtime(&self) -> Result<(), ManifestError> {
        match (self.runtime, self.limits.concurrency) {
            (Runtime::Wasip3Experimental, _) if self.grants.data => Err(ManifestError {
                kind: "wasip3_data_refused",
                message: "runtime = \"wasip3-experimental\" is only for apps without data, and this spared.toml has data = true (the default)".into(),
                fix: "set `data = false` under [grants], or remove the runtime line to run the app as wasip2. Requests in flight in one wasip3 instance share one SQLite connection, so a database app is refused".into(),
            }),
            (Runtime::Wasip3Experimental, Some(0)) => Err(ManifestError {
                kind: "bad_manifest",
                message: "limits.concurrency must be at least 1".into(),
                fix: "set limits.concurrency to 1 or more, or leave it out for the server's default".into(),
            }),
            (Runtime::Wasip2, Some(_)) => Err(ManifestError {
                kind: "bad_manifest",
                message: "limits.concurrency applies only to runtime = \"wasip3-experimental\"; a wasip2 instance runs one request at a time".into(),
                fix: "remove limits.concurrency".into(),
            }),
            _ => Ok(()),
        }
    }

    fn parse_unchecked(text: &str) -> Result<Manifest, ManifestError> {
        let bad = |message: String| ManifestError {
            kind: "bad_manifest",
            message,
            fix: MANIFEST_KEYS_FIX.into(),
        };
        let mut table: toml::Table = text.parse().map_err(|e| bad(format!("{e}")))?;
        if let Some(v) = table.remove("schema") {
            match v.as_integer() {
                Some(MANIFEST_SCHEMA_I64) => {}
                Some(n) => {
                    return Err(ManifestError {
                        kind: "unsupported_schema",
                        message: format!(
                            "spared.toml says schema = {n}; this spared reads schema {MANIFEST_SCHEMA}"
                        ),
                        fix: "upgrade spared (`spared --version` shows what you run), or write `schema = 1` if the file really is a version 1 manifest".into(),
                    });
                }
                None => return Err(bad("schema must be an integer, such as schema = 1".into())),
            }
        }
        if table.contains_key("secrets") {
            return Err(ManifestError {
                kind: "secrets_not_supported",
                message: "spared.toml has a secrets key: [secrets] is reserved, and this spared does not provide secrets".into(),
                fix: "remove [secrets]. a secret can only live in the app's code or database, and the server stores both unencrypted, so do not put secret values in spared.toml".into(),
            });
        }
        Manifest::deserialize(table).map_err(|e| bad(format!("{e}")))
    }

    /// Parse a manifest stored by an earlier deploy. A `fetch` entry that a newer spared refuses
    /// (it can never match a host) must not take the whole app down at start, so it is dropped
    /// and named in the returned warnings. When every entry is unusable the list is kept as it is
    /// (none of them can match, so each fetch is still denied) because an app with no `fetch`
    /// at all would no longer be allowed its outbound-HTTP import and would not load.
    pub fn parse_stored(text: &str) -> Result<(Manifest, Vec<String>), ManifestError> {
        let mut manifest = Manifest::parse_unchecked(text)?;
        let mut warnings = vec![];
        let usable: Vec<String> = manifest
            .grants
            .fetch
            .iter()
            .filter(|entry| {
                let one = Grants {
                    fetch: vec![(*entry).clone()],
                    ..Grants::default()
                };
                match one.check_fetch() {
                    Ok(()) => true,
                    Err(e) => {
                        warnings.push(e.message);
                        false
                    }
                }
            })
            .cloned()
            .collect();
        if !warnings.is_empty() && !usable.is_empty() {
            manifest.grants.fetch = usable;
        }
        Ok((manifest, warnings))
    }
}

impl Grants {
    /// Refuse a `fetch` entry that can never match a host, so it fails at deploy instead of as a
    /// denied call on every request.
    pub fn check_fetch(&self) -> Result<(), ManifestError> {
        for entry in &self.fetch {
            let refuse = |why: &str, fix: &str| ManifestError {
                kind: "bad_manifest",
                message: format!("fetch entry '{entry}' {why}"),
                fix: fix.into(),
            };
            if entry.is_empty() {
                return Err(refuse("is empty", FETCH_ENTRY_FIX));
            }
            if entry.contains("://") {
                return Err(refuse(
                    "has a scheme, but fetch takes host names only",
                    FETCH_ENTRY_FIX,
                ));
            }
            if let Ok(ip) = entry
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<IpAddr>()
            {
                if ip_denied(ip) {
                    return Err(refuse(
                        "is a private, loopback, link-local or metadata address",
                        "those addresses can never be granted, so no fetch entry can allow them. Use a public host name, for example example.com",
                    ));
                }
                continue;
            }
            if entry.contains('/') || entry.contains('?') || entry.contains('#') {
                return Err(refuse(
                    "has a path, but fetch takes host names only",
                    FETCH_ENTRY_FIX,
                ));
            }
            if entry.contains('@') {
                return Err(refuse(
                    "has a user name, but fetch takes host names only",
                    FETCH_ENTRY_FIX,
                ));
            }
            if entry.contains(':') {
                return Err(refuse(
                    "has a port, but fetch matches the host name only, on any port",
                    FETCH_ENTRY_FIX,
                ));
            }
            let name = entry.strip_prefix("*.").unwrap_or(entry);
            if entry == "*" {
                return Err(refuse(
                    "would allow every host, which fetch does not offer",
                    FETCH_ENTRY_FIX,
                ));
            }
            let ok = !name.is_empty()
                && !name.starts_with('.')
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
            if !ok {
                return Err(refuse("is not a host name", FETCH_ENTRY_FIX));
            }
        }
        Ok(())
    }
}

const MANIFEST_SCHEMA_I64: i64 = MANIFEST_SCHEMA as i64;

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(default = "d_mem")]
    pub memory_mb: usize,
    #[serde(default = "d_cpu")]
    pub cpu_ms: u64,
    #[serde(default = "d_deadline")]
    pub deadline_ms: u64,
    #[serde(default = "d_out")]
    pub max_outbound: u32,
    /// wasip3-experimental only: requests one instance may have in flight. `None` takes the
    /// server's `--max-app-concurrency`; a manifest may ask for less, never more.
    #[serde(default)]
    pub concurrency: Option<u32>,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            memory_mb: d_mem(),
            cpu_ms: d_cpu(),
            deadline_ms: d_deadline(),
            max_outbound: d_out(),
            concurrency: None,
        }
    }
}

/// Operator ceilings. An app's spared.toml can ask for less, never more: deploy refuses a
/// manifest above them and loading an app from disk clamps to them.
#[derive(Debug, Clone)]
pub struct Maxima {
    pub memory_mb: usize,
    pub cpu_ms: u64,
    pub deadline_ms: u64,
    pub max_outbound: u32,
    pub request_mb: usize,
    pub response_mb: usize,
    /// Requests one wasip3-experimental instance may have in flight (`--max-app-concurrency`).
    pub concurrency: u32,
}

impl Default for Maxima {
    fn default() -> Self {
        Maxima {
            memory_mb: 256,
            cpu_ms: 5000,
            deadline_ms: 30000,
            max_outbound: 1000,
            request_mb: 8,
            response_mb: 16,
            concurrency: DEFAULT_CONCURRENCY,
        }
    }
}

impl Limits {
    fn fields(&self, max: &Maxima) -> [(&'static str, u64, u64, &'static str); 4] {
        [
            (
                "memory_mb",
                self.memory_mb as u64,
                max.memory_mb as u64,
                "--max-memory-mb",
            ),
            ("cpu_ms", self.cpu_ms, max.cpu_ms, "--max-cpu-ms"),
            (
                "deadline_ms",
                self.deadline_ms,
                max.deadline_ms,
                "--max-deadline-ms",
            ),
            (
                "max_outbound",
                self.max_outbound as u64,
                max.max_outbound as u64,
                "--max-outbound",
            ),
        ]
    }

    /// The first limit above the server's maximum: (field, asked, maximum, serve flag).
    pub fn over(&self, max: &Maxima) -> Option<(&'static str, u64, u64, &'static str)> {
        self.fields(max)
            .into_iter()
            .find(|(_, v, m, _)| v > m)
            .or_else(|| {
                let c = self.concurrency?;
                (c > max.concurrency).then_some((
                    "concurrency",
                    c as u64,
                    max.concurrency as u64,
                    "--max-app-concurrency",
                ))
            })
    }

    /// Requests one wasip3-experimental instance may have in flight under `max`.
    pub fn concurrency_in(&self, max: &Maxima) -> u32 {
        self.concurrency
            .unwrap_or(max.concurrency)
            .clamp(1, max.concurrency.max(1))
    }

    /// Lower every limit to the maximum. Returns true if anything changed.
    pub fn clamp(&mut self, max: &Maxima) -> bool {
        let over = self.over(max).is_some();
        self.memory_mb = self.memory_mb.min(max.memory_mb);
        self.cpu_ms = self.cpu_ms.min(max.cpu_ms);
        self.deadline_ms = self.deadline_ms.min(max.deadline_ms);
        self.max_outbound = self.max_outbound.min(max.max_outbound);
        self.concurrency = self.concurrency.map(|c| c.min(max.concurrency));
        over
    }
}

fn d_mem() -> usize {
    64
}
fn d_cpu() -> u64 {
    1000
}
fn d_deadline() -> u64 {
    5000
}
fn d_out() -> u32 {
    10
}

/// An app name: 1 to 40 lowercase letters, digits and dashes, not starting or ending with a dash.
pub fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 40
        && !n.starts_with('-')
        && !n.ends_with('-')
        && n.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Imports the component asks for that the manifest and runtime do not grant. Empty means
/// loadable. Each import is matched with its version: a `wasi:filesystem@0.3` import is not the
/// `wasi:filesystem@0.2` that `data = true` grants, and an import without a version is refused.
pub fn ungranted_imports(
    engine: &Engine,
    comp: &Component,
    grants: &Grants,
    runtime: Runtime,
) -> Vec<String> {
    comp.component_type()
        .imports(engine)
        .filter(|(name, _)| !import_granted(name, grants, runtime))
        .map(|(name, _)| name.to_string())
        .collect()
}

fn import_granted(name: &str, grants: &Grants, runtime: Runtime) -> bool {
    let Some((iface, version)) = name.split_once('@') else {
        return false;
    };
    let p2 = version.starts_with("0.2.");
    let p3 = version.starts_with("0.3.") && runtime.is_p3();
    let any = |prefixes: &[&str]| prefixes.iter().any(|p| iface.starts_with(p));
    if p2 {
        if any(&["wasi:cli/", "wasi:clocks/", "wasi:random/", "wasi:io/"]) {
            return true;
        }
        if iface.starts_with("wasi:filesystem/") {
            return grants.data;
        }
        // A wasip3 app gets HTTP through wasi:http 0.3 only.
        return match iface {
            "wasi:http/types" => !runtime.is_p3(),
            "wasi:http/outgoing-handler" => !runtime.is_p3() && !grants.fetch.is_empty(),
            _ => false,
        };
    }
    if p3 {
        // Never wasi:filesystem, wasi:sockets or wasi:cli at 0.3: the host does not link them
        // (std's file and console calls use the 0.2 interfaces above).
        if any(&["wasi:clocks/", "wasi:random/"]) || iface == "wasi:http/types" {
            return true;
        }
        if iface == "wasi:http/client" {
            return !grants.fetch.is_empty();
        }
    }
    false
}

pub fn ip_denied(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_denied(v4),
        IpAddr::V6(v6) => v6_denied(v6),
    }
}

/// Provider platform endpoints that sit on addresses no range rule above covers. Azure's
/// WireServer and DNS/metadata virtual IP is the one known with certainty; AWS, GCP, DigitalOcean
/// (169.254.169.254, 169.254.169.253), Alibaba (100.100.100.200) and Oracle (192.0.0.192)
/// are already inside the link-local, CGNAT and 192.0.0.0/24 rules.
const PROVIDER_ENDPOINTS: [Ipv4Addr; 1] = [Ipv4Addr::new(168, 63, 129, 16)];

fn v4_denied(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    PROVIDER_ENDPOINTS.contains(&ip)
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || o[0] == 0
        || (o[0] == 100 && (o[1] & 0xc0) == 64)
        || (o[0] == 198 && (o[1] & 0xfe) == 18)
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        || (o[0] == 192 && o[1] == 88 && o[2] == 99)
        || o[0] >= 240
}

fn v6_denied(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return v4_denied(v4);
    }
    let s = ip.segments();
    let embedded_v4 = Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8);
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (s[..6] == [0; 6] && v4_denied(embedded_v4))
        || (s[..4] == [0; 4] && s[4] == 0xffff && s[5] == 0)
        || (s[0] & 0xfe00) == 0xfc00
        || (s[0] & 0xffc0) == 0xfe80
        || (s[0] & 0xffc0) == 0xfec0
        || (s[0] == 0x64 && s[1] == 0xff9b)
        || (s[0] == 0x100 && s[1..4] == [0; 3])
        || (s[0] == 0x2001 && (s[1] == 0 || s[1] == 0xdb8))
        || (s[0] == 0x2002)
}

/// The host part of an authority or Host header (`host`, `host:port`, `[v6]:port`), lowercased and
/// without a trailing dot, so `API.Example.com.:443` and `api.example.com` compare equal.
pub fn norm_host(authority: &str) -> String {
    let host = match authority.strip_prefix('[') {
        Some(rest) => &authority[..rest.find(']').map(|i| i + 2).unwrap_or(authority.len())],
        None => authority.split(':').next().unwrap_or(""),
    };
    host.trim_end_matches('.').to_ascii_lowercase()
}

pub fn host_allowed(allow: &[String], host: &str) -> bool {
    let host = norm_host(host)
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    allow.iter().any(|a| {
        let a = a
            .trim_end_matches('.')
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_ascii_lowercase();
        match a.strip_prefix("*.") {
            Some(suffix) => host.ends_with(&format!(".{suffix}")),
            None => host == a,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denies_private_and_metadata() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fe80::1",
            "fd00::1",
            "::ffff:169.254.169.254",
            "::ffff:127.0.0.1",
            "fd00:ec2::254",
            "::7f00:1",
            "::169.254.169.254",
            "::ffff:0:7f00:1",
            "2001::1",
            "2001:db8::1",
            "fec0::1",
            "100::1",
            "64:ff9b::a9fe:a9fe",
            "192.0.0.8",
            "192.0.0.192",
            "100.100.100.200",
            "168.63.129.16",
            "::ffff:168.63.129.16",
            "64:ff9b::a83f:8110",
            "192.88.99.1",
        ] {
            assert!(ip_denied(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "93.184.216.34",
            "1.1.1.1",
            "168.63.129.15",
            "168.63.129.17",
            "2606:4700:4700::1111",
        ] {
            assert!(!ip_denied(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn host_matching() {
        let a = vec!["api.example.com".to_string(), "*.cdn.test".to_string()];
        assert!(host_allowed(&a, "api.example.com"));
        assert!(host_allowed(&a, "x.cdn.test"));
        assert!(!host_allowed(&a, "cdn.test"));
        assert!(!host_allowed(&a, "evil-api.example.com"));
        assert!(host_allowed(&a, "API.Example.COM"));
        assert!(host_allowed(&a, "api.example.com."));
        assert!(host_allowed(&a, "X.CDN.test."));
        assert!(!host_allowed(&a, "cdn.test."));
        let dotted = vec!["api.example.com.".to_string()];
        assert!(host_allowed(&dotted, "api.example.com"));
    }

    #[test]
    fn host_normalising() {
        assert_eq!(norm_host("API.Example.com.:443"), "api.example.com");
        assert_eq!(norm_host("example.com"), "example.com");
        assert_eq!(norm_host("[::1]:8080"), "[::1]");
        assert_eq!(norm_host("[::1]"), "[::1]");
        assert_eq!(norm_host("1.2.3.4:80"), "1.2.3.4");
    }

    #[test]
    fn limits_capped_by_maxima() {
        let max = Maxima::default();
        let mut l = Limits::default();
        assert_eq!(l.over(&max), None);
        l.cpu_ms = 1_000_000_000_000;
        l.memory_mb = 1_000_000;
        let (field, asked, cap, flag) = l.over(&max).unwrap();
        assert_eq!(
            (field, asked, cap, flag),
            ("memory_mb", 1_000_000, 256, "--max-memory-mb")
        );
        assert!(l.clamp(&max));
        assert_eq!((l.memory_mb, l.cpu_ms), (256, 5000));
        assert_eq!(l.over(&max), None);
        assert!(!l.clamp(&max));
        let m: Manifest =
            toml::from_str("name = \"x\"\n[limits]\nmax_outbound = 4000000000\n").unwrap();
        assert_eq!(m.limits.over(&max).map(|o| o.0), Some("max_outbound"));
    }

    #[test]
    fn wasip3_is_named_by_runtime_and_refused_for_data_apps() {
        let m = Manifest::parse("name = \"x\"\n").unwrap();
        assert_eq!(m.runtime, Runtime::Wasip2);
        let p3 = "name = \"x\"\nruntime = \"wasip3-experimental\"\n";
        let e = Manifest::parse(p3).unwrap_err();
        assert_eq!(e.kind, "wasip3_data_refused");
        assert!(e.fix.contains("data = false"), "{}", e.fix);
        let m = Manifest::parse(&format!("{p3}[grants]\ndata = false\n")).unwrap();
        assert_eq!(m.runtime, Runtime::Wasip3Experimental);
        assert_eq!(m.limits.concurrency, None);
        assert!(Manifest::parse("name = \"x\"\nruntime = \"wasip3\"\n").is_err());
        let e = Manifest::parse("name = \"x\"\n[limits]\nconcurrency = 2\n").unwrap_err();
        assert!(e.message.contains("only to runtime"), "{}", e.message);
        let e = Manifest::parse(&format!(
            "{p3}[grants]\ndata = false\n[limits]\nconcurrency = 0\n"
        ))
        .unwrap_err();
        assert_eq!(e.kind, "bad_manifest");
    }

    #[test]
    fn concurrency_may_ask_for_less_than_the_operator_allows_never_more() {
        let max = Maxima::default();
        let mut l = Limits::default();
        assert_eq!(l.concurrency_in(&max), DEFAULT_CONCURRENCY);
        l.concurrency = Some(2);
        assert_eq!(l.over(&max), None);
        assert_eq!(l.concurrency_in(&max), 2);
        l.concurrency = Some(9);
        assert_eq!(
            l.over(&max),
            Some(("concurrency", 9, 4, "--max-app-concurrency"))
        );
        assert!(l.clamp(&max));
        assert_eq!(l.concurrency_in(&max), 4);
    }

    #[test]
    fn unknown_manifest_keys_are_rejected() {
        for (text, key) in [
            ("name = \"x\"\nnmae = 1\n", "nmae"),
            ("name = \"x\"\n[grants]\ndat = true\n", "dat"),
            ("name = \"x\"\n[limits]\nmemory = 8\n", "memory"),
        ] {
            let e = toml::from_str::<Manifest>(text).unwrap_err().to_string();
            assert!(e.contains(key), "{e}");
        }
        assert!(toml::from_str::<Manifest>("name = \"x\"\n[grants]\ndata = false\n").is_ok());
    }

    #[test]
    fn schema_one_or_none_is_accepted_and_a_future_one_is_refused_by_name() {
        assert!(Manifest::parse("name = \"x\"\n").is_ok());
        assert!(Manifest::parse("schema = 1\nname = \"x\"\n").is_ok());
        let e = Manifest::parse("schema = 2\nname = \"x\"\n").unwrap_err();
        assert_eq!(e.kind, "unsupported_schema");
        assert!(e.message.contains("schema = 2"), "{}", e.message);
        assert!(e.fix.contains("upgrade spared"), "{}", e.fix);
        // A newer manifest's new keys must not hide the real reason.
        let e = Manifest::parse("schema = 3\nname = \"x\"\nbrand_new = true\n").unwrap_err();
        assert_eq!(e.kind, "unsupported_schema");
        for bad in [
            "schema = \"1\"\nname = \"x\"\n",
            "schema = 0\nname = \"x\"\n",
        ] {
            assert!(Manifest::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_secrets_table_is_reserved_and_refused() {
        for text in [
            "name = \"x\"\n[secrets]\nAPI_KEY = \"abc\"\n",
            "name = \"x\"\nsecrets = [\"API_KEY\"]\n",
        ] {
            let e = Manifest::parse(text).unwrap_err();
            assert_eq!(e.kind, "secrets_not_supported", "{text}");
            assert!(e.message.contains("reserved"), "{}", e.message);
            assert!(!e.message.contains("abc"), "the value is not echoed");
        }
    }

    fn fetch_manifest(entry: &str) -> Result<Manifest, ManifestError> {
        Manifest::parse(&format!("name = \"x\"\n[grants]\nfetch = [{entry:?}]\n"))
    }

    #[test]
    fn a_stored_manifest_with_an_unusable_fetch_entry_loads_without_it() {
        let text =
            "name = \"x\"\n[grants]\nfetch = [\"https://example.com/a\", \"example.org\", \"*\"]\n";
        assert!(Manifest::parse(text).is_err());
        let (m, warnings) = Manifest::parse_stored(text).unwrap();
        assert_eq!(m.grants.fetch, vec!["example.org".to_string()]);
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].contains("https://example.com/a"));
        let all_bad = "name = \"x\"\n[grants]\nfetch = [\"*\"]\n";
        let (m, warnings) = Manifest::parse_stored(all_bad).unwrap();
        assert_eq!(
            m.grants.fetch,
            vec!["*".to_string()],
            "kept, so the app still loads"
        );
        assert_eq!(warnings.len(), 1);
        assert!(Manifest::parse_stored("schema = 2\nname = \"x\"\n").is_err());
        let (_, none) = Manifest::parse_stored("name = \"x\"\n").unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn fetch_entries_that_can_never_match_are_refused_at_parse() {
        for entry in [
            "https://example.com",
            "http://example.com",
            "example.com/path",
            "example.com:8080",
            "user@example.com",
            "*",
            "",
            "exa mple.com",
            "*example.com",
            "**.example.com",
            ".example.com",
        ] {
            let e = fetch_manifest(entry).unwrap_err();
            assert_eq!(e.kind, "bad_manifest", "{entry}");
            assert!(e.message.contains("fetch entry"), "{entry}: {}", e.message);
            assert!(
                e.fix
                    .contains("host names, for example example.com or *.example.com"),
                "{entry}: {}",
                e.fix
            );
        }
    }

    #[test]
    fn private_address_literals_are_refused_with_the_reason() {
        for entry in [
            "127.0.0.1",
            "10.0.0.5",
            "169.254.169.254",
            "::1",
            "[::1]",
            "192.168.1.1",
        ] {
            let e = fetch_manifest(entry).unwrap_err();
            assert_eq!(e.kind, "bad_manifest", "{entry}");
            assert!(e.fix.contains("can never be granted"), "{entry}: {}", e.fix);
        }
    }

    #[test]
    fn host_names_wildcard_subdomains_and_public_addresses_are_accepted() {
        for entry in [
            "example.com",
            "api.example.com",
            "*.example.com",
            "api.example.com.",
            "my_host.example.com",
            "xn--bcher-kva.example",
            "93.184.216.34",
        ] {
            assert!(fetch_manifest(entry).is_ok(), "{entry}");
        }
        assert!(Manifest::parse("name = \"x\"\n").is_ok());
    }

    #[test]
    fn parse_keeps_the_unknown_key_error() {
        let e = Manifest::parse("name = \"x\"\nnmae = 1\n").unwrap_err();
        assert_eq!(e.kind, "bad_manifest");
        assert!(e.message.contains("nmae"), "{}", e.message);
    }
}
