use std::{net::SocketAddr, sync::Arc};

use criterion::*;
use futures::future::join_all;
use hyper::client::HttpConnector;
use hyper::{body::HttpBody as _, Body, Method, Request, Response, Server};

criterion_main!(benches);
criterion_group!(benches, http1);

fn http1(c: &mut Criterion) {
    let mut group = c.benchmark_group("hyper concurrent connections");
    let body = &[b'x'; 1024];
    let response = &[b'r'; 1024];
    let rt = Arc::new(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt build"),
    );
    let exec = rt.clone();
    let opts = opts().request_body(body).response_body(response);
    let addr = spawn_server(&rt, &opts);
    let url: hyper::Uri = format!("http://{}/hello", addr).parse().unwrap();

    let make_request = || {
        let chunk_cnt = opts.request_chunks;
        let body = if chunk_cnt > 0 {
            let (mut tx, body) = Body::channel();
            let chunk = opts.request_body.expect("request_chunks means request_body");
            exec.spawn(async move {
                for _ in 0..chunk_cnt {
                    tx.send_data(chunk.into()).await.expect("send_data");
                }
            });
            body
        } else {
            opts.request_body
                .map(Body::from)
                .unwrap_or_else(Body::empty)
        };
        let mut req = Request::new(body);
        *req.method_mut() = opts.request_method.clone();
        *req.uri_mut() = url.clone();
        req
    };

    let send_request = |client: hyper::Client<HttpConnector>, req: Request<Body>| {
        let fut = client.request(req);
        async {
            let res = fut.await.expect("client wait");
            let mut body = res.into_body();
            while let Some(_chunk) = body.data().await {}
        }
    };

    for conns in [2, 4, 8, 16, 32, 64].iter() {

        group.bench_function(format!("hyper concurrent conn: {}", conns), |b| {
            b.iter_with_setup(|| {
                let clients: Vec<_> = (0..*conns)
                    .map(|_| {
                        let connector = HttpConnector::new();
                        hyper::Client::builder()
                            .http2_only(opts.http2)
                            .http2_initial_stream_window_size(opts.http2_stream_window)
                            .http2_initial_connection_window_size(opts.http2_conn_window)
                            .http2_adaptive_window(opts.http2_adaptive_window)
                            .build::<_, Body>(connector)
                    })
                    .collect();
                clients
            },
            |clients| {
                let futs = clients.into_iter().map(|c| {
                    let req = make_request();
                    send_request(c, req)
                });
                // Await all spawned futures becoming completed.
                rt.block_on(join_all(futs));
            });
        });
    }
}

/// Copied from hyper benches and modified accordingly.
struct Opts {
    http2: bool,
    http2_stream_window: Option<u32>,
    http2_conn_window: Option<u32>,
    http2_adaptive_window: bool,
    request_method: Method,
    request_body: Option<&'static [u8]>,
    request_chunks: usize,
    response_body: &'static [u8],
}

fn opts() -> Opts {
    Opts {
        http2: false,
        http2_stream_window: None,
        http2_conn_window: None,
        http2_adaptive_window: false,
        request_method: Method::GET,
        request_body: None,
        request_chunks: 0,
        response_body: b"",
    }
}

impl Opts {
    fn http2(mut self) -> Self {
        self.http2 = true;
        self
    }

    fn http2_stream_window(mut self, sz: impl Into<Option<u32>>) -> Self {
        assert!(!self.http2_adaptive_window);
        self.http2_stream_window = sz.into();
        self
    }

    fn http2_conn_window(mut self, sz: impl Into<Option<u32>>) -> Self {
        assert!(!self.http2_adaptive_window);
        self.http2_conn_window = sz.into();
        self
    }

    fn http2_adaptive_window(mut self) -> Self {
        assert!(self.http2_stream_window.is_none());
        assert!(self.http2_conn_window.is_none());
        self.http2_adaptive_window = true;
        self
    }

    fn method(mut self, m: Method) -> Self {
        self.request_method = m;
        self
    }

    fn request_body(mut self, body: &'static [u8]) -> Self {
        self.request_body = Some(body);
        self
    }

    fn request_chunks(mut self, chunk: &'static [u8], cnt: usize) -> Self {
        assert!(cnt > 0);
        self.request_body = Some(chunk);
        self.request_chunks = cnt;
        self
    }

    fn response_body(mut self, body: &'static [u8]) -> Self {
        self.response_body = body;
        self
    }
}

fn spawn_server(rt: &tokio::runtime::Runtime, opts: &Opts) -> SocketAddr {
    use hyper::service::{make_service_fn, service_fn};
    let addr = "127.0.0.1:0".parse().unwrap();

    let body = opts.response_body;
    let srv = rt.block_on(async move {
        Server::bind(&addr)
            .http2_only(opts.http2)
            .http2_initial_stream_window_size(opts.http2_stream_window)
            .http2_initial_connection_window_size(opts.http2_conn_window)
            .http2_adaptive_window(opts.http2_adaptive_window)
            .serve(make_service_fn(move |_| async move {
                Ok::<_, hyper::Error>(service_fn(move |req: Request<Body>| async move {
                    let mut req_body = req.into_body();
                    while let Some(_chunk) = req_body.data().await {}
                    Ok::<_, hyper::Error>(Response::new(Body::from(body)))
                }))
            }))
    });
    let addr = srv.local_addr();
    rt.spawn(async {
        if let Err(err) = srv.await {
            panic!("server error: {}", err);
        }
    });
    addr
}
