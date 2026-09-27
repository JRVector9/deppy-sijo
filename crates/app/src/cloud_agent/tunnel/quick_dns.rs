//! Recover newly published Quick Tunnel names from stale system negative DNS entries.
//! Applies only to the automatic unauthenticated metadata probe, never manual hosts.

use std::{
    io::Read,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};
use ureq::unversioned::{
    resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver},
    transport::NextTimeout,
};

#[derive(Debug)]
pub(super) struct QuickResolver {
    system: Arc<dyn Resolver>,
    lookup: ureq::Agent,
    endpoint: String,
    backup: String,
    backup_after: Duration,
    born: Instant,
}
impl QuickResolver {
    pub(super) fn new() -> Self {
        Self::with_backup_delay(Duration::from_secs(20))
    }
    pub(super) fn with_backup_delay(backup_after: Duration) -> Self {
        Self {
            system: Arc::new(DefaultResolver::default()),
            lookup: ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(2)))
                .max_redirects(0)
                .build()
                .into(),
            endpoint: "https://cloudflare-dns.com/dns-query".into(),
            backup: "https://dns.google/resolve".into(),
            backup_after,
            born: Instant::now(),
        }
    }
}
impl Resolver for QuickResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let started = Instant::now();
        let error = match self.system.resolve(uri, config, timeout) {
            Ok(addrs) => return Ok(addrs),
            Err(error) => error,
        };
        let Some(host) = uri.host() else {
            return Err(error);
        };
        if !matches!(error, ureq::Error::HostNotFound | ureq::Error::Io(_))
            || config.proxy().is_some_and(|proxy| !proxy.is_no_proxy(uri))
            || uri.scheme_str() != Some("https")
            || uri.port_u16().is_some_and(|port| port != 443)
            || super::generated_host(&format!("https://{host}")).is_none()
        {
            return Err(error);
        }
        // Leave part of the original request deadline for TCP/TLS and metadata.
        let budget = (*timeout.after).min(Duration::from_secs(1));
        let mut result = self.empty();
        // Do not prime the independent resolver while the fresh name is still
        // being published. It is only a backstop for prolonged negative caching.
        let use_backup = self.born.elapsed() >= self.backup_after;
        for endpoint in std::iter::once(&self.endpoint).chain(use_backup.then_some(&self.backup)) {
            let provider_started = Instant::now();
            let provider_budget = if use_backup {
                Duration::from_millis(500)
            } else {
                budget
            };
            for (kind, numeric) in [("A", 1), ("AAAA", 28)] {
                if matches!(
                    (config.ip_family(), numeric),
                    (ureq::config::IpFamily::Ipv4Only, 28) | (ureq::config::IpFamily::Ipv6Only, 1)
                ) {
                    continue;
                }
                let left = budget
                    .saturating_sub(started.elapsed())
                    .min(provider_budget.saturating_sub(provider_started.elapsed()));
                if left.is_zero() {
                    break;
                }
                let privacy = if endpoint == &self.backup {
                    "&edns_client_subnet=0.0.0.0/0"
                } else {
                    ""
                };
                let url = format!("{endpoint}?name={host}&type={kind}{privacy}");
                let Ok(response) = self
                    .lookup
                    .get(&url)
                    .header("Accept", "application/dns-json")
                    .config()
                    .timeout_global(Some(left))
                    .build()
                    .call()
                else {
                    continue;
                };
                if response.status() != 200 {
                    continue;
                }
                let mut body = String::new();
                if response
                    .into_body()
                    .into_reader()
                    .take((super::MAX_LINE + 1) as u64)
                    .read_to_string(&mut body)
                    .is_err()
                    || body.len() > super::MAX_LINE
                {
                    continue;
                }
                for ip in dns_answers(&body, host, numeric)
                    .into_iter()
                    .take(16 - result.len())
                {
                    result.push(SocketAddr::new(ip, 443));
                }
                if !result.is_empty() {
                    return Ok(result);
                }
            }
        }
        if result.is_empty() {
            Err(error)
        } else {
            Ok(result)
        }
    }
}
fn dns_answers(text: &str, host: &str, kind: u64) -> Vec<IpAddr> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return vec![];
    };
    let matches_name = |name: &str| {
        name.strip_suffix('.')
            .unwrap_or(name)
            .eq_ignore_ascii_case(host)
    };
    if value["Status"].as_u64() != Some(0)
        || value["TC"].as_bool() == Some(true)
        || !value["Question"].as_array().is_some_and(|questions| {
            questions.len() == 1
                && questions[0]["name"].as_str().is_some_and(matches_name)
                && questions[0]["type"].as_u64() == Some(kind)
        })
    {
        return vec![];
    }
    value["Answer"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|answer| {
            answer["name"].as_str().is_some_and(matches_name)
                && answer["type"].as_u64() == Some(kind)
        })
        .filter_map(|answer| answer["data"].as_str()?.parse::<IpAddr>().ok())
        .filter(|ip| {
            matches!((kind, ip), (1, IpAddr::V4(_)) | (28, IpAddr::V6(_))) && public_ip(*ip)
        })
        .take(16)
        .collect()
}
fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            !ip.is_private()
                && !ip.is_loopback()
                && !ip.is_link_local()
                && !ip.is_unspecified()
                && !ip.is_multicast()
                && !ip.is_broadcast()
                && !ip.is_documentation()
                && ip.octets()[0] != 0
                && ip.octets()[0] < 240
                && !(ip.octets()[0] == 100 && (64..=127).contains(&ip.octets()[1]))
                && !(ip.octets()[0] == 198 && (18..=19).contains(&ip.octets()[1]))
                && !(ip.octets()[0] == 192 && ip.octets()[1] == 0 && ip.octets()[2] == 0)
                && !(ip.octets()[0] == 192 && ip.octets()[1] == 88 && ip.octets()[2] == 99)
        }
        IpAddr::V6(ip) => {
            (ip.segments()[0] & 0xe000) == 0x2000
                && !(ip.segments()[0] == 0x2001 && ip.segments()[1] == 0x0db8)
                && !(ip.segments()[0] == 0x2001 && ip.segments()[1] < 0x0200)
                && ip.segments()[0] != 0x2002
                && !(ip.segments()[0] == 0x3fff && ip.segments()[1] < 0x1000)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::Arc,
    };
    use ureq::unversioned::{
        resolver::{ResolvedSocketAddrs, Resolver},
        transport::NextTimeout,
    };

    #[derive(Debug)]
    struct Missing;
    impl Resolver for Missing {
        fn resolve(
            &self,
            _: &ureq::http::Uri,
            _: &ureq::config::Config,
            _: NextTimeout,
        ) -> Result<ResolvedSocketAddrs, ureq::Error> {
            Err(ureq::Error::HostNotFound)
        }
    }
    #[test]
    fn stale_primary_https_dns_can_use_an_independent_answer() {
        fn serve(body: &'static str) -> (String, std::thread::JoinHandle<()>) {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/dns-query", listener.local_addr().unwrap());
            let worker = std::thread::spawn(move || {
                listener.set_nonblocking(true).unwrap();
                let end = Instant::now() + Duration::from_secs(3);
                while Instant::now() < end {
                    if let Ok((mut stream, _)) = listener.accept() {
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(Duration::from_secs(1)))
                            .unwrap();
                        let mut request = [0; 4096];
                        stream.read(&mut request).unwrap();
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                    } else {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                }
            });
            (url, worker)
        }
        let (primary, p) = serve(r#"{"Status":3}"#);
        let (backup, b) = serve(
            r#"{"Status":0,"Question":[{"name":"fresh.trycloudflare.com","type":1}],"Answer":[{"name":"fresh.trycloudflare.com","type":1,"data":"104.16.230.132"}]}"#,
        );
        let mut resolver = QuickResolver::new();
        resolver.system = Arc::new(Missing);
        resolver.endpoint = primary;
        resolver.backup = backup;
        resolver.backup_after = Duration::ZERO;
        let found = resolver.resolve(
            &"https://fresh.trycloudflare.com/mcp".parse().unwrap(),
            &ureq::config::Config::default(),
            NextTimeout {
                after: Duration::from_secs(2).into(),
                reason: ureq::Timeout::Global,
            },
        );
        p.join().unwrap();
        b.join().unwrap();
        assert!(!found.unwrap().is_empty());
    }
    #[test]
    fn cached_system_dns_failure_uses_bounded_https_dns_answer() {
        for bypass in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                for _ in 0..1 {
                    let (mut stream, _) = listener.accept().unwrap();
                    stream
                        .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                        .unwrap();
                    let mut request = [0; 4096];
                    let n = stream.read(&mut request).unwrap();
                    let ipv6 = std::str::from_utf8(&request[..n])
                        .unwrap()
                        .contains("type=AAAA");
                    let (kind, ip) = if ipv6 {
                        (28, "2606:4700::6810:e684")
                    } else {
                        (1, "104.16.230.132")
                    };
                    let body = format!(
                        r#"{{"Status":0,"Question":[{{"name":"fresh.trycloudflare.com.","type":{kind}}}],"Answer":[{{"name":"fresh.trycloudflare.com.","type":{kind},"data":"{ip}"}}]}}"#
                    );
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .unwrap();
                }
            });
            let mut resolver = QuickResolver::new();
            resolver.system = Arc::new(Missing);
            resolver.endpoint = format!("http://{addr}/dns-query");
            let config = if bypass {
                ureq::Agent::config_builder()
                    .proxy(Some(
                        ureq::Proxy::builder(ureq::ProxyProtocol::Http)
                            .host("proxy.example.com")
                            .port(8080)
                            .no_proxy("*.trycloudflare.com")
                            .build()
                            .unwrap(),
                    ))
                    .build()
            } else {
                ureq::config::Config::default()
            };
            let found = resolver
                .resolve(
                    &"https://fresh.trycloudflare.com/mcp".parse().unwrap(),
                    &config,
                    NextTimeout {
                        after: std::time::Duration::from_secs(2).into(),
                        reason: ureq::Timeout::Global,
                    },
                )
                .unwrap();
            assert_eq!(found.len(), 1);
            assert_eq!(found[0], "104.16.230.132:443".parse().unwrap());
            server.join().unwrap();
        }
    }
    #[test]
    fn usable_a_answer_does_not_wait_for_an_unneeded_aaaa_lookup() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let asked = Arc::new(AtomicBool::new(false));
        let seen = asked.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            stream.read(&mut request).unwrap();
            let body = r#"{"Status":0,"Question":[{"name":"fresh.trycloudflare.com","type":1}],"Answer":[{"name":"fresh.trycloudflare.com","type":1,"data":"104.16.230.132"}]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            drop(stream);
            listener.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_millis(300);
            while Instant::now() < deadline {
                if let Ok((_stream, _)) = listener.accept() {
                    seen.store(true, Ordering::Release);
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        });
        let mut resolver = QuickResolver::new();
        resolver.system = Arc::new(Missing);
        resolver.endpoint = format!("http://{addr}/dns-query");
        let _ = resolver
            .resolve(
                &"https://fresh.trycloudflare.com/mcp".parse().unwrap(),
                &ureq::config::Config::default(),
                NextTimeout {
                    after: Duration::from_secs(2).into(),
                    reason: ureq::Timeout::Global,
                },
            )
            .unwrap();
        server.join().unwrap();
        assert!(
            !asked.load(Ordering::Acquire),
            "usable A answer must leave time for the actual HTTPS probe"
        );
    }
    #[test]
    fn reserved_addresses_are_not_probe_targets() {
        for value in [
            "198.18.0.1",
            "198.19.255.255",
            "192.0.0.1",
            "192.88.99.1",
            "2001:2::1",
            "2001:10::1",
            "2001:20::1",
            "2002::1",
        ] {
            assert!(!public_ip(value.parse().unwrap()), "{value}");
        }
    }

    #[test]
    fn unrelated_host_never_uses_the_fallback() {
        let mut resolver = QuickResolver::new();
        resolver.system = Arc::new(Missing);
        resolver.endpoint = "http://127.0.0.1:1".into();
        let error = resolver
            .resolve(
                &"https://example.com/mcp".parse().unwrap(),
                &ureq::config::Config::default(),
                NextTimeout {
                    after: std::time::Duration::from_secs(2).into(),
                    reason: ureq::Timeout::Global,
                },
            )
            .unwrap_err();
        assert!(matches!(error, ureq::Error::HostNotFound));
    }
    #[test]
    fn malformed_mismatched_and_private_dns_answers_are_rejected() {
        for text in [
            r#"{"Status":3,"Answer":[{"name":"fresh.trycloudflare.com","type":1,"data":"104.16.230.132"}]}"#,
            r#"{"Status":0,"Question":[{"name":"evil.com","type":1}],"Answer":[{"name":"fresh.trycloudflare.com","type":1,"data":"104.16.230.132"}]}"#,
            r#"{"Status":0,"Question":[{"name":"fresh.trycloudflare.com","type":1}],"Answer":[{"name":"fresh.trycloudflare.com","type":1,"data":"127.0.0.1"}]}"#,
            r#"{"Status":0,"Question":[{"name":"fresh.trycloudflare.com","type":1}],"Answer":[{"name":"evil.com","type":1,"data":"104.16.230.132"}]}"#,
        ] {
            assert!(dns_answers(text, "fresh.trycloudflare.com", 1).is_empty());
        }
    }
}
