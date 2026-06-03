use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::RwLock;

use anyhow::Context;
use hickory_proto::runtime::TokioRuntimeProvider;
use hickory_proto::xfer::Protocol;
use hickory_resolver::config::{LookupIpStrategy, NameServerConfig, ResolverConfig, ResolverOpts};
use hickory_resolver::name_server::{GenericConnector, TokioConnectionProvider};
use hickory_resolver::proto::rr::rdata::SRV;
use hickory_resolver::{Resolver, TokioResolver};
use once_cell::sync::Lazy;
use tokio::net::lookup_host;

use super::error::Error;

const SYSTEM_DNS_RESOLVER: &str = "system";

#[derive(Clone)]
enum DnsResolver {
    System,
    Hickory {
        uri: String,
        name_servers: Vec<NameServerConfig>,
        resolver: Arc<Resolver<GenericConnector<TokioRuntimeProvider>>>,
    },
}

pub fn get_default_dns_resolvers() -> Vec<String> {
    vec![SYSTEM_DNS_RESOLVER.to_string()]
}

fn bootstrap_ips_for_doh_host(host: &str) -> Option<Vec<IpAddr>> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Some(vec![ip]);
    }

    match host {
        "dns.alidns.com" => Some(vec![
            IpAddr::V4(Ipv4Addr::new(223, 5, 5, 5)),
            IpAddr::V4(Ipv4Addr::new(223, 6, 6, 6)),
            IpAddr::V6(Ipv6Addr::new(0x2400, 0x3200, 0, 0, 0, 0, 0, 1)),
            IpAddr::V6(Ipv6Addr::new(0x2400, 0x3200, 0xbaba, 0, 0, 0, 0, 1)),
        ]),
        _ => None,
    }
}

fn build_hickory_resolver(
    config: ResolverConfig,
) -> Arc<Resolver<GenericConnector<TokioRuntimeProvider>>> {
    let mut opt = ResolverOpts::default();
    opt.ip_strategy = LookupIpStrategy::Ipv4AndIpv6;
    Arc::new(
        TokioResolver::builder_with_config(config, TokioConnectionProvider::default())
            .with_options(opt)
            .build(),
    )
}

fn build_doh_resolver(raw: &str) -> Result<DnsResolver, Error> {
    let url = url::Url::parse(raw).map_err(|e| Error::InvalidUrl(e.to_string()))?;
    if url.scheme() != "https" {
        return Err(anyhow::anyhow!("unsupported dns resolver scheme: {}", url.scheme()).into());
    }

    let host = url
        .host_str()
        .with_context(|| format!("DoH resolver host is empty: {}", raw))?;
    let ips = bootstrap_ips_for_doh_host(host).with_context(|| {
        format!(
            "DoH resolver {} requires a known bootstrap IP; currently only dns.alidns.com or IP literals are supported",
            raw
        )
    })?;
    let port = url.port().unwrap_or(443);
    let http_endpoint = match url.path() {
        "" | "/" | "/dns-query" => None,
        path => Some(path.to_string()),
    };

    let name_servers = ips
        .into_iter()
        .map(|ip| NameServerConfig {
            socket_addr: SocketAddr::new(ip, port),
            protocol: Protocol::Https,
            tls_dns_name: Some(host.to_string()),
            http_endpoint: http_endpoint.clone(),
            trust_negative_responses: true,
            bind_addr: None,
        })
        .collect::<Vec<_>>();

    Ok(DnsResolver::Hickory {
        uri: raw.to_string(),
        name_servers: name_servers.clone(),
        resolver: build_hickory_resolver(ResolverConfig::from_parts(
            None,
            Vec::new(),
            name_servers,
        )),
    })
}

fn build_dns_resolver(raw: &str) -> Result<DnsResolver, Error> {
    if raw.eq_ignore_ascii_case(SYSTEM_DNS_RESOLVER) {
        return Ok(DnsResolver::System);
    }

    build_doh_resolver(raw)
}

fn build_dns_resolvers(raw_resolvers: &[String]) -> Result<Vec<DnsResolver>, Error> {
    let raw_resolvers = if raw_resolvers.is_empty() {
        get_default_dns_resolvers()
    } else {
        raw_resolvers.to_vec()
    };

    raw_resolvers
        .iter()
        .map(|raw| build_dns_resolver(raw))
        .collect()
}

pub fn validate_dns_resolvers(raw_resolvers: &[String]) -> Result<(), Error> {
    build_dns_resolvers(raw_resolvers).map(|_| ())
}

pub fn set_dns_resolvers(raw_resolvers: Vec<String>) -> Result<(), Error> {
    let resolvers = build_dns_resolvers(&raw_resolvers)?;
    *DNS_RESOLVERS.write().unwrap() = resolvers;
    Ok(())
}

static DNS_RESOLVERS: Lazy<RwLock<Vec<DnsResolver>>> =
    Lazy::new(|| RwLock::new(build_dns_resolvers(&get_default_dns_resolvers()).unwrap()));

static SYSTEM_RECORD_RESOLVER: Lazy<Option<Arc<Resolver<GenericConnector<TokioRuntimeProvider>>>>> =
    Lazy::new(|| {
        TokioResolver::builder_tokio().ok().map(|mut builder| {
            builder.options_mut().ip_strategy = LookupIpStrategy::Ipv4AndIpv6;
            Arc::new(builder.build())
        })
    });

fn configured_dns_resolvers() -> Vec<DnsResolver> {
    DNS_RESOLVERS.read().unwrap().clone()
}

pub enum ForwardResolverConfig {
    System,
    Hickory(ResolverConfig),
}

fn forward_resolver_configs_from(resolvers: Vec<DnsResolver>) -> Vec<ForwardResolverConfig> {
    resolvers
        .into_iter()
        .map(|resolver| match resolver {
            DnsResolver::System => ForwardResolverConfig::System,
            DnsResolver::Hickory { name_servers, .. } => ForwardResolverConfig::Hickory(
                ResolverConfig::from_parts(None, Vec::new(), name_servers),
            ),
        })
        .collect()
}

pub fn get_forward_resolver_configs() -> Vec<ForwardResolverConfig> {
    forward_resolver_configs_from(configured_dns_resolvers())
}

pub async fn resolve_txt_record(domain_name: &str) -> Result<String, Error> {
    let mut last_err = None;
    for resolver in configured_dns_resolvers() {
        let response = match resolver {
            DnsResolver::System => {
                let Some(resolver) = SYSTEM_RECORD_RESOLVER.as_ref().cloned() else {
                    last_err = Some(anyhow::anyhow!("system dns resolver is unavailable").into());
                    continue;
                };
                resolver.txt_lookup(domain_name).await
            }
            DnsResolver::Hickory { uri, resolver, .. } => {
                let response = resolver.txt_lookup(domain_name).await;
                if response.is_err() {
                    tracing::debug!(?uri, ?domain_name, "txt lookup failed with resolver");
                }
                response
            }
        };

        let Ok(response) = response else {
            last_err =
                Some(anyhow::anyhow!("txt_lookup failed, domain_name: {}", domain_name).into());
            continue;
        };

        let Some(txt_record) = response.iter().next() else {
            last_err =
                Some(anyhow::anyhow!("no txt record found, domain_name: {}", domain_name).into());
            continue;
        };

        let txt_data = String::from_utf8_lossy(&txt_record.txt_data()[0]);
        tracing::info!(?txt_data, ?domain_name, "get txt record");

        return Ok(txt_data.to_string());
    }

    Err(last_err.unwrap_or(Error::NotFound))
}

pub async fn resolve_srv_records(domain_name: &str) -> Result<Vec<SRV>, Error> {
    let mut last_err = None;
    for resolver in configured_dns_resolvers() {
        let response = match resolver {
            DnsResolver::System => {
                let Some(resolver) = SYSTEM_RECORD_RESOLVER.as_ref().cloned() else {
                    last_err = Some(anyhow::anyhow!("system dns resolver is unavailable").into());
                    continue;
                };
                resolver.srv_lookup(domain_name).await
            }
            DnsResolver::Hickory { uri, resolver, .. } => {
                let response = resolver.srv_lookup(domain_name).await;
                if response.is_err() {
                    tracing::debug!(?uri, ?domain_name, "srv lookup failed with resolver");
                }
                response
            }
        };

        let Ok(response) = response else {
            last_err =
                Some(anyhow::anyhow!("srv_lookup failed, domain_name: {}", domain_name).into());
            continue;
        };

        let records = response.iter().cloned().collect::<Vec<_>>();
        if records.is_empty() {
            last_err =
                Some(anyhow::anyhow!("no srv record found, domain_name: {}", domain_name).into());
            continue;
        }

        return Ok(records);
    }

    Err(last_err.unwrap_or(Error::NotFound))
}

pub async fn socket_addrs(
    url: &url::Url,
    default_port_number: impl Fn() -> Option<u16>,
) -> Result<Vec<SocketAddr>, Error> {
    let host = url.host().ok_or(Error::InvalidUrl(url.to_string()))?;
    let port = url
        .port()
        .or_else(default_port_number)
        .ok_or(Error::InvalidUrl(url.to_string()))?;

    match host {
        url::Host::Ipv4(ip) => return Ok(vec![SocketAddr::new(IpAddr::V4(ip), port)]),
        url::Host::Ipv6(ip) => return Ok(vec![SocketAddr::new(IpAddr::V6(ip), port)]),
        _ => {}
    }

    let host = host.to_string();
    resolve_host(&host, port).await
}

pub async fn resolve_host(host: &str, port: u16) -> Result<Vec<SocketAddr>, Error> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }

    let mut last_err = None;
    for resolver in configured_dns_resolvers() {
        match resolver {
            DnsResolver::System => {
                let socket_addr = format!("{}:{}", host, port);
                match lookup_host(socket_addr).await {
                    Ok(addrs) => {
                        let addrs = addrs.collect::<Vec<_>>();
                        if !addrs.is_empty() {
                            tracing::debug!(?addrs, "system dns lookup done");
                            return Ok(addrs);
                        }
                    }
                    Err(e) => {
                        tracing::debug!(?e, "system dns lookup failed");
                        last_err = Some(Error::from(e));
                    }
                }
            }
            DnsResolver::Hickory { uri, resolver, .. } => match resolver.lookup_ip(host).await {
                Ok(lookup) => {
                    let addrs = lookup
                        .iter()
                        .map(|ip| SocketAddr::new(ip, port))
                        .collect::<Vec<_>>();
                    if !addrs.is_empty() {
                        return Ok(addrs);
                    }
                }
                Err(e) => {
                    tracing::debug!(?uri, ?host, ?e, "hickory dns lookup failed");
                    last_err = Some(
                        anyhow::anyhow!(
                            "hickory dns lookup_ip failed, host: {}, port: {}",
                            host,
                            port
                        )
                        .into(),
                    );
                }
            },
        }
    }

    Err(last_err.unwrap_or(Error::NotFound))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_dns_resolver_is_system() {
        assert_eq!(get_default_dns_resolvers(), vec!["system".to_string()]);
        assert!(matches!(
            build_dns_resolvers(&get_default_dns_resolvers()).unwrap()[0],
            DnsResolver::System
        ));
    }

    #[test]
    fn alidns_doh_resolver_is_supported() {
        validate_dns_resolvers(&["https://dns.alidns.com/dns-query".to_string()]).unwrap();
    }

    #[test]
    fn forward_resolver_configs_preserve_order() {
        let resolvers = build_dns_resolvers(&[
            "system".to_string(),
            "https://dns.alidns.com/dns-query".to_string(),
        ])
        .unwrap();

        let forward_configs = forward_resolver_configs_from(resolvers);
        assert!(matches!(forward_configs[0], ForwardResolverConfig::System));
        assert!(matches!(
            forward_configs[1],
            ForwardResolverConfig::Hickory(_)
        ));
    }

    #[test]
    fn unknown_doh_resolver_requires_bootstrap_ip() {
        assert!(validate_dns_resolvers(&["https://example.com/dns-query".to_string()]).is_err());
    }

    #[tokio::test]
    async fn test_socket_addrs() {
        let url = url::Url::parse("tcp://github-ci-test.easytier.cn:80").unwrap();
        let addrs = socket_addrs(&url, || Some(80)).await.unwrap();
        assert_eq!(2, addrs.len(), "addrs: {:?}", addrs);
        println!("addrs: {:?}", addrs);
    }

    #[tokio::test]
    async fn socket_addrs_preserves_explicit_zero_port() {
        let cases = [
            ("ws://127.0.0.1:0", 80, 0),
            ("wss://127.0.0.1:0", 443, 0),
            ("ws://127.0.0.1", 80, 80),
            ("wss://127.0.0.1", 443, 443),
        ];

        for (raw_url, default_port, expected_port) in cases {
            let url = url::Url::parse(raw_url).unwrap();
            let addrs = socket_addrs(&url, || Some(default_port)).await.unwrap();
            assert_eq!(
                addrs,
                vec![SocketAddr::from(([127, 0, 0, 1], expected_port))]
            );
        }
    }
}
