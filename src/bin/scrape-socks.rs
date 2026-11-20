//! High-performance local SOCKS5 (username/password) forwarder bound to
//! WireGuard scrape tunnel addresses.
//!
//! Drop-in replacement for `/usr/local/sbin/scrape-socks.py`: reads the same
//! `<port> <user> <password> <bind_ip> [via]` map file and listens on
//! `127.0.0.1` plus any addresses in `SCRAPE_SOCKS_LISTEN`.

use std::{
   env,
   fs,
   io,
   net::{
      IpAddr,
      Ipv4Addr,
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
      lookup_host,
   },
   time::{
      sleep,
      timeout,
   },
};

const LISTEN_BACKLOG: u32 = 1024;
const DEFAULT_LISTEN: &str = "127.0.0.1";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const PIPE_BUF_BYTES: usize = 65_536;
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
      TargetAddr::V6(octets) => SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::from(*octets)), port),
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
         Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
         Ok(Ok(n)) => {
            if writer.write_all(&buf[..n]).await.is_err() {
               break;
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
   if req[0] != 5 || req[1] != 1 {
      let _ = client.write_all(&[5, 7, 0, 1, 0, 0, 0, 0, 0, 0]).await;
      abort_socket(client);
      return;
   }

   let target = match req[3] {
      1 => {
         let mut buf = [0_u8; 4];
         if client.read_exact(&mut buf).await.is_err() {
            abort_socket(client);
            return;
         }
         TargetAddr::V4(buf)
      },
      3 => {
         let mut len = [0_u8; 1];
         if client.read_exact(&mut len).await.is_err() {
            abort_socket(client);
            return;
         }
         let mut host = vec![0_u8; usize::from(len[0])];
         if client.read_exact(&mut host).await.is_err() {
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
         if client.read_exact(&mut buf).await.is_err() {
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
   if client.read_exact(&mut port_buf).await.is_err() {
      abort_socket(client);
      return;
   }
   let port = u16::from_be_bytes(port_buf);

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
         # comment\n\
         1081 alice secret1 10.137.253.125\n\
         1086 bob secret2 10.137.253.125 via # trailing\n";
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
}
