use std::{
   io::Read as _,
   time::Duration,
};

use axum::http::{
   HeaderMap,
   Method,
   Uri,
   header,
};
use bytes::Bytes;
use flate2::read::GzDecoder;
use http_body_util::{
   BodyExt as _,
   Empty,
};
use hyper::{
   StatusCode,
   body as hyper_body,
};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::{
   client::legacy::{
      Client,
      connect::HttpConnector,
   },
   rt::TokioExecutor,
};
use serde::de::DeserializeOwned;

use crate::error::{
   Error,
   Result,
};

type Connector = hyper_rustls::HttpsConnector<HttpConnector>;

/// Lightweight HTTP client wrapping hyper-util's connection-pooling client.
#[derive(Clone)]
#[expect(
   clippy::module_name_repetitions,
   reason = "HttpClient is clearer than Client"
)]
pub struct HttpClient {
   inner:           Client<Connector, Empty<Bytes>>,
   default_headers: HeaderMap,
}

/// Response wrapper providing convenience methods.
pub struct Response {
   status:  StatusCode,
   headers: HeaderMap,
   body:    hyper_body::Incoming,
}

impl HttpClient {
   pub fn new() -> Self {
      let connector = HttpsConnectorBuilder::new()
         .with_native_roots()
         .expect("native TLS roots")
         .https_or_http()
         .enable_http1()
         .build();

      let inner = Client::builder(TokioExecutor::new())
         .pool_idle_timeout(Duration::from_secs(90))
         .build(connector);

      Self {
         inner,
         default_headers: HeaderMap::new(),
      }
   }

   /// Create a client with default headers applied to every request.
   pub fn with_default_headers(mut self, headers: HeaderMap) -> Self {
      self.default_headers = headers;
      self
   }

   /// Send a request with a given method and optional extra headers.
   async fn send(&self, method: Method, uri: &str, extra_headers: &HeaderMap) -> Result<Response> {
      let parsed: Uri = uri
         .parse()
         .map_err(|err| Error::Internal(format!("invalid URI: {err}")))?;

      let mut builder = hyper::Request::builder().method(method).uri(parsed);

      for (key, value) in &self.default_headers {
         builder = builder.header(key, value);
      }
      for (key, value) in extra_headers {
         builder = builder.header(key, value);
      }

      let request = builder
         .body(Empty::new())
         .map_err(|err| Error::Internal(format!("build request: {err}")))?;

      let resp = self
         .inner
         .request(request)
         .await
         .map_err(|err| Error::Internal(format!("HTTP request failed: {err}")))?;

      let (parts, body) = resp.into_parts();
      Ok(Response {
         status: parts.status,
         headers: parts.headers,
         body,
      })
   }

   /// Send a GET request.
   pub async fn get(&self, uri: &str) -> Result<Response> {
      self.send(Method::GET, uri, &HeaderMap::new()).await
   }

   /// Send a GET request with additional headers.
   pub async fn get_with_headers(&self, uri: &str, extra_headers: &HeaderMap) -> Result<Response> {
      self.send(Method::GET, uri, extra_headers).await
   }

   /// Send a HEAD request.
   pub async fn head(&self, uri: &str) -> Result<Response> {
      self.send(Method::HEAD, uri, &HeaderMap::new()).await
   }
}

impl Response {
   pub const fn status(&self) -> StatusCode {
      self.status
   }

   pub const fn headers(&self) -> &HeaderMap {
      &self.headers
   }

   /// Collect the response body as bytes, decompressing gzip if needed.
   pub async fn bytes(self) -> Result<Bytes> {
      let is_gzip = self
         .headers
         .get(header::CONTENT_ENCODING)
         .and_then(|val| val.to_str().ok())
         .is_some_and(|val| val.contains("gzip"));

      let collected = self
         .body
         .collect()
         .await
         .map_err(|err| Error::Internal(format!("read body: {err}")))?
         .to_bytes();

      if is_gzip {
         let mut gz = GzDecoder::new(&*collected);
         let mut decoded = Vec::new();
         gz.read_to_end(&mut decoded)
            .map_err(|err| Error::Internal(format!("gzip decode: {err}")))?;
         Ok(Bytes::from(decoded))
      } else {
         Ok(collected)
      }
   }

   /// Collect the response body as a UTF-8 string.
   pub async fn text(self) -> Result<String> {
      let data = self.bytes().await?;
      String::from_utf8(data.to_vec())
         .map_err(|err| Error::Internal(format!("invalid UTF-8: {err}")))
   }

   /// Deserialize the response body as JSON.
   pub async fn json<T: DeserializeOwned>(self) -> Result<T> {
      let data = self.bytes().await?;
      serde_json::from_slice(&data).map_err(Into::into)
   }
}
