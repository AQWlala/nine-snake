use anyhow::{anyhow, Result};
use std::net::{IpAddr, Ipv4Addr};

#[derive(Debug, Clone)]
pub struct SsrfGuard {
    allow_private: bool,
}

impl SsrfGuard {
    pub fn new() -> Self {
        Self {
            allow_private: false,
        }
    }

    pub fn with_allow_private(mut self, allow: bool) -> Self {
        self.allow_private = allow;
        self
    }

    pub fn validate_url(&self, url: &str) -> Result<()> {
        let parsed = url::Url::parse(url).map_err(|e| anyhow!("invalid URL: {e}"))?;
        let host = parsed
            .host_str()
            .ok_or_else(|| anyhow!("URL has no host: {url}"))?;

        if self.allow_private {
            return Ok(());
        }

        let ip: IpAddr = match host.parse() {
            Ok(ip) => ip,
            Err(_) => {
                use std::net::ToSocketAddrs;
                let addrs: Vec<IpAddr> = format!("{host}:0")
                    .to_socket_addrs()
                    .map(|iter| iter.map(|a| a.ip()).collect())
                    .unwrap_or_default();
                match addrs.first() {
                    Some(ip) => *ip,
                    None => return Err(anyhow!("cannot resolve host: {host}")),
                }
            }
        };

        self.validate_ip(&ip)
    }

    fn validate_ip(&self, ip: &IpAddr) -> Result<()> {
        match ip {
            IpAddr::V4(v4) => {
                if is_loopback_v4(v4) {
                    return Err(anyhow!("SSRF: loopback address {} is not allowed", v4));
                }
                if is_private_v4(v4) {
                    return Err(anyhow!("SSRF: private address {} is not allowed", v4));
                }
                if is_link_local_v4(v4) {
                    return Err(anyhow!("SSRF: link-local address {} is not allowed", v4));
                }
                if is_cgnat(v4) {
                    return Err(anyhow!("SSRF: CGNAT address {} is not allowed", v4));
                }
                if is_broadcast(v4) {
                    return Err(anyhow!("SSRF: broadcast address {} is not allowed", v4));
                }
            }
            IpAddr::V6(v6) => {
                if v6.is_loopback() {
                    return Err(anyhow!("SSRF: loopback address {} is not allowed", v6));
                }
            }
        }
        Ok(())
    }

    pub fn validate_redirect_chain(&self, urls: &[String]) -> Result<()> {
        for url in urls {
            self.validate_url(url)?;
        }
        Ok(())
    }
}

fn is_loopback_v4(ip: &Ipv4Addr) -> bool {
    ip.octets()[0] == 127
}

fn is_private_v4(ip: &Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 10
        || (octets[0] == 172 && octets[1] >= 16 && octets[1] <= 31)
        || (octets[0] == 192 && octets[1] == 168)
}

fn is_link_local_v4(ip: &Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 169 && octets[1] == 254
}

pub fn is_cgnat(ip: &Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 100 && octets[1] >= 64 && octets[1] <= 127
}

fn is_broadcast(ip: &Ipv4Addr) -> bool {
    *ip == Ipv4Addr::BROADCAST
}

impl Default for SsrfGuard {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_private_192_168() {
        let guard = SsrfGuard::new();
        assert!(guard.validate_url("http://192.168.1.1/api").is_err());
    }

    #[test]
    fn rejects_private_10() {
        let guard = SsrfGuard::new();
        assert!(guard.validate_url("http://10.0.0.1/api").is_err());
    }

    #[test]
    fn rejects_loopback() {
        let guard = SsrfGuard::new();
        assert!(guard.validate_url("http://127.0.0.1/api").is_err());
    }

    #[test]
    fn rejects_link_local_cloud_metadata() {
        let guard = SsrfGuard::new();
        assert!(guard
            .validate_url("http://169.254.169.254/latest/meta-data/")
            .is_err());
    }

    #[test]
    fn rejects_cgnat() {
        let guard = SsrfGuard::new();
        assert!(guard.validate_url("http://100.64.0.1/api").is_err());
    }

    #[test]
    fn allows_public_ip() {
        let guard = SsrfGuard::new();
        assert!(guard.validate_url("https://api.openai.com").is_ok());
    }

    #[test]
    fn allow_private_opt_out() {
        let guard = SsrfGuard::new().with_allow_private(true);
        assert!(guard.validate_url("http://192.168.1.1/api").is_ok());
    }

    #[test]
    fn rejects_broadcast() {
        let guard = SsrfGuard::new();
        assert!(guard.validate_url("http://255.255.255.255/api").is_err());
    }
}
