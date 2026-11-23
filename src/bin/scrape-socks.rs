//! High-performance local SOCKS5 (username/password) forwarder bound to
//! WireGuard scrape tunnel addresses.
//!
//! Drop-in replacement for `/usr/local/sbin/scrape-socks.py`: reads the same
//! `<port> <user> <password> <bind_ip> [via]` map file and listens on
//! `127.0.0.1` plus any addresses in `SCRAPE_SOCKS_LISTEN`. Supports both
//! RFC 1928 TCP `CONNECT` (`0x01`) and `UDP ASSOCIATE` (`0x03`) for HTTP/3
//! (QUIC), including chaining `UDP ASSOCIATE` through Mullvad's in-tunnel
//! SOCKS5 relay (`via`).

use std::{
   collections::HashMap,
   env,
   fs,
   io,
   net::{
      IpAddr,
      Ipv4Addr,
      Ipv6Addr,
      SocketAddr,
   },
   sync::Arc,
   time::Duration,
};

use tokio::{
   io::{
      AsyncReadExt as _,
      AsyncWriteExt as _,
   },
   net::{
      TcpListener,
      TcpSocket,
      TcpStream,
      UdpSocket,
      lookup_host,
   },
   time::{
      Instant,
      sleep,
      timeout,
   },
};

const LISTEN_BACKLOG: u32 = 1024;
const DEFAULT_LISTEN: &str = "127.0.0.1";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const UDP_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const UDP_DNS_TTL: Duration = Duration::from_secs(60);
const UDP_DNS_CACHE_MAX: usize = 64;
const PIPE_BUF_BYTES: usize = 64 * 1024;
const UDP_BUF_BYTES: usize = 0xFFFF;
const SOCKS_CMD_CONNECT: u8 = 1;
const SOCKS_CMD_UDP_ASSOCIATE: u8 = 3;
const MULLVAD_SOCKS: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 64, 0, 1)), 1080);

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExitEntry {
   port:     u16,
   user:     String,
   password: String,
   bind_ip:  IpAddr,
   via:      Option<SocketAddr>,
}

fn parse_map(content: &str) -> Result<Vec<ExitEntry>, String> {
   let mut exits = Vec::new();
   for (idx, raw) in content.lines().enumerate() {
      let line = raw.split('#').next().unwrap_or("").trim();
      if line.is_empty() {
         continue;
      }
      let parts: Vec<&str> = line.split_whitespace().collect();
      let (port_s, user, password, bind_s, via) = match parts.as_slice() {
         [port_s, user, password, bind_s] => (*port_s, *user, *password, *bind_s, None),
         [port_s, user, password, bind_s, "via"] => {
            (*port_s, *user, *password, *bind_s, Some(MULLVAD_SOCKS))
         },
         _ => return Err(format!("bad map line {}: {line}", idx + 1)),
      };
      let port: u16 = port_s
         .parse()
         .map_err(|err| format!("bad port on line {}: {err}", idx + 1))?;
      let bind_ip: IpAddr = bind_s
         .parse()
         .map_err(|err| format!("bad bind IP on line {}: {err}", idx + 1))?;
      exits.push(ExitEntry {
         port,
         user: user.to_owned(),
         password: password.to_owned(),
         bind_ip,
         via,
      });
   }
   Ok(exits)
}

fn listen_hosts() -> Vec<IpAddr> {
   let mut hosts = vec![IpAddr::V4(Ipv4Addr::LOCALHOST)];
   if let Ok(raw) = env::var("SCRAPE_SOCKS_LISTEN") {
      for token in raw.replace(',', " ").split_whitespace() {
         if let Ok(ip) = token.parse::<IpAddr>()
            && !hosts.contains(&ip)
         {
            hosts.push(ip);
         }
      }
   }
   hosts
}

fn bind_listener(addr: SocketAddr) -> io::Result<TcpListener> {
   let socket = match addr {
      SocketAddr::V4(_) => TcpSocket::new_v4()?,
      SocketAddr::V6(_) => TcpSocket::new_v6()?,
   };
   socket.set_reuseaddr(true)?;
   socket.bind(addr)?;
   socket.listen(LISTEN_BACKLOG)
}

#[expect(
   deprecated,
   reason = "SO_LINGER with Duration::ZERO sends immediate RST and never blocks on drop"
)]
fn set_rst_linger(stream: &TcpStream) {
   let _ = stream.set_linger(Some(Duration::ZERO));
}

fn abort_socket(stream: TcpStream) {
   set_rst_linger(&stream);
   drop(stream);
}

enum TargetAddr {
   V4([u8; 4]),
   V6([u8; 16]),
   Domain(String),
}

async fn socks5_auth(stream: &mut TcpStream, exit: &ExitEntry) -> io::Result<bool> {
   let mut hdr = [0_u8; 2];
   stream.read_exact(&mut hdr).await?;
   if hdr[0] != 5 {
      return Ok(false);
   }
   let mut methods = vec![0_u8; usize::from(hdr[1])];
   stream.read_exact(&mut methods).await?;
   if !methods.contains(&2) {
      stream.write_all(&[5, 0xFF]).await?;
      return Ok(false);
   }
   stream.write_all(&[5, 2]).await?;

   let mut ver = [0_u8; 1];
   stream.read_exact(&mut ver).await?;
   if ver[0] != 1 {
      return Ok(false);
   }
   let mut ulen = [0_u8; 1];
   stream.read_exact(&mut ulen).await?;
   let mut uname = vec![0_u8; usize::from(ulen[0])];
   stream.read_exact(&mut uname).await?;

   let mut plen = [0_u8; 1];
   stream.read_exact(&mut plen).await?;
   let mut pass = vec![0_u8; usize::from(plen[0])];
   stream.read_exact(&mut pass).await?;

   let ok = uname == exit.user.as_bytes() && pass == exit.password.as_bytes();
   stream.write_all(&[1, u8::from(!ok)]).await?;
   Ok(ok)
}

async fn connect_bound(bind_ip: IpAddr, target: SocketAddr) -> io::Result<TcpStream> {
   let socket = match (bind_ip, target) {
      (IpAddr::V4(_), SocketAddr::V4(_)) => TcpSocket::new_v4()?,
      (IpAddr::V6(_), SocketAddr::V6(_)) => TcpSocket::new_v6()?,
      _ => {
         return Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "address family mismatch",
         ));
      },
   };
   socket.bind(SocketAddr::new(bind_ip, 0))?;
   let stream = timeout(CONNECT_TIMEOUT, socket.connect(target))
      .await
      .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connect timeout"))??;
   let _ = stream.set_nodelay(true);
   Ok(stream)
}

async fn open_remote(target: &TargetAddr, port: u16, exit: &ExitEntry) -> io::Result<TcpStream> {
   if let Some(via_addr) = exit.via {
      let mut remote = connect_bound(exit.bind_ip, via_addr).await?;
      remote.write_all(&[5, 1, 0]).await?;
      let mut reply = [0_u8; 2];
      timeout(CONNECT_TIMEOUT, remote.read_exact(&mut reply))
         .await
         .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "via auth timeout"))??;
      if reply != [5, 0] {
         abort_socket(remote);
         return Err(io::Error::other("mullvad socks rejected no-auth"));
      }
      let mut req = Vec::with_capacity(64);
      req.extend_from_slice(&[5, 1, 0]);
      match target {
         TargetAddr::V4(octets) => {
            req.push(1);
            req.extend_from_slice(octets);
         },
         TargetAddr::V6(octets) => {
            req.push(4);
            req.extend_from_slice(octets);
         },
         TargetAddr::Domain(host) => {
            if let Ok(v4) = host.parse::<Ipv4Addr>() {
               req.push(1);
               req.extend_from_slice(&v4.octets());
            } else if let Ok(v6) = host.parse::<std::net::Ipv6Addr>() {
               req.push(4);
               req.extend_from_slice(&v6.octets());
            } else {
               let raw = host.as_bytes();
               let len = u8::try_from(raw.len())
                  .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "host too long"))?;
               req.push(3);
               req.push(len);
               req.extend_from_slice(raw);
            }
         },
      }
      req.extend_from_slice(&port.to_be_bytes());
      remote.write_all(&req).await?;

      let mut hdr = [0_u8; 4];
      timeout(CONNECT_TIMEOUT, remote.read_exact(&mut hdr))
         .await
         .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "via connect timeout"))??;
      if hdr[1] != 0 {
         abort_socket(remote);
         return Err(io::Error::other(format!("mullvad socks status {}", hdr[1])));
      }
      match hdr[3] {
         1 => {
            let mut skip = [0_u8; 6];
            timeout(CONNECT_TIMEOUT, remote.read_exact(&mut skip))
               .await
               .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "via addr timeout"))??;
         },
         3 => {
            let mut n = [0_u8; 1];
            timeout(CONNECT_TIMEOUT, remote.read_exact(&mut n))
               .await
               .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "via addr timeout"))??;
            let mut skip = vec![0_u8; usize::from(n[0]) + 2];
            timeout(CONNECT_TIMEOUT, remote.read_exact(&mut skip))
               .await
               .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "via addr timeout"))??;
         },
         4 => {
            let mut skip = [0_u8; 18];
            timeout(CONNECT_TIMEOUT, remote.read_exact(&mut skip))
               .await
               .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "via addr timeout"))??;
         },
         _ => {
            abort_socket(remote);
            return Err(io::Error::other("mullvad socks unknown atyp"));
         },
      }
      return Ok(remote);
   }

   let addr = match target {
      TargetAddr::V4(octets) => SocketAddr::new(IpAddr::V4(Ipv4Addr::from(*octets)), port),
      TargetAddr::V6(octets) => {
         SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::from(*octets)), port)
      },
      TargetAddr::Domain(host) => {
         let want_v4 = exit.bind_ip.is_ipv4();
         let addrs = timeout(CONNECT_TIMEOUT, lookup_host((host.as_str(), port)))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "dns timeout"))??;
         addrs
            .into_iter()
            .find(|candidate| candidate.is_ipv4() == want_v4)
            .ok_or_else(|| io::Error::new(io::ErrorKind::AddrNotAvailable, "no matching IP"))?
      },
   };
   connect_bound(exit.bind_ip, addr).await
}

async fn pipe_half<R, W>(mut reader: R, mut writer: W)
where
   R: tokio::io::AsyncRead + Unpin,
   W: tokio::io::AsyncWrite + Unpin,
{
   let mut buf = vec![0_u8; PIPE_BUF_BYTES];
   loop {
      match timeout(IDLE_TIMEOUT, reader.read(&mut buf)).await {
         Ok(Ok(0) | Err(_)) | Err(_) => break,
         Ok(Ok(n)) => {
            if writer.write_all(&buf[..n]).await.is_err() {
               break;
            }
         },
      }
   }
}

async fn write_socks5_reply(stream: &mut TcpStream, rep: u8, bound: SocketAddr) -> io::Result<()> {
   let mut buf = [0_u8; 22];
   buf[0] = 5;
   buf[1] = rep;
   buf[2] = 0;
   let len = match bound {
      SocketAddr::V4(v4) => {
         buf[3] = 1;
         buf[4..8].copy_from_slice(&v4.ip().octets());
         buf[8..10].copy_from_slice(&v4.port().to_be_bytes());
         10
      },
      SocketAddr::V6(v6) => {
         buf[3] = 4;
         buf[4..20].copy_from_slice(&v6.ip().octets());
         buf[20..22].copy_from_slice(&v6.port().to_be_bytes());
         22
      },
   };
   stream.write_all(&buf[..len]).await
}

enum UdpDest<'a> {
   Addr(SocketAddr),
   Domain(&'a str, u16),
}

/// Parse an RFC 1928 §7 SOCKS5 UDP datagram header. Fragmented (`FRAG != 0`)
/// and malformed packets return `None` and are dropped per the RFC.
fn parse_udp_datagram(packet: &[u8]) -> Option<(UdpDest<'_>, &[u8])> {
   if packet.len() < 4 || packet[0] != 0 || packet[1] != 0 || packet[2] != 0 {
      return None;
   }
   match packet[3] {
      1 => {
         if packet.len() < 10 {
            return None;
         }
         let ip = Ipv4Addr::new(packet[4], packet[5], packet[6], packet[7]);
         let port = u16::from_be_bytes([packet[8], packet[9]]);
         Some((UdpDest::Addr(SocketAddr::from((ip, port))), &packet[10..]))
      },
      4 => {
         if packet.len() < 22 {
            return None;
         }
         let octets: [u8; 16] = packet[4..20].try_into().ok()?;
         let ip = Ipv6Addr::from(octets);
         let port = u16::from_be_bytes([packet[20], packet[21]]);
         Some((UdpDest::Addr(SocketAddr::from((ip, port))), &packet[22..]))
      },
      3 => {
         let dlen = usize::from(*packet.get(4)?);
         if dlen == 0 || packet.len() < 5 + dlen + 2 {
            return None;
         }
         let host = std::str::from_utf8(&packet[5..5 + dlen]).ok()?;
         let port = u16::from_be_bytes([packet[5 + dlen], packet[6 + dlen]]);
         Some((UdpDest::Domain(host, port), &packet[7 + dlen..]))
      },
      _ => None,
   }
}

/// Write an RFC 1928 §7 SOCKS5 UDP header plus `payload` into `out`.
fn encode_udp_datagram(src: SocketAddr, payload: &[u8], out: &mut [u8]) -> Option<usize> {
   let hdr_len: usize = match src {
      SocketAddr::V4(_) => 10,
      SocketAddr::V6(_) => 22,
   };
   let total = hdr_len.checked_add(payload.len())?;
   if total > out.len() {
      return None;
   }
   out[0] = 0;
   out[1] = 0;
   out[2] = 0;
   match src {
      SocketAddr::V4(v4) => {
         out[3] = 1;
         out[4..8].copy_from_slice(&v4.ip().octets());
         out[8..10].copy_from_slice(&v4.port().to_be_bytes());
      },
      SocketAddr::V6(v6) => {
         out[3] = 4;
         out[4..20].copy_from_slice(&v6.ip().octets());
         out[20..22].copy_from_slice(&v6.port().to_be_bytes());
      },
   }
   out[hdr_len..total].copy_from_slice(payload);
   Some(total)
}

async fn resolve_direct_udp(
   host: &str,
   port: u16,
   want_v4: bool,
   cache: &mut HashMap<(String, u16), (SocketAddr, Instant)>,
) -> Option<SocketAddr> {
   if let Ok(ip) = host.parse::<IpAddr>() {
      return (ip.is_ipv4() == want_v4).then_some(SocketAddr::new(ip, port));
   }
   let now = Instant::now();
   if let Some(&(addr, expires)) = cache.get(&(host.to_owned(), port))
      && now < expires
   {
      return Some(addr);
   }
   let addrs = timeout(CONNECT_TIMEOUT, lookup_host((host, port)))
      .await
      .ok()?
      .ok()?;
   let resolved = addrs
      .into_iter()
      .find(|candidate| candidate.is_ipv4() == want_v4)?;
   if cache.len() >= UDP_DNS_CACHE_MAX {
      cache.retain(|_, &mut (_, expires)| now < expires);
      if cache.len() >= UDP_DNS_CACHE_MAX {
         cache.clear();
      }
   }
   cache.insert((host.to_owned(), port), (resolved, now + UDP_DNS_TTL));
   Some(resolved)
}

/// Establish an upstream SOCKS5 `UDP ASSOCIATE` session through `via_addr`
/// (e.g. Mullvad's in-tunnel SOCKS5 at `10.64.0.1:1080`), bound to `bind_ip`.
async fn open_via_udp(
   bind_ip: IpAddr,
   via_addr: SocketAddr,
) -> io::Result<(TcpStream, SocketAddr)> {
   let mut remote = connect_bound(bind_ip, via_addr).await?;
   remote.write_all(&[5, 1, 0]).await?;
   let mut reply = [0_u8; 2];
   timeout(CONNECT_TIMEOUT, remote.read_exact(&mut reply))
      .await
      .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "via udp auth timeout"))??;
   if reply != [5, 0] {
      abort_socket(remote);
      return Err(io::Error::other("mullvad socks rejected no-auth"));
   }

   // CMD=0x03 (UDP ASSOCIATE), ATYP=1, 0.0.0.0:0.
   remote
      .write_all(&[5, SOCKS_CMD_UDP_ASSOCIATE, 0, 1, 0, 0, 0, 0, 0, 0])
      .await?;

   let mut hdr = [0_u8; 4];
   timeout(CONNECT_TIMEOUT, remote.read_exact(&mut hdr))
      .await
      .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "via udp associate timeout"))??;
   if hdr[0] != 5 || hdr[1] != 0 {
      abort_socket(remote);
      return Err(io::Error::other(format!(
         "mullvad socks udp associate status {}",
         hdr[1]
      )));
   }

   let raw_relay = match hdr[3] {
      1 => {
         let mut body = [0_u8; 6];
         timeout(CONNECT_TIMEOUT, remote.read_exact(&mut body))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "via udp addr timeout"))??;
         let ip = Ipv4Addr::new(body[0], body[1], body[2], body[3]);
         let port = u16::from_be_bytes([body[4], body[5]]);
         SocketAddr::from((ip, port))
      },
      4 => {
         let mut body = [0_u8; 18];
         timeout(CONNECT_TIMEOUT, remote.read_exact(&mut body))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "via udp addr timeout"))??;
         let octets: [u8; 16] = body[..16]
            .try_into()
            .map_err(|_| io::Error::other("invalid v6 relay"))?;
         let ip = Ipv6Addr::from(octets);
         let port = u16::from_be_bytes([body[16], body[17]]);
         SocketAddr::from((ip, port))
      },
      3 => {
         let mut len_buf = [0_u8; 1];
         timeout(CONNECT_TIMEOUT, remote.read_exact(&mut len_buf))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "via udp addr timeout"))??;
         let dlen = usize::from(len_buf[0]);
         let mut body = vec![0_u8; dlen + 2];
         timeout(CONNECT_TIMEOUT, remote.read_exact(&mut body))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "via udp addr timeout"))??;
         let host = String::from_utf8(body[..dlen].to_vec())
            .map_err(|_| io::Error::other("invalid utf8 relay domain"))?;
         let port = u16::from_be_bytes([body[dlen], body[dlen + 1]]);
         let want_v4 = bind_ip.is_ipv4();
         let addrs = timeout(CONNECT_TIMEOUT, lookup_host((host.as_str(), port)))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "via relay dns timeout"))??;
         addrs
            .into_iter()
            .find(|candidate| candidate.is_ipv4() == want_v4)
            .ok_or_else(|| {
               io::Error::new(io::ErrorKind::AddrNotAvailable, "no matching relay IP")
            })?
      },
      _ => {
         abort_socket(remote);
         return Err(io::Error::other("mullvad socks udp unknown atyp"));
      },
   };

   // RFC 1928: if the server reports 0.0.0.0 / ::, send UDP to the proxy's TCP IP.
   let relay = if raw_relay.ip().is_unspecified() {
      SocketAddr::new(via_addr.ip(), raw_relay.port())
   } else {
      raw_relay
   };
   Ok((remote, relay))
}

/// RFC 1928 §7 SOCKS5 `UDP ASSOCIATE` handler for both direct WireGuard exits
/// and chained `via` Mullvad exits.
async fn handle_udp_associate(
   mut client: TcpStream,
   target: &TargetAddr,
   port: u16,
   exit: &ExitEntry,
) {
   let Ok(client_local) = client.local_addr() else {
      abort_socket(client);
      return;
   };
   let Ok(client_peer) = client.peer_addr() else {
      abort_socket(client);
      return;
   };
   let client_peer_ip = client_peer.ip().to_canonical();

   let Ok(client_udp) = UdpSocket::bind(SocketAddr::new(client_local.ip(), 0)).await else {
      let _ = client.write_all(&[5, 1, 0, 1, 0, 0, 0, 0, 0, 0]).await;
      abort_socket(client);
      return;
   };
   let Ok(client_relay_addr) = client_udp.local_addr() else {
      abort_socket(client);
      return;
   };

   let Ok(upstream_udp) = UdpSocket::bind(SocketAddr::new(exit.bind_ip, 0)).await else {
      let _ = client.write_all(&[5, 1, 0, 1, 0, 0, 0, 0, 0, 0]).await;
      abort_socket(client);
      return;
   };

   let mut via_state = if let Some(via_addr) = exit.via {
      match open_via_udp(exit.bind_ip, via_addr).await {
         Ok((via_control, via_relay)) => {
            set_rst_linger(&via_control);
            Some((via_control, via_relay))
         },
         Err(_) => {
            let _ = client.write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0]).await;
            abort_socket(client);
            return;
         },
      }
   } else {
      None
   };

   if write_socks5_reply(&mut client, 0, client_relay_addr)
      .await
      .is_err()
   {
      abort_socket(client);
      if let Some((via_control, _)) = via_state {
         abort_socket(via_control);
      }
      return;
   }
   set_rst_linger(&client);

   // Seed client UDP peer if the client supplied a concrete non-zero address
   // matching its TCP control peer; otherwise learn it from the first packet.
   let mut client_udp_peer: Option<SocketAddr> = if port != 0 {
      match target {
         TargetAddr::V4(octets) => {
            let ip = IpAddr::V4(Ipv4Addr::from(*octets));
            (!ip.is_unspecified() && ip.to_canonical() == client_peer_ip)
               .then_some(SocketAddr::new(ip, port))
         },
         TargetAddr::V6(octets) => {
            let ip = IpAddr::V6(Ipv6Addr::from(*octets));
            (!ip.is_unspecified() && ip.to_canonical() == client_peer_ip)
               .then_some(SocketAddr::new(ip, port))
         },
         TargetAddr::Domain(_) => None,
      }
   } else {
      None
   };

   let want_v4 = exit.bind_ip.is_ipv4();
   let mut dns_cache: HashMap<(String, u16), (SocketAddr, Instant)> = HashMap::new();
   let mut client_buf = vec![0_u8; UDP_BUF_BYTES];
   let mut upstream_buf = vec![0_u8; UDP_BUF_BYTES];
   let mut reply_buf = if via_state.is_none() {
      vec![0_u8; UDP_BUF_BYTES + 22]
   } else {
      Vec::new()
   };
   let mut ctl_buf = [0_u8; 64];
   let mut via_ctl_buf = [0_u8; 64];

   let idle = sleep(UDP_IDLE_TIMEOUT);
   tokio::pin!(idle);

   loop {
      let via_relay = via_state.as_ref().map(|&(_, relay)| relay);
      tokio::select! {
         () = &mut idle => break,

         ctl = client.read(&mut ctl_buf) => {
            match ctl {
               Ok(0) | Err(_) => break,
               Ok(_) => {},
            }
         },

         via_ctl = async {
            if let Some((ref mut via_control, _)) = via_state {
               via_control.read(&mut via_ctl_buf).await
            } else {
               std::future::pending().await
            }
         } => {
            match via_ctl {
               Ok(0) | Err(_) => break,
               Ok(_) => {},
            }
         },

         recv = client_udp.recv_from(&mut client_buf) => {
            let Ok((n, src)) = recv else {
               break;
            };
            if src.ip().to_canonical() != client_peer_ip {
               continue;
            }
            client_udp_peer = Some(src);
            let packet = &client_buf[..n];
            if let Some(relay_addr) = via_relay {
               // Passthrough: upstream Mullvad SOCKS5 accepts standard RFC 1928
               // UDP headers (IPv4, IPv6, and ATYP=3 domain names).
               if packet.len() >= 4 && packet[0] == 0 && packet[1] == 0 && packet[2] == 0 {
                  if upstream_udp.send_to(packet, relay_addr).await.is_ok() {
                     idle.as_mut().reset(Instant::now() + UDP_IDLE_TIMEOUT);
                  }
               }
            } else if let Some((dest, payload)) = parse_udp_datagram(packet) {
               let dest_addr = match dest {
                  UdpDest::Addr(addr) => (addr.is_ipv4() == want_v4).then_some(addr),
                  UdpDest::Domain(host, dst_port) => {
                     resolve_direct_udp(host, dst_port, want_v4, &mut dns_cache).await
                  },
               };
               if let Some(addr) = dest_addr
                  && upstream_udp.send_to(payload, addr).await.is_ok()
               {
                  idle.as_mut().reset(Instant::now() + UDP_IDLE_TIMEOUT);
               }
            }
         },

         recv = upstream_udp.recv_from(&mut upstream_buf) => {
            let Ok((n, src)) = recv else {
               break;
            };
            let Some(peer) = client_udp_peer else {
               continue;
            };
            if let Some(relay_addr) = via_relay {
               // Match upstream relay by IP only (some relays answer from a
               // different egress port than BND.PORT).
               if src.ip().to_canonical() != relay_addr.ip().to_canonical() {
                  continue;
               }
               if client_udp.send_to(&upstream_buf[..n], peer).await.is_ok() {
                  idle.as_mut().reset(Instant::now() + UDP_IDLE_TIMEOUT);
               }
            } else if let Some(total) = encode_udp_datagram(src, &upstream_buf[..n], &mut reply_buf)
               && client_udp.send_to(&reply_buf[..total], peer).await.is_ok()
            {
               idle.as_mut().reset(Instant::now() + UDP_IDLE_TIMEOUT);
            }
         },
      }
   }
}

async fn handle(mut client: TcpStream, exit: Arc<ExitEntry>) {
   let _ = client.set_nodelay(true);
   let auth_ok = timeout(CONNECT_TIMEOUT, socks5_auth(&mut client, &exit))
      .await
      .ok()
      .and_then(Result::ok)
      .unwrap_or(false);
   if !auth_ok {
      abort_socket(client);
      return;
   }

   let mut req = [0_u8; 4];
   if timeout(CONNECT_TIMEOUT, client.read_exact(&mut req))
      .await
      .ok()
      .and_then(Result::ok)
      .is_none()
   {
      abort_socket(client);
      return;
   }
   let cmd = req[1];
   if req[0] != 5 || (cmd != SOCKS_CMD_CONNECT && cmd != SOCKS_CMD_UDP_ASSOCIATE) {
      let _ = client.write_all(&[5, 7, 0, 1, 0, 0, 0, 0, 0, 0]).await;
      abort_socket(client);
      return;
   }

   let target = match req[3] {
      1 => {
         let mut buf = [0_u8; 4];
         if timeout(CONNECT_TIMEOUT, client.read_exact(&mut buf))
            .await
            .ok()
            .and_then(Result::ok)
            .is_none()
         {
            abort_socket(client);
            return;
         }
         TargetAddr::V4(buf)
      },
      3 => {
         let mut len = [0_u8; 1];
         if timeout(CONNECT_TIMEOUT, client.read_exact(&mut len))
            .await
            .ok()
            .and_then(Result::ok)
            .is_none()
         {
            abort_socket(client);
            return;
         }
         let mut host = vec![0_u8; usize::from(len[0])];
         if timeout(CONNECT_TIMEOUT, client.read_exact(&mut host))
            .await
            .ok()
            .and_then(Result::ok)
            .is_none()
         {
            abort_socket(client);
            return;
         }
         let Ok(host) = String::from_utf8(host) else {
            abort_socket(client);
            return;
         };
         TargetAddr::Domain(host)
      },
      4 => {
         let mut buf = [0_u8; 16];
         if timeout(CONNECT_TIMEOUT, client.read_exact(&mut buf))
            .await
            .ok()
            .and_then(Result::ok)
            .is_none()
         {
            abort_socket(client);
            return;
         }
         TargetAddr::V6(buf)
      },
      _ => {
         abort_socket(client);
         return;
      },
   };

   let mut port_buf = [0_u8; 2];
   if timeout(CONNECT_TIMEOUT, client.read_exact(&mut port_buf))
      .await
      .ok()
      .and_then(Result::ok)
      .is_none()
   {
      abort_socket(client);
      return;
   }
   let port = u16::from_be_bytes(port_buf);

   if cmd == SOCKS_CMD_UDP_ASSOCIATE {
      handle_udp_associate(client, &target, port, &exit).await;
      return;
   }

   let remote = match open_remote(&target, port, &exit).await {
      Ok(stream) => stream,
      Err(_) => {
         let _ = client.write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0]).await;
         abort_socket(client);
         return;
      },
   };

   if client
      .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0])
      .await
      .is_err()
   {
      abort_socket(client);
      abort_socket(remote);
      return;
   }

   // RST on close (matching scrape-socks.py's abort) so local loopback sockets
   // never linger in CLOSE-WAIT when either peer finishes.
   set_rst_linger(&client);
   set_rst_linger(&remote);

   let (client_r, client_w) = client.into_split();
   let (remote_r, remote_w) = remote.into_split();

   tokio::select! {
      () = pipe_half(client_r, remote_w) => {},
      () = pipe_half(remote_r, client_w) => {},
   }
}

async fn serve_host(exits: Arc<[Arc<ExitEntry>]>, host: IpAddr) {
   let mut delay = Duration::from_secs(1);
   loop {
      let mut listeners = Vec::with_capacity(exits.len());
      let mut failed = None;
      for exit in exits.iter() {
         let addr = SocketAddr::new(host, exit.port);
         match bind_listener(addr) {
            Ok(listener) => listeners.push((listener, Arc::clone(exit))),
            Err(err) => {
               failed = Some((addr, err));
               break;
            },
         }
      }
      if let Some((addr, err)) = failed {
         drop(listeners);
         eprintln!(
            "scrape-socks: bind {addr} failed: {err}; retrying in {}s",
            delay.as_secs()
         );
         sleep(delay).await;
         delay = (delay * 2).min(Duration::from_secs(30));
         continue;
      }

      let mut tasks = tokio::task::JoinSet::new();
      for (listener, exit) in listeners {
         tasks.spawn(async move {
            loop {
               match listener.accept().await {
                  Ok((stream, _peer)) => {
                     let exit = Arc::clone(&exit);
                     tokio::spawn(handle(stream, exit));
                  },
                  Err(err) => {
                     eprintln!("scrape-socks: accept error on port {}: {err}", exit.port);
                     sleep(Duration::from_millis(50)).await;
                  },
               }
            }
         });
      }
      while tasks.join_next().await.is_some() {}
      return;
   }
}

#[tokio::main]
async fn main() {
   let Some(map_path) = env::args().nth(1) else {
      eprintln!("usage: scrape-socks <map-file>");
      std::process::exit(2);
   };
   let content = match fs::read_to_string(&map_path) {
      Ok(text) => text,
      Err(err) => {
         eprintln!("scrape-socks: cannot read {map_path}: {err}");
         std::process::exit(1);
      },
   };
   let parsed = match parse_map(&content) {
      Ok(entries) => entries,
      Err(err) => {
         eprintln!("scrape-socks: {err}");
         std::process::exit(1);
      },
   };
   let hosts = listen_hosts();
   let host_list = hosts
      .iter()
      .map(ToString::to_string)
      .collect::<Vec<_>>()
      .join(",");
   eprintln!(
      "scrape-socks (rust): {} ports, listen {host_list} (default {DEFAULT_LISTEN}), backlog \
       {LISTEN_BACKLOG}",
      parsed.len()
   );

   let exits: Arc<[Arc<ExitEntry>]> = parsed.into_iter().map(Arc::new).collect();
   let mut tasks = tokio::task::JoinSet::new();
   for host in hosts {
      tasks.spawn(serve_host(Arc::clone(&exits), host));
   }

   #[cfg(unix)]
   {
      let mut sigterm =
         tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
      tokio::select! {
         _ = tokio::signal::ctrl_c() => {},
         () = async {
            if let Some(ref mut sig) = sigterm {
               sig.recv().await;
            } else {
               std::future::pending::<()>().await;
            }
         } => {},
      }
   }
   #[cfg(not(unix))]
   {
      let _ = tokio::signal::ctrl_c().await;
   }
   tasks.abort_all();
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn parses_direct_and_via_entries() {
      let sample = "\
         # comment\n1081 alice secret1 10.137.253.125\n1086 bob secret2 10.137.253.125 via # \
                    trailing\n";
      let exits = parse_map(sample).unwrap();
      assert_eq!(exits.len(), 2);
      assert_eq!(exits[0].port, 1081);
      assert_eq!(exits[0].via, None);
      assert_eq!(exits[1].port, 1086);
      assert_eq!(exits[1].via, Some(MULLVAD_SOCKS));
   }

   #[tokio::test]
   async fn authenticates_and_forwards_tcp() {
      let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
      let echo_addr = echo.local_addr().unwrap();
      tokio::spawn(async move {
         if let Ok((mut s, _)) = echo.accept().await {
            let mut buf = [0_u8; 16];
            if let Ok(n) = s.read(&mut buf).await {
               let _ = s.write_all(&buf[..n]).await;
            }
         }
      });

      let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
      let proxy_addr = proxy_listener.local_addr().unwrap();
      let exit = Arc::new(ExitEntry {
         port:     proxy_addr.port(),
         user:     "u".into(),
         password: "p".into(),
         bind_ip:  IpAddr::V4(Ipv4Addr::LOCALHOST),
         via:      None,
      });
      tokio::spawn(async move {
         if let Ok((stream, _)) = proxy_listener.accept().await {
            handle(stream, exit).await;
         }
      });

      let mut client = TcpStream::connect(proxy_addr).await.unwrap();
      client.write_all(&[5, 1, 2]).await.unwrap();
      let mut resp = [0_u8; 2];
      client.read_exact(&mut resp).await.unwrap();
      assert_eq!(resp, [5, 2]);

      client.write_all(&[1, 1, b'u', 1, b'p']).await.unwrap();
      client.read_exact(&mut resp).await.unwrap();
      assert_eq!(resp, [1, 0]);

      let mut req = vec![5, 1, 0, 1, 127, 0, 0, 1];
      req.extend_from_slice(&echo_addr.port().to_be_bytes());
      client.write_all(&req).await.unwrap();
      let mut conn_resp = [0_u8; 10];
      client.read_exact(&mut conn_resp).await.unwrap();
      assert_eq!(conn_resp[1], 0);

      client.write_all(b"ping").await.unwrap();
      let mut echoed = [0_u8; 4];
      client.read_exact(&mut echoed).await.unwrap();
      assert_eq!(&echoed, b"ping");
   }

   #[test]
   fn parses_and_encodes_socks5_udp_headers() {
      let mut buf = [0_u8; 64];
      let v4_addr: SocketAddr = "162.159.140.229:443".parse().unwrap();
      let len = encode_udp_datagram(v4_addr, b"quic", &mut buf).unwrap();
      let (dest, payload) = parse_udp_datagram(&buf[..len]).unwrap();
      assert!(matches!(dest, UdpDest::Addr(addr) if addr == v4_addr));
      assert_eq!(payload, b"quic");

      let v6_addr: SocketAddr = "[2001:db8::1]:443".parse().unwrap();
      let len6 = encode_udp_datagram(v6_addr, b"v6", &mut buf).unwrap();
      let (dest6, payload6) = parse_udp_datagram(&buf[..len6]).unwrap();
      assert!(matches!(dest6, UdpDest::Addr(addr) if addr == v6_addr));
      assert_eq!(payload6, b"v6");

      let mut domain_pkt = vec![0, 0, 0, 3, 5];
      domain_pkt.extend_from_slice(b"x.com");
      domain_pkt.extend_from_slice(&443_u16.to_be_bytes());
      domain_pkt.extend_from_slice(b"hello");
      let (dest_dom, payload_dom) = parse_udp_datagram(&domain_pkt).unwrap();
      assert!(matches!(dest_dom, UdpDest::Domain("x.com", 443)));
      assert_eq!(payload_dom, b"hello");

      // Fragmented datagram (FRAG=1) must be dropped.
      let mut frag = buf[..len].to_vec();
      frag[2] = 1;
      assert!(parse_udp_datagram(&frag).is_none());
   }

   #[tokio::test]
   async fn authenticates_and_forwards_direct_udp_ipv4_and_domain() {
      let echo_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
      let echo_addr = echo_udp.local_addr().unwrap();
      tokio::spawn(async move {
         let mut buf = vec![0_u8; 4096];
         for _ in 0..2 {
            if let Ok((n, peer)) = echo_udp.recv_from(&mut buf).await {
               let _ = echo_udp.send_to(&buf[..n], peer).await;
            }
         }
      });

      let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
      let proxy_addr = proxy_listener.local_addr().unwrap();
      let exit = Arc::new(ExitEntry {
         port:     proxy_addr.port(),
         user:     "u".into(),
         password: "p".into(),
         bind_ip:  IpAddr::V4(Ipv4Addr::LOCALHOST),
         via:      None,
      });
      let handle_task = tokio::spawn(async move {
         if let Ok((stream, _)) = proxy_listener.accept().await {
            handle(stream, exit).await;
         }
      });

      let mut ctl = TcpStream::connect(proxy_addr).await.unwrap();
      ctl.write_all(&[5, 1, 2]).await.unwrap();
      let mut resp = [0_u8; 2];
      ctl.read_exact(&mut resp).await.unwrap();
      assert_eq!(resp, [5, 2]);
      ctl.write_all(&[1, 1, b'u', 1, b'p']).await.unwrap();
      ctl.read_exact(&mut resp).await.unwrap();
      assert_eq!(resp, [1, 0]);

      ctl.write_all(&[5, 3, 0, 1, 0, 0, 0, 0, 0, 0])
         .await
         .unwrap();
      let mut assoc_resp = [0_u8; 10];
      ctl.read_exact(&mut assoc_resp).await.unwrap();
      assert_eq!(&assoc_resp[..4], &[5, 0, 0, 1]);
      let relay_ip = Ipv4Addr::new(assoc_resp[4], assoc_resp[5], assoc_resp[6], assoc_resp[7]);
      let relay_port = u16::from_be_bytes([assoc_resp[8], assoc_resp[9]]);
      let relay_addr = SocketAddr::from((relay_ip, relay_port));

      let client_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();

      // 1. 1250-byte QUIC-sized datagram via IPv4 (ATYP=1).
      let quic_payload = vec![0xAB_u8; 1250];
      let mut pkt = vec![0_u8; 10 + quic_payload.len()];
      let pkt_len = encode_udp_datagram(echo_addr, &quic_payload, &mut pkt).unwrap();
      client_udp
         .send_to(&pkt[..pkt_len], relay_addr)
         .await
         .unwrap();

      let mut recv_buf = vec![0_u8; 4096];
      let (n, from) = timeout(Duration::from_secs(2), client_udp.recv_from(&mut recv_buf))
         .await
         .unwrap()
         .unwrap();
      assert_eq!(from, relay_addr);
      let (src_dest, payload) = parse_udp_datagram(&recv_buf[..n]).unwrap();
      assert!(matches!(src_dest, UdpDest::Addr(addr) if addr == echo_addr));
      assert_eq!(payload, quic_payload.as_slice());

      // 2. Domain-addressed datagram (ATYP=3, socks5h://) to "localhost".
      let mut dom_pkt = vec![0, 0, 0, 3, 9];
      dom_pkt.extend_from_slice(b"localhost");
      dom_pkt.extend_from_slice(&echo_addr.port().to_be_bytes());
      dom_pkt.extend_from_slice(b"quic-domain");
      client_udp.send_to(&dom_pkt, relay_addr).await.unwrap();

      let (n2, _) = timeout(Duration::from_secs(2), client_udp.recv_from(&mut recv_buf))
         .await
         .unwrap()
         .unwrap();
      let (src_dest2, payload2) = parse_udp_datagram(&recv_buf[..n2]).unwrap();
      assert!(matches!(src_dest2, UdpDest::Addr(addr) if addr == echo_addr));
      assert_eq!(payload2, b"quic-domain");

      // Closing TCP control must terminate the UDP association promptly.
      drop(ctl);
      timeout(Duration::from_secs(2), handle_task)
         .await
         .unwrap()
         .unwrap();
   }

   #[tokio::test]
   async fn chains_via_udp_associate_passthrough() {
      // Mock upstream Mullvad SOCKS5 (no-auth, CMD=3) + target UDP echo.
      let echo_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
      let echo_addr = echo_udp.local_addr().unwrap();
      tokio::spawn(async move {
         let mut buf = vec![0_u8; 4096];
         if let Ok((n, peer)) = echo_udp.recv_from(&mut buf).await {
            let _ = echo_udp.send_to(&buf[..n], peer).await;
         }
      });

      let mock_mullvad_tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
      let mock_mullvad_addr = mock_mullvad_tcp.local_addr().unwrap();
      tokio::spawn(async move {
         let (mut ctl, _) = mock_mullvad_tcp.accept().await.unwrap();
         let mut greet = [0_u8; 3];
         ctl.read_exact(&mut greet).await.unwrap();
         assert_eq!(greet, [5, 1, 0]);
         ctl.write_all(&[5, 0]).await.unwrap();

         let mut req = [0_u8; 10];
         ctl.read_exact(&mut req).await.unwrap();
         assert_eq!(&req[..4], &[5, 3, 0, 1]);

         let relay_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
         let relay_local = relay_udp.local_addr().unwrap();
         write_socks5_reply(&mut ctl, 0, relay_local).await.unwrap();

         let mut buf = vec![0_u8; 4096];
         let (n, caller) = relay_udp.recv_from(&mut buf).await.unwrap();
         let (dest, payload) = parse_udp_datagram(&buf[..n]).unwrap();
         let UdpDest::Addr(target_addr) = dest else {
            panic!("expected IPv4 dest");
         };
         let out_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
         out_udp.send_to(payload, target_addr).await.unwrap();
         let (rn, rsrc) = out_udp.recv_from(&mut buf).await.unwrap();
         let mut wrapped = vec![0_u8; rn + 22];
         let wlen = encode_udp_datagram(rsrc, &buf[..rn], &mut wrapped).unwrap();
         relay_udp.send_to(&wrapped[..wlen], caller).await.unwrap();

         // Hold control open until caller closes it.
         let mut drain = [0_u8; 8];
         let _ = ctl.read(&mut drain).await;
      });

      let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
      let proxy_addr = proxy_listener.local_addr().unwrap();
      let exit = Arc::new(ExitEntry {
         port:     proxy_addr.port(),
         user:     "u".into(),
         password: "p".into(),
         bind_ip:  IpAddr::V4(Ipv4Addr::LOCALHOST),
         via:      Some(mock_mullvad_addr),
      });
      let handle_task = tokio::spawn(async move {
         if let Ok((stream, _)) = proxy_listener.accept().await {
            handle(stream, exit).await;
         }
      });

      let mut ctl = TcpStream::connect(proxy_addr).await.unwrap();
      ctl.write_all(&[5, 1, 2]).await.unwrap();
      let mut resp = [0_u8; 2];
      ctl.read_exact(&mut resp).await.unwrap();
      assert_eq!(resp, [5, 2]);
      ctl.write_all(&[1, 1, b'u', 1, b'p']).await.unwrap();
      ctl.read_exact(&mut resp).await.unwrap();
      assert_eq!(resp, [1, 0]);

      ctl.write_all(&[5, 3, 0, 1, 0, 0, 0, 0, 0, 0])
         .await
         .unwrap();
      let mut assoc_resp = [0_u8; 10];
      ctl.read_exact(&mut assoc_resp).await.unwrap();
      assert_eq!(&assoc_resp[..4], &[5, 0, 0, 1]);
      let relay_addr = SocketAddr::from((
         Ipv4Addr::new(assoc_resp[4], assoc_resp[5], assoc_resp[6], assoc_resp[7]),
         u16::from_be_bytes([assoc_resp[8], assoc_resp[9]]),
      ));

      let client_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
      let quic_payload = vec![0xCD_u8; 1250];
      let mut pkt = vec![0_u8; 10 + quic_payload.len()];
      let pkt_len = encode_udp_datagram(echo_addr, &quic_payload, &mut pkt).unwrap();
      client_udp
         .send_to(&pkt[..pkt_len], relay_addr)
         .await
         .unwrap();

      let mut recv_buf = vec![0_u8; 4096];
      let (n, _) = timeout(Duration::from_secs(2), client_udp.recv_from(&mut recv_buf))
         .await
         .unwrap()
         .unwrap();
      let (src_dest, payload) = parse_udp_datagram(&recv_buf[..n]).unwrap();
      assert!(matches!(src_dest, UdpDest::Addr(addr) if addr == echo_addr));
      assert_eq!(payload, quic_payload.as_slice());

      drop(ctl);
      timeout(Duration::from_secs(2), handle_task)
         .await
         .unwrap()
         .unwrap();
   }

   #[tokio::test]
   async fn rejects_unsupported_command_with_rep_7() {
      let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
      let proxy_addr = proxy_listener.local_addr().unwrap();
      let exit = Arc::new(ExitEntry {
         port:     proxy_addr.port(),
         user:     "u".into(),
         password: "p".into(),
         bind_ip:  IpAddr::V4(Ipv4Addr::LOCALHOST),
         via:      None,
      });
      tokio::spawn(async move {
         if let Ok((stream, _)) = proxy_listener.accept().await {
            handle(stream, exit).await;
         }
      });

      let mut ctl = TcpStream::connect(proxy_addr).await.unwrap();
      ctl.write_all(&[5, 1, 2]).await.unwrap();
      let mut resp = [0_u8; 2];
      ctl.read_exact(&mut resp).await.unwrap();
      ctl.write_all(&[1, 1, b'u', 1, b'p']).await.unwrap();
      ctl.read_exact(&mut resp).await.unwrap();

      // CMD=2 (BIND) is unsupported.
      ctl.write_all(&[5, 2, 0, 1, 0, 0, 0, 0, 0, 0])
         .await
         .unwrap();
      let mut reply = [0_u8; 10];
      ctl.read_exact(&mut reply).await.unwrap();
      assert_eq!(reply[1], 7);
   }
}
