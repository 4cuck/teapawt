//! The browser each account pretends to be.
//!
//! Every cookie session presents one real browser's TLS, HTTP/2, and HTTP/3
//! (QUIC) fingerprint and the headers to match. The engine is drawn from the
//! session id, so an account keeps the same browser across restarts the way it
//! keeps its proxy port, and the mix is weighted like real desktop traffic
//! rather than uniform. Within an engine the newest version X still accepts is
//! used; that is found by a handshake at startup, since a profile can advertise
//! something X's edge rejects.

use std::{
   collections::HashMap,
   fmt,
   sync::OnceLock,
   time::Duration,
};

use koon_core::{
   BrowserProfile,
   Chrome,
   Client,
   Edge,
   Firefox,
   IpVersion,
   Opera,
   Os,
   Safari,
};
use tokio::task::JoinSet;

use super::http::{
   ProxyConfig,
   ProxyKind,
};

/// Browser families a session can present as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Engine {
   Chrome,
   Edge,
   Firefox,
   Safari,
   Opera,
}

/// Specific version of an [`Engine`] supported by `koon-core`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProfileVersion {
   Chrome(u32),
   Edge(u32),
   Firefox(u32),
   Safari(&'static str),
   Opera(u32),
}

impl ProfileVersion {
   fn build(self, os: Os) -> BrowserProfile {
      match self {
         Self::Chrome(major) => Chrome::version(major, os).unwrap_or_else(|_| Chrome::latest()),
         Self::Edge(major) => Edge::version(major, os).unwrap_or_else(|_| Edge::latest()),
         Self::Firefox(major) => Firefox::version(major, os).unwrap_or_else(|_| Firefox::latest()),
         Self::Safari(version) => Safari::version(version, os).unwrap_or_else(|_| Safari::latest()),
         Self::Opera(major) => Opera::version(major, os).unwrap_or_else(|_| Opera::latest()),
      }
   }
}

impl fmt::Display for ProfileVersion {
   fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
      match *self {
         Self::Chrome(major) => write!(f, "Chrome{major}"),
         Self::Edge(major) => write!(f, "Edge{major}"),
         Self::Firefox(major) => write!(f, "Firefox{major}"),
         Self::Safari(version) => write!(f, "Safari{version}"),
         Self::Opera(major) => write!(f, "Opera{major}"),
      }
   }
}

impl Engine {
   const ALL: [Self; 5] = [
      Self::Chrome,
      Self::Edge,
      Self::Firefox,
      Self::Safari,
      Self::Opera,
   ];

   /// Profiles newest first. Only the newest that X accepts is ever used.
   const fn candidates(self) -> &'static [ProfileVersion] {
      match self {
         Self::Chrome => {
            &[
               ProfileVersion::Chrome(158),
               ProfileVersion::Chrome(157),
               ProfileVersion::Chrome(156),
               ProfileVersion::Chrome(155),
               ProfileVersion::Chrome(154),
               ProfileVersion::Chrome(153),
               ProfileVersion::Chrome(152),
            ]
         },
         Self::Edge => {
            &[
               ProfileVersion::Edge(157),
               ProfileVersion::Edge(156),
               ProfileVersion::Edge(155),
               ProfileVersion::Edge(154),
               ProfileVersion::Edge(153),
               ProfileVersion::Edge(152),
            ]
         },
         Self::Firefox => {
            &[
               ProfileVersion::Firefox(159),
               ProfileVersion::Firefox(158),
               ProfileVersion::Firefox(157),
               ProfileVersion::Firefox(156),
               ProfileVersion::Firefox(155),
            ]
         },
         Self::Safari => {
            &[
               ProfileVersion::Safari("27.0"),
               ProfileVersion::Safari("26.6"),
               ProfileVersion::Safari("26.5"),
               ProfileVersion::Safari("26.4"),
               ProfileVersion::Safari("26.3"),
               ProfileVersion::Safari("26.0"),
               ProfileVersion::Safari("18.5"),
            ]
         },
         Self::Opera => {
            &[
               ProfileVersion::Opera(139),
               ProfileVersion::Opera(138),
               ProfileVersion::Opera(137),
               ProfileVersion::Opera(136),
            ]
         },
      }
   }
}

impl fmt::Display for Engine {
   fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
      f.write_str(match *self {
         Self::Chrome => "chrome",
         Self::Edge => "edge",
         Self::Firefox => "firefox",
         Self::Safari => "safari",
         Self::Opera => "opera",
      })
   }
}

/// One session's browser: an engine and an operating system it really ships
/// on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
   pub engine: Engine,
   pub os:     Os,
}

impl Identity {
   /// The browser profile (TLS, HTTP/2, HTTP/3, and headers) for this identity:
   /// this engine's newest accepted version on its OS.
   #[must_use]
   pub fn profile(self) -> BrowserProfile {
      accepted_profile(self.engine).build(self.os)
   }
}

impl fmt::Display for Identity {
   fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
      write!(f, "{} on {:?}", self.engine, self.os)
   }
}

/// Approximate desktop browser share, in tenths of a percent.
const ENGINE_WEIGHTS: [(Engine, u64); 5] = [
   (Engine::Chrome, 650),
   (Engine::Edge, 130),
   (Engine::Safari, 90),
   (Engine::Firefox, 80),
   (Engine::Opera, 50),
];

/// Operating systems each engine is seen on, weighted. Safari stays on macOS
/// and Edge on Windows; a Safari-on-Windows user agent does not exist.
const fn os_weights(engine: Engine) -> &'static [(Os, u64)] {
   match engine {
      Engine::Chrome => &[(Os::Windows, 75), (Os::MacOS, 20), (Os::Linux, 5)],
      Engine::Edge => &[(Os::Windows, 90), (Os::MacOS, 10)],
      Engine::Firefox => &[(Os::Windows, 70), (Os::Linux, 20), (Os::MacOS, 10)],
      Engine::Safari => &[(Os::MacOS, 100)],
      Engine::Opera => &[(Os::Windows, 100)],
   }
}

/// The browser a session presents as, derived from its id alone so it is the
/// same on every start.
#[must_use]
pub fn identity_for(session_id: i64) -> Identity {
   let mut seed = splitmix64(session_id as u64 ^ 0x7465_6170_6177_7421);
   let engine = pick(&ENGINE_WEIGHTS, &mut seed);
   let os = pick(os_weights(engine), &mut seed);
   Identity { engine, os }
}

/// `navigator.platform` for an OS, as a real browser reports it. The JSD
/// payload carries this string, so it must match the session's headers.
#[must_use]
pub fn navigator_platform(os: Os) -> &'static str {
   match os {
      Os::MacOS => "MacIntel",
      Os::Linux => "Linux x86_64",
      // Windows (and anything else a desktop session presents as).
      _ => "Win32",
   }
}

/// `sec-ch-ua-platform` for an OS (the quoted low-entropy hint), a fallback for
/// when a profile does not carry the header.
#[must_use]
pub fn sec_ch_ua_platform(os: Os) -> String {
   match os {
      Os::MacOS => "\"macOS\"".to_owned(),
      Os::Linux => "\"Linux\"".to_owned(),
      _ => "\"Windows\"".to_owned(),
   }
}

/// A header value from a session's browser profile (`user-agent`, `sec-ch-ua`,
/// `sec-ch-ua-platform`), matched case-insensitively. The JSD solver reads
/// these so the token it mints names the same browser the requests present as.
#[must_use]
pub fn profile_header(session_id: i64, name: &str) -> Option<String> {
   identity_for(session_id)
      .profile()
      .headers
      .iter()
      .find(|(key, _)| key.eq_ignore_ascii_case(name))
      .map(|(_, value)| value.clone())
}

fn pick<T: Copy>(weights: &[(T, u64)], seed: &mut u64) -> T {
   let total: u64 = weights.iter().map(|&(_, weight)| weight).sum();
   *seed = splitmix64(*seed);
   let mut roll = *seed % total.max(1);
   for &(item, weight) in weights {
      if roll < weight {
         return item;
      }
      roll -= weight;
   }
   weights[0].0
}

/// Deterministic 64-bit mixer, so identities do not depend on an RNG.
const fn splitmix64(mut state: u64) -> u64 {
   state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
   let mut result = state;
   result = (result ^ (result >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
   result = (result ^ (result >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
   result ^ (result >> 31)
}

/// Newest version per engine that completed a handshake with X.
static ACCEPTED: OnceLock<HashMap<Engine, ProfileVersion>> = OnceLock::new();

/// Newest known-accepted fallback when the probe has not run or an engine
/// could not be tested: the second newest candidate, since the newest is the
/// one most likely to run ahead of what X's edge understands.
const fn fallback_profile(engine: Engine) -> ProfileVersion {
   let candidates = engine.candidates();
   if candidates.len() > 1 {
      candidates[1]
   } else {
      candidates[0]
   }
}

fn accepted_profile(engine: Engine) -> ProfileVersion {
   ACCEPTED
      .get()
      .and_then(|accepted| accepted.get(&engine).copied())
      .unwrap_or_else(|| fallback_profile(engine))
}

/// How long one probe handshake may take before the version is skipped.
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// Handshakes a version must pass, every one of them, across the operating
/// systems its sessions run on.
const PROBE_HANDSHAKES: usize = 3;

/// Find, per engine, the newest profile X accepts, by handshaking with
/// `x.com` through `proxy` (or directly). Runs once at startup; a second call
/// is a no-op.
pub async fn probe_accepted(proxy: Option<ProxyConfig>) {
   if ACCEPTED.get().is_some() {
      return;
   }
   let mut probes = JoinSet::new();
   for engine in Engine::ALL {
      let proxy = proxy.clone();
      probes.spawn(async move { (engine, newest_accepted(engine, proxy.as_ref()).await) });
   }
   let mut accepted = HashMap::new();
   while let Some(joined) = probes.join_next().await {
      let Ok((engine, profile)) = joined else {
         continue;
      };
      match profile {
         Some((version, protocol)) => {
            accepted.insert(engine, version);
            tracing::debug!(%engine, %version, %protocol, "X accepted browser profile");
         },
         None => {
            tracing::warn!(%engine, "no browser profile completed a handshake with X, using fallback");
         },
      }
   }
   let summary = Engine::ALL
      .iter()
      .map(|engine| {
         format!(
            "{engine}={}",
            accepted
               .get(engine)
               .copied()
               .unwrap_or_else(|| fallback_profile(*engine))
         )
      })
      .collect::<Vec<_>>()
      .join(" ");
   tracing::info!("browser profiles accepted by X: {summary}");
   let _ = ACCEPTED.set(accepted);
}

async fn newest_accepted(
   engine: Engine,
   proxy: Option<&ProxyConfig>,
) -> Option<(ProfileVersion, String)> {
   'versions: for &version in engine.candidates() {
      let oses = os_weights(engine);
      // Sent together. One after another they were a second of every start,
      // and the server takes no request until they are done.
      let mut rounds = JoinSet::new();
      for round in 0..PROBE_HANDSHAKES.max(oses.len()) {
         let os = oses[round % oses.len()].0;
         let proxy = proxy.cloned();
         rounds.spawn(async move { handshakes(version, os, proxy.as_ref()).await });
      }
      let mut protocol = String::new();
      while let Some(passed) = rounds.join_next().await {
         let Ok(Some(ver)) = passed else {
            tracing::debug!(%engine, %version, "X refused this browser profile");
            continue 'versions;
         };
         if protocol.is_empty() || ver == "h3" {
            protocol = ver;
         }
      }
      return Some((version, protocol));
   }
   None
}

/// Whether a fresh client built from `version` on `os` gets any HTTP answer
/// from `x.com`. Seeds `Alt-Svc` for `x.com:443` so QUIC/HTTP/3 is exercised
/// alongside TCP fallback.
async fn handshakes(
   version: ProfileVersion,
   os: Os,
   proxy: Option<&ProxyConfig>,
) -> Option<String> {
   let mut builder = Client::builder(version.build(os))
      .follow_redirects(false)
      .cookie_jar(false)
      .timeout(PROBE_TIMEOUT)
      .alt_svc_h3("x.com", 443, 443);
   if let Some(proxy) = proxy {
      builder = builder.proxy(&proxy.url()).ok()?;
      if proxy.kind == ProxyKind::Socks5 {
         builder = builder.ip_version(IpVersion::V4);
      }
   }
   let client = builder.build().ok()?;
   let response = client.get("https://x.com/").await.ok()?;
   let protocol = response.version;
   client.shutdown().await;
   Some(protocol)
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn identity_is_stable_per_session() {
      for id in [1_i64, 2, 57, 225, 9_999] {
         assert_eq!(identity_for(id), identity_for(id));
      }
   }

   #[test]
   fn os_always_matches_engine() {
      for id in 0..2_000_i64 {
         let identity = identity_for(id);
         let allowed = os_weights(identity.engine);
         assert!(
            allowed.iter().any(|&(os, _)| os == identity.os),
            "{identity} is not a browser that exists"
         );
      }
   }

   #[test]
   fn mix_is_weighted_towards_chrome() {
      let mut counts: HashMap<Engine, usize> = HashMap::new();
      for id in 0..5_000_i64 {
         *counts.entry(identity_for(id).engine).or_default() += 1;
      }
      let chrome = counts[&Engine::Chrome];
      assert!(
         chrome > 2_500 && chrome < 4_000,
         "chrome share {chrome}/5000"
      );
      for engine in Engine::ALL {
         assert!(
            counts.get(&engine).copied().unwrap_or(0) > 0,
            "{engine} never picked"
         );
      }
   }

   #[test]
   fn fallback_is_second_newest() {
      assert_eq!(
         fallback_profile(Engine::Chrome),
         ProfileVersion::Chrome(157)
      );
      assert_eq!(
         fallback_profile(Engine::Safari),
         ProfileVersion::Safari("26.6")
      );
   }

   #[tokio::test]
   #[ignore = "reaches x.com and quic.browserleaks.com through the first proxy exit"]
   async fn chrome_http3_and_tls_fingerprint_over_proxy() {
      let proxies = std::fs::read_to_string("proxies.txt").expect("proxies.txt");
      let line = proxies
         .lines()
         .find(|line| !line.trim().is_empty() && !line.starts_with('#'))
         .expect("an exit");
      let parts: Vec<&str> = line.trim().split(':').collect();
      let url = format!(
         "socks5h://{}:{}@{}:{}",
         parts[2], parts[3], parts[0], parts[1]
      );
      let report = koon_core::verify::verify("chrome157-windows", Some(&url))
         .await
         .expect("verify");
      assert_eq!(
         report.outcome,
         koon_core::verify::Outcome::Match,
         "{report:?}"
      );

      let client = Client::builder(
         Identity {
            engine: Engine::Chrome,
            os:     Os::Windows,
         }
         .profile(),
      )
      .proxy(&url)
      .expect("proxy")
      .ip_version(IpVersion::V4)
      .alt_svc_h3("x.com", 443, 443)
      .alt_svc_h3("api.x.com", 443, 443)
      .timeout(Duration::from_secs(15))
      .build()
      .expect("client");

      let resp = client.get("https://x.com/").await.expect("x.com");
      assert_eq!(resp.version, "h3", "expected HTTP/3 to x.com");
      let api_resp = client.get("https://api.x.com/").await.expect("api.x.com");
      assert_eq!(api_resp.version, "h3", "expected HTTP/3 to api.x.com");
      client.shutdown().await;

      // Verify cold H2 -> H3 automatic background upgrade without pre-seeding
      // Alt-Svc.
      let cold = Client::builder(
         Identity {
            engine: Engine::Chrome,
            os:     Os::Windows,
         }
         .profile(),
      )
      .proxy(&url)
      .expect("proxy")
      .ip_version(IpVersion::V4)
      .timeout(Duration::from_secs(15))
      .build()
      .expect("cold client");
      let first = cold.get("https://x.com/").await.expect("first x.com");
      assert_eq!(first.version, "h2", "cold first call should start on h2");
      tokio::time::sleep(Duration::from_millis(250)).await;
      let second = cold.get("https://x.com/").await.expect("second x.com");
      assert_eq!(
         second.version, "h3",
         "second call after Alt-Svc should upgrade to h3"
      );
      cold.shutdown().await;

      // Verify all 5 engines (Chrome, Edge, Firefox, Safari, Opera) handshake on
      // HTTP/3.
      let proxy_cfg = ProxyConfig {
         host:     parts[0].to_owned(),
         port:     parts[1].parse().expect("port"),
         kind:     ProxyKind::Socks5,
         username: Some(parts[2].to_owned()),
         password: Some(parts[3].to_owned()),
      };
      for engine in Engine::ALL {
         let (version, protocol) = newest_accepted(engine, Some(&proxy_cfg))
            .await
            .unwrap_or_else(|| panic!("{engine} failed to handshake with x.com"));
         eprintln!("probe accepted: {engine}={version} via {protocol}");
         assert_eq!(
            protocol, "h3",
            "expected {engine}={version} to negotiate HTTP/3 with x.com"
         );
      }
   }

   #[tokio::test]
   #[ignore = "measures QUIC 0-RTT resumption and ECH over the first proxy exit"]
   async fn quic_0rtt_and_ech_over_socks5_proxy() {
      let proxies = std::fs::read_to_string("proxies.txt").expect("proxies.txt");
      let line = proxies
         .lines()
         .find(|line| !line.trim().is_empty() && !line.starts_with('#'))
         .expect("an exit");
      let parts: Vec<&str> = line.trim().split(':').collect();
      let url = format!(
         "socks5h://{}:{}@{}:{}",
         parts[2], parts[3], parts[0], parts[1]
      );

      let client = Client::builder(
         Identity {
            engine: Engine::Chrome,
            os:     Os::Windows,
         }
         .profile(),
      )
      .proxy(&url)
      .expect("proxy")
      .ip_version(IpVersion::V4)
      .alt_svc_h3("quic.browserleaks.com", 443, 443)
      .alt_svc_h3("x.com", 443, 443)
      .alt_svc_h3("api.x.com", 443, 443)
      .timeout(Duration::from_secs(15))
      .build()
      .expect("client");

      // 1. quic.browserleaks.com: cold handshake -> close -> 0-RTT resumed handshake.
      let t0 = std::time::Instant::now();
      let r1 = client
         .get("https://quic.browserleaks.com/?minify=1")
         .await
         .expect("cold quic.browserleaks");
      let d1 = t0.elapsed();
      let j1: serde_json::Value = serde_json::from_slice(&r1.body).expect("json1");
      eprintln!(
         "quic.browserleaks COLD: ver={} resumed={} ech={:?} 0-rtt={:?} ja4={} ({:?})",
         r1.version,
         r1.tls_resumed,
         j1.pointer("/tls/ech_success"),
         j1.pointer("/quic/0-rtt"),
         j1["ja4"],
         d1
      );
      assert_eq!(r1.version, "h3");

      // Drop the pooled QUIC connection; TLS ticket + QUIC token + H3 peer settings
      // stay cached.
      client.close();
      let t1 = std::time::Instant::now();
      let r2 = client
         .get("https://quic.browserleaks.com/?minify=1")
         .await
         .expect("resumed quic.browserleaks");
      let d2 = t1.elapsed();
      let j2: serde_json::Value = serde_json::from_slice(&r2.body).expect("json2");
      eprintln!(
         "quic.browserleaks RESUMED: ver={} resumed={} ech={:?} 0-rtt={:?} ja4={} ({:?})",
         r2.version,
         r2.tls_resumed,
         j2.pointer("/tls/ech_success"),
         j2.pointer("/quic/0-rtt"),
         j2["ja4"],
         d2
      );
      assert_eq!(r2.version, "h3");
      assert!(
         r2.tls_resumed,
         "expected TLS PSK resumption on second H3 conn"
      );

      // 2. api.x.com: cold -> pooled -> close -> 0-RTT resumed.
      let tx0 = std::time::Instant::now();
      let x1 = client
         .get("https://api.x.com/")
         .await
         .expect("cold api.x.com");
      let dx1 = tx0.elapsed();
      let txp = std::time::Instant::now();
      let xp = client
         .get("https://api.x.com/")
         .await
         .expect("pooled api.x.com");
      let dxp = txp.elapsed();
      client.close();
      let tx2 = std::time::Instant::now();
      let x2 = client
         .get("https://api.x.com/")
         .await
         .expect("resumed api.x.com");
      let dx2 = tx2.elapsed();
      eprintln!(
         "api.x.com: cold={} (resumed={}, {:?}), pooled={} (reused={}, {:?}), resumed={} \
          (resumed={}, {:?})",
         x1.version,
         x1.tls_resumed,
         dx1,
         xp.version,
         xp.connection_reused,
         dxp,
         x2.version,
         x2.tls_resumed,
         dx2,
      );
      assert_eq!(x1.version, "h3");
      assert_eq!(xp.version, "h3");
      assert!(xp.connection_reused);
      assert_eq!(x2.version, "h3");
      assert!(
         x2.tls_resumed,
         "expected TLS resumption on resumed api.x.com H3 conn"
      );
      client.shutdown().await;
   }
}
