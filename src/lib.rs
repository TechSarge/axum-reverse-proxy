//! A flexible and efficient reverse proxy implementation for Axum web applications.
//!
//! This crate provides a reverse proxy that can be easily integrated into Axum applications,
//! allowing for seamless forwarding of HTTP requests to upstream servers. It supports:
//!
//! - Path-based routing
//! - Automatic retry mechanism
//! - Header forwarding
//! - Configurable HTTP client settings
//! - Easy integration with Axum's Router
//!
//! # Example
//!
//! ```rust
//! use axum::Router;
//! use axum_reverse_proxy::ReverseProxy;
//!
//! // Create a reverse proxy that forwards requests from /api to httpbin.org
//! let proxy = ReverseProxy::new("/api", "https://httpbin.org");
//!
//! // Convert the proxy to a router and use it in your Axum application
//! let app: Router = proxy.into();
//! ```
//!
//!  You can also merge the proxy with an existing router, compatible with arbitrary state:
//!
//! ```rust
//! use axum::{routing::get, Router, response::IntoResponse, extract::State};
//! use axum_reverse_proxy::ReverseProxy;
//!
//! #[derive(Clone)]
//! struct AppState { foo: usize }
//!
//! async fn root_handler(State(state): State<AppState>) -> impl IntoResponse {
//!     (axum::http::StatusCode::OK, format!("Hello, World! {}", state.foo))
//! }
//!
//! let app: Router<AppState> = Router::new()
//!     .route("/", get(root_handler))
//!     .merge(ReverseProxy::new("/api", "https://httpbin.org"))
//!     .with_state(AppState { foo: 42 });
//! ```

use axum::{body::Body, extract::State, http::Request, response::Response, Router};
use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt};
use hyper::StatusCode;
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::TokioExecutor,
};
use std::convert::Infallible;
use tracing::{error, trace};

/// Configuration options for the reverse proxy
#[derive(Clone, Debug, Default)]
pub struct ProxyOptions {
    /// Whether to buffer the entire request/response bodies in memory
    /// If false (default), requests and responses will be streamed
    pub buffer_bodies: bool,
}

/// A reverse proxy that forwards HTTP requests to an upstream server.
///
/// The `ReverseProxy` struct handles the forwarding of HTTP requests from a specified path
/// to a target upstream server. It manages its own HTTP client with configurable settings
/// for connection pooling, timeouts, and retries.
#[derive(Clone)]
pub struct ReverseProxy {
    path: String,
    target: String,
    client: Client<HttpConnector, Body>,
    options: ProxyOptions,
}

impl ReverseProxy {
    /// Creates a new `ReverseProxy` instance with default options.
    ///
    /// # Arguments
    ///
    /// * `path` - The base path to match incoming requests against (e.g., "/api")
    /// * `target` - The upstream server URL to forward requests to (e.g., "https://api.example.com")
    ///
    /// # Example
    ///
    /// ```rust
    /// use axum_reverse_proxy::ReverseProxy;
    ///
    /// let proxy = ReverseProxy::new("/api", "https://api.example.com");
    /// ```
    pub fn new<S>(path: S, target: S) -> Self
    where
        S: Into<String>,
    {
        Self::new_with_options(path, target, ProxyOptions::default())
    }

    /// Creates a new `ReverseProxy` instance with custom proxy options and a default HTTP client configuration..
    ///
    /// # Arguments
    ///
    /// * `path` - The base path to match incoming requests against
    /// * `target` - The upstream server URL to forward requests to
    /// * `options` - Custom configuration options for the proxy
    ///
    /// # Example
    ///
    /// ```rust
    /// use axum_reverse_proxy::{ReverseProxy, ProxyOptions};
    ///
    /// let options = ProxyOptions {
    ///     buffer_bodies: true,
    /// };
    /// let proxy = ReverseProxy::new_with_options("/api", "https://api.example.com", options);
    /// ```
    pub fn new_with_options<S>(path: S, target: S, options: ProxyOptions) -> Self
    where
        S: Into<String>,
    {
        let mut connector = HttpConnector::new();
        connector.set_nodelay(true);
        connector.enforce_http(false);
        connector.set_keepalive(Some(std::time::Duration::from_secs(60)));
        connector.set_connect_timeout(Some(std::time::Duration::from_secs(10)));
        connector.set_reuse_address(true);

        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(std::time::Duration::from_secs(60))
            .pool_max_idle_per_host(32)
            .retry_canceled_requests(true)
            .set_host(true)
            .build(connector);

        Self::new_with_client_and_options(path, target, client, options)
    }

    /// Creates a new `ReverseProxy` instance with a custom HTTP client and default proxy options.
    pub fn new_with_client<S>(path: S, target: S, client: Client<HttpConnector, Body>) -> Self
    where
        S: Into<String>,
    {
        Self {
            path: path.into(),
            target: target.into(),
            client,
            options: ProxyOptions::default(),
        }
    }

    /// Creates a new `ReverseProxy` instance with a custom HTTP client and options.
    ///
    /// This method allows for more fine-grained control over the proxy behavior by accepting
    /// a pre-configured HTTP client.
    ///
    /// # Arguments
    ///
    /// * `path` - The base path to match incoming requests against
    /// * `target` - The upstream server URL to forward requests to
    /// * `client` - A custom-configured HTTP client
    /// * `options` - Custom configuration options for the proxy
    ///
    /// # Example
    ///
    /// ```rust
    /// use axum_reverse_proxy::{ReverseProxy, ProxyOptions};
    /// use hyper_util::client::legacy::{Client, connect::HttpConnector};
    /// use axum::body::Body;
    /// use hyper_util::rt::TokioExecutor;
    ///
    /// let client = Client::builder(TokioExecutor::new())
    ///     .pool_idle_timeout(std::time::Duration::from_secs(120))
    ///     .build(HttpConnector::new());
    ///
    /// let proxy = ReverseProxy::new_with_client(
    ///     "/api",
    ///     "https://api.example.com",
    ///     client,
    /// );
    /// ```
    pub fn new_with_client_and_options<S>(
        path: S,
        target: S,
        client: Client<HttpConnector, Body>,
        options: ProxyOptions,
    ) -> Self
    where
        S: Into<String>,
    {
        Self {
            path: path.into(),
            target: target.into(),
            client,
            options,
        }
    }

    /// Handles the proxying of a single request to the upstream server.
    async fn proxy_request(&self, mut req: Request<Body>) -> Result<Response<Body>, Infallible> {
        trace!("Proxying request method={} uri={}", req.method(), req.uri());
        trace!("Original headers headers={:?}", req.headers());

        let mut retries: u32 = 3;
        let mut error_msg;
        let mut buffered_body: Option<Bytes> = None;

        if self.options.buffer_bodies {
            // If we're in buffered mode, collect the body once at the start
            let (parts, body) = req.into_parts();
            buffered_body = match body.collect().await {
                Ok(collected) => Some(collected.to_bytes()),
                Err(e) => {
                    error!("Failed to read request body: {}", e);
                    return Ok(Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(Body::empty())
                        .unwrap());
                }
            };
            trace!(
                "Request body collected body_length={}",
                buffered_body.as_ref().unwrap().len()
            );

            // Reconstruct the request with the buffered body
            req = Request::from_parts(parts, Body::from(buffered_body.as_ref().unwrap().clone()));
        }

        loop {
            let forward_req = {
                let mut builder = Request::builder().method(req.method().clone()).uri(format!(
                    "{}{}",
                    self.target,
                    req.uri().path_and_query().map(|x| x.as_str()).unwrap_or("")
                ));

                // Forward headers
                for (key, value) in req.headers() {
                    if key != "host" {
                        builder = builder.header(key, value);
                    }
                }

                // Create the request body
                if self.options.buffer_bodies {
                    builder
                        .body(Body::from(buffered_body.as_ref().unwrap().clone()))
                        .unwrap()
                } else {
                    // For streaming mode, we take ownership of the body
                    let (parts, body) = req.into_parts();
                    req = Request::from_parts(parts, Body::empty());
                    builder.body(body).unwrap()
                }
            };

            trace!(
                "Forwarding headers forwarded_headers={:?}",
                forward_req.headers()
            );

            match self.client.request(forward_req).await {
                Ok(res) => {
                    trace!(
                        "Received response status={} headers={:?} version={:?}",
                        res.status(),
                        res.headers(),
                        res.version()
                    );

                    let (parts, body) = res.into_parts();

                    // Build the response based on buffering mode
                    let response_body = if self.options.buffer_bodies {
                        // Buffered mode: collect the entire response
                        match body.collect().await {
                            Ok(collected) => Body::from(collected.to_bytes()),
                            Err(e) => {
                                error!("Failed to read response body: {}", e);
                                return Ok(Response::builder()
                                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                                    .body(Body::empty())
                                    .unwrap());
                            }
                        }
                    } else {
                        // For streaming mode, convert the incoming body to axum Body
                        Body::new(BoxBody::new(body))
                    };

                    let mut response = Response::builder()
                        .status(parts.status)
                        .body(response_body)
                        .unwrap();

                    *response.headers_mut() = parts.headers;
                    return Ok(response);
                }
                Err(e) => {
                    error_msg = e.to_string();
                    retries = retries.saturating_sub(1);
                    if retries == 0 {
                        error!("Proxy error occurred after all retries err={}", error_msg);
                        return Ok(Response::builder()
                            .status(StatusCode::BAD_GATEWAY)
                            .body(Body::from(format!(
                                "Failed to connect to upstream server: {}",
                                error_msg
                            )))
                            .unwrap());
                    }
                    error!(
                        "Proxy error occurred, retrying ({} left) err={}",
                        retries, error_msg
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            }
        }
    }
}

/// Enables conversion from a `ReverseProxy` into an Axum `Router`.
///
/// This implementation allows the reverse proxy to be easily integrated into an Axum
/// application. It handles:
///
/// - Path-based routing using the configured base path
/// - State management using `Arc` for thread-safety
/// - Fallback handling for all HTTP methods
///
/// # Example
///
/// ```rust
/// use axum::Router;
/// use axum_reverse_proxy::ReverseProxy;
///
/// let proxy = ReverseProxy::new("/api", "https://api.example.com");
/// let app: Router = proxy.into();
/// ```
impl<S> From<ReverseProxy> for Router<S>
where
    S: Send + Sync + Clone + 'static,
{
    fn from(proxy: ReverseProxy) -> Self {
        let path = proxy.path.clone();
        let proxy_router = Router::new()
            .fallback(|State(proxy): State<ReverseProxy>, req| async move {
                proxy.proxy_request(req).await
            })
            .with_state(proxy);

        if ["", "/"].contains(&path.as_str()) {
            proxy_router
        } else {
            Router::new().nest(&path, proxy_router)
        }
    }
}
