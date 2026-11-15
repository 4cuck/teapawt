//! SOCKS5 exit pool for X API calls.
//!
//! Each cookie session is pinned to one `host:port` so its apparent IP stays
//! stable across restarts. Existing pins are read-only; other sessions use a
//! deterministic port from the configured list, including after a live import.

use std::{collections::BTreeMap, net::IpAddr, path::Path, time::Duration};

use tokio::{
   fs,
   net::{TcpStream, lookup_host},
   process::Command,
   task::JoinSet,
   time::{Instant, timeout},
};

use super::http::{ProxyConfig, ProxyKind};
use crate::{
   config::Config,
   error::{Error, Result},
};

struct Endpoint {
   port: u16,
   username: String,
   password: String,
}

/// Resolved SOCKS5 endpoints plus the session→port map.
pub struct ProxyPool {
   connect_host: String,
   endpoints: Vec<Endpoint>,
   fallback_ports: Vec<u16>,
   pins: BTreeMap<i64, u16>,
}

impl ProxyPool {
   /// Load `host:port:user:pass` lines, pick the lowest-latency A record, and
   /// restore pins. Returns [`None`] when the file is unset or empty.
   pub async fn load(config: &Config) -> Result<Option<Self>> {
      let path = config.config.socks_proxies_file.trim();
      if path.is_empty() {
         return Ok(None);
      }
      let content = fs::read_to_string(path).await.map_err(|err| {
         Error::InvalidConfig(format!("socksProxiesFile {path:?} unreadable: {err}"))
      })?;
      let mut endpoints = Vec::new();
      let mut hostname = None;
      for (idx, line) in content.lines().enumerate() {
         let line = line.trim();
         if line.is_empty() || line.starts_with('#') {
            continue;
         }
         let (host, port, username, password) = parse_proxy_line(line).ok_or_else(|| {
            Error::InvalidConfig(format!(
               "socksProxiesFile line {}: expected host:port:user:pass",
               idx + 1
            ))
         })?;
         match hostname {
            None => hostname = Some(host),
            Some(ref existing) if existing == &host => {},
            Some(ref existing) => {
               return Err(Error::InvalidConfig(format!(
                  "socksProxiesFile mixes hosts {existing} and {host}; pin one hostname"
               )));
            },
         }
         endpoints.push(Endpoint {
            port,
            username,
            password,
         });
      }
      if endpoints.is_empty() {
         return Ok(None);
      }
      let hostname = hostname.unwrap_or_default();
      let probe_port = endpoints[0].port;
      let connect_host = pick_fastest_host(&hostname, probe_port).await;
      tracing::info!(
         host = %hostname,
         connect = %connect_host,
         endpoints = endpoints.len(),
         "SOCKS5 pool ready"
      );

      let pin_path = session_proxies_path(config);
      let pins = load_pins(&pin_path).await;
      Ok(Some(Self::with_pins(connect_host, endpoints, pins)))
   }

   fn with_pins(connect_host: String, endpoints: Vec<Endpoint>, mut pins: BTreeMap<i64, u16>) -> Self {
      // Canonical fallback order, without changing the configured order used
      // by first() and all() for unscoped clients.
      let mut fallback_ports = endpoints.iter().map(|endpoint| endpoint.port).collect::<Vec<_>>();
      fallback_ports.sort_unstable();
      fallback_ports.dedup();
      pins.retain(|_, port| fallback_ports.binary_search(port).is_ok());
      Self { connect_host, endpoints, fallback_ports, pins }
   }

   /// SOCKS5 config for this session, always the same port once pinned.
   #[must_use]
   pub fn for_session(&self, session_id: i64) -> ProxyConfig {
      self.at_port(self.port_for(session_id))
   }

   /// The port this session is pinned to.
   #[must_use]
   pub fn port_for(&self, session_id: i64) -> u16 {
      // This binding is defined for every id before any slot can become
      // visible, including ids added while the server is running.
      self.pins.get(&session_id).copied().unwrap_or_else(|| {
         let idx = (session_id.unsigned_abs() % self.fallback_ports.len() as u64) as usize;
         self.fallback_ports[idx]
      })
   }

   /// First endpoint, used for TID bootstrap and other unscoped API fetches.
   #[must_use]
   pub fn first(&self) -> ProxyConfig {
      self.at_port(self.endpoints[0].port)
   }

   /// Every configured exit. The HTTP layer uses this list to move traffic
   /// away from a port that X temporarily rate-limited at the edge.
   #[must_use]
   pub fn all(&self) -> Vec<ProxyConfig> {
      self
         .endpoints
         .iter()
         .map(|endpoint| self.at_port(endpoint.port))
         .collect()
   }

   fn at_port(&self, port: u16) -> ProxyConfig {
      let endpoint = self
         .endpoints
         .iter()
         .find(|endpoint| endpoint.port == port)
         .unwrap_or(&self.endpoints[0]);
      ProxyConfig {
         host: self.connect_host.clone(),
         port: endpoint.port,
         kind: ProxyKind::Socks5,
         username: Some(endpoint.username.clone()),
         password: Some(endpoint.password.clone()),
      }
   }
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn live_and_restart_pins_preserve_legacy_ports_and_ignore_file_order() {
      fn pool(ports: [u16; 3]) -> ProxyPool {
         let endpoints = ports.into_iter().map(|port| Endpoint {
            port,
            username: String::new(),
            password: String::new(),
         }).collect();
         ProxyPool::with_pins("localhost".into(), endpoints, BTreeMap::from([(1, 1000), (2, 9999)]))
      }
      let live = pool([3000, 1000, 2000]);
      let restart = pool([2000, 3000, 1000]);
      assert_eq!(live.port_for(1), 1000);
      assert_eq!(live.port_for(2), 3000); // Invalid legacy pin falls back.
      for id in [1, 2, 100, 101, 102, i64::MIN, i64::MAX] {
         assert_eq!(live.port_for(id), restart.port_for(id));
      }
      assert_eq!(live.first().port, 3000);
   }
}

/// A gateway slower than this is not used. The proxy hostname has several
/// addresses and a slow one adds the wait the fallback is meant to avoid.
const GATEWAY_MAX_MS: f64 = 20.0;

/// Load the fallback proxy list and point every exit at a gateway that
/// answered within [`GATEWAY_MAX_MS`]. Each line's scheme picks the tunnel:
/// `socks5h://` is SOCKS5, `http://` and bare `host:port:user:pass` lines are
/// HTTP CONNECT. With no gateway that fast the fallback stays off rather than
/// keeping teapot from starting.
pub async fn load_fallback_proxies(path: &str) -> Result<Vec<ProxyConfig>> {
   let path = path.trim();
   if path.is_empty() {
      return Ok(Vec::new());
   }
   let content = fs::read_to_string(path).await.map_err(|err| {
      Error::InvalidConfig(format!("fallbackProxiesFile {path:?} unreadable: {err}"))
   })?;
   let mut parsed = Vec::new();
   for (idx, line) in content.lines().enumerate() {
      let line = line.trim();
      if line.is_empty() || line.starts_with('#') {
         continue;
      }
      let Some(entry) = parse_fallback_line(line) else {
         return Err(Error::InvalidConfig(format!(
            "fallbackProxiesFile line {}: expected scheme://user:pass@host:port or host:port:user:pass",
            idx + 1
         )));
      };
      parsed.push(entry);
   }
   if parsed.is_empty() {
      return Ok(Vec::new());
   }
   let hostname = parsed[0].host.clone();
   if parsed.iter().any(|entry| entry.host != hostname) {
      return Err(Error::InvalidConfig(
         "fallbackProxiesFile mixes proxy hosts; use one gateway hostname".into(),
      ));
   }
   let Some(gateway) = pick_gateway(&hostname, parsed[0].port).await else {
      tracing::warn!(
         host = %hostname,
         "no fallback proxy gateway answered within {GATEWAY_MAX_MS:.0} ms; running without the fallback"
      );
      return Ok(Vec::new());
   };
   let proxies = parsed
      .into_iter()
      .map(|entry| ProxyConfig {
         host: gateway.clone(),
         ..entry
      })
      .collect::<Vec<_>>();
   tracing::info!(
      host = %hostname,
      gateway = %gateway,
      proxies = proxies.len(),
      "fallback proxy pool ready"
   );
   Ok(proxies)
}

fn parse_fallback_line(line: &str) -> Option<ProxyConfig> {
   let schemes = [
      ("socks5h://", ProxyKind::Socks5),
      ("socks5://", ProxyKind::Socks5),
      ("http://", ProxyKind::Http),
   ];
   let (kind, (host, port, username, password)) = match schemes
      .iter()
      .find_map(|(scheme, kind)| Some((*kind, line.strip_prefix(scheme)?)))
   {
      Some((kind, rest)) => {
         let (userinfo, hostport) = rest.rsplit_once('@')?;
         let (username, password) = userinfo.split_once(':')?;
         let (host, port) = hostport.rsplit_once(':')?;
         let port: u16 = port.parse().ok()?;
         if host.is_empty() || username.is_empty() || password.is_empty() || port == 0 {
            return None;
         }
         (
            kind,
            (
               host.to_owned(),
               port,
               username.to_owned(),
               password.to_owned(),
            ),
         )
      },
      None => (ProxyKind::Http, parse_proxy_line(line)?),
   };
   Some(ProxyConfig {
      host,
      port,
      kind,
      username: Some(username),
      password: Some(password),
   })
}

/// The gateway address to use: `hostname` itself when it is an IP, or the
/// fastest of its A records, as long as it answers within [`GATEWAY_MAX_MS`].
async fn pick_gateway(hostname: &str, probe_port: u16) -> Option<String> {
   let ips: Vec<IpAddr> = if let Ok(ip) = hostname.parse::<IpAddr>() {
      vec![ip]
   } else {
      let lookup = format!("{hostname}:{probe_port}");
      let mut ips: Vec<IpAddr> = match lookup_host(&lookup).await {
         Ok(addrs) => addrs.map(|addr| addr.ip()).collect(),
         Err(err) => {
            tracing::warn!("fallback proxy DNS for {hostname} failed: {err}");
            return None;
         },
      };
      ips.sort();
      ips.dedup();
      ips
   };
   let mut probes = JoinSet::new();
   for ip in ips {
      probes.spawn(async move { (ip, probe_ip(ip, probe_port).await) });
   }
   let mut best: Option<(IpAddr, f64)> = None;
   while let Some(joined) = probes.join_next().await {
      let Ok((ip, rtt)) = joined else {
         continue;
      };
      tracing::info!(%ip, rtt_ms = ?rtt, "fallback proxy gateway probe");
      let Some(ms) = rtt.filter(|ms| *ms <= GATEWAY_MAX_MS) else {
         continue;
      };
      if best.is_none_or(|(_, best_ms)| ms < best_ms) {
         best = Some((ip, ms));
      }
   }
   let (ip, ms) = best?;
   tracing::info!(%ip, rtt_ms = ms, "fallback proxy using gateway");
   Some(ip.to_string())
}

fn session_proxies_path(config: &Config) -> String {
   let path = config.config.session_proxies_file.trim();
   if path.is_empty() {
      "session-proxies.json".to_owned()
   } else {
      path.to_owned()
   }
}

fn parse_proxy_line(line: &str) -> Option<(String, u16, String, String)> {
   let mut parts = line.splitn(4, ':');
   let host = parts.next()?.to_owned();
   let port = parts.next()?.parse().ok()?;
   let username = parts.next()?.to_owned();
   let password = parts.next()?.to_owned();
   if host.is_empty() || username.is_empty() || password.is_empty() || port == 0 {
      return None;
   }
   Some((host, port, username, password))
}

async fn load_pins(path: &str) -> BTreeMap<i64, u16> {
   if path.is_empty() || !Path::new(path).exists() {
      return BTreeMap::new();
   }
   match fs::read_to_string(path).await {
      Ok(content) => serde_json::from_str(&content).unwrap_or_else(|err| {
         tracing::warn!("Ignoring unreadable session proxy pins: {err}");
         BTreeMap::new()
      }),
      Err(_) => BTreeMap::new(),
   }
}


/// Resolve `hostname` and pick the A record with the lowest ping.
async fn pick_fastest_host(hostname: &str, probe_port: u16) -> String {
   let lookup = format!("{hostname}:{probe_port}");
   let addrs = match lookup_host(&lookup).await {
      Ok(addrs) => addrs,
      Err(err) => {
         tracing::warn!("SOCKS5 DNS for {hostname} failed ({err}); using hostname");
         return hostname.to_owned();
      },
   };
   let mut ips: Vec<IpAddr> = addrs.map(|addr| addr.ip()).collect();
   ips.sort();
   ips.dedup();
   if ips.is_empty() {
      return hostname.to_owned();
   }

   // Probed together: run one after another, six addresses held the listener
   // back for twelve seconds on every restart while requests piled up.
   let mut probes = JoinSet::new();
   for ip in ips {
      probes.spawn(async move { (ip, probe_ip(ip, probe_port).await) });
   }
   let mut best: Option<(IpAddr, f64)> = None;
   while let Some(joined) = probes.join_next().await {
      let Ok((ip, rtt)) = joined else {
         continue;
      };
      tracing::info!(%ip, rtt_ms = ?rtt, "SOCKS5 probe");
      let Some(ms) = rtt else {
         continue;
      };
      match best {
         Some((_, best_ms)) if ms >= best_ms => {},
         _ => best = Some((ip, ms)),
      }
   }
   match best {
      Some((ip, ms)) => {
         tracing::info!(%ip, rtt_ms = ms, "SOCKS5 using lowest-ping address");
         ip.to_string()
      },
      None => hostname.to_owned(),
   }
}

async fn probe_ip(ip: IpAddr, port: u16) -> Option<f64> {
   if let Some(ms) = icmp_rtt_ms(ip).await {
      return Some(ms);
   }
   tcp_rtt_ms(ip, port).await
}

async fn icmp_rtt_ms(ip: IpAddr) -> Option<f64> {
   let output = timeout(
      Duration::from_secs(6),
      Command::new("ping")
         .args(["-c", "3", "-W", "1", "-n", "-q", &ip.to_string()])
         .output(),
   )
   .await
   .ok()?
   .ok()?;
   if !output.status.success() {
      return None;
   }
   let stdout = String::from_utf8_lossy(&output.stdout);
   for line in stdout.lines() {
      if !line.contains("min/avg/max") {
         continue;
      }
      let stats = line.split('=').nth(1)?;
      let avg = stats.split('/').nth(1)?.trim();
      return avg.parse().ok();
   }
   None
}

async fn tcp_rtt_ms(ip: IpAddr, port: u16) -> Option<f64> {
   let start = Instant::now();
   timeout(Duration::from_secs(2), TcpStream::connect((ip, port)))
      .await
      .ok()?
      .ok()?;
   Some(start.elapsed().as_secs_f64() * 1000.0)
}
