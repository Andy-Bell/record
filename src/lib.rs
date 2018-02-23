#[macro_use] extern crate serde_derive;

extern crate curl;
extern crate futures;
extern crate hyper;
extern crate hyper_tls;
extern crate serde;
extern crate serde_json;
extern crate tokio_core;
extern crate tokio_service;

use std::cell::RefCell;
use std::collections::HashSet;
use std::env;
use std::fs::File;
use std::io::prelude::*;
use std::io;
use std::net;
use std::path::PathBuf;
use std::rc::Rc;
use std::str;
use std::sync::{Arc, Mutex};
use std::thread;

use self::futures::{Future, Stream};
use self::futures::sync::oneshot;
use self::hyper::server::Http;
use self::tokio_core::net::TcpListener;
use self::tokio_core::reactor::Core;
use self::tokio_service::Service;

macro_rules! t {
    ($e:expr) => (
        match $e {
            Ok(e) => e,
            Err(m) => panic!("{} failed with: {}", stringify!($e), m),
        }
    )
}

// A "bomb" so when the test task exists we know when to shut down
// the server and fail if the subtask failed.
pub struct Bomb {
    iorx: Sink,
    quittx: Option<oneshot::Sender<()>>,
    #[cfg_attr(feature = "cargo-clippy", allow(type_complexity))]
    thread: Option<thread::JoinHandle<Option<(Vec<u8>, PathBuf)>>>,
}

#[derive(Clone)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl<'a> Write for &'a Sink {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        Write::write(&mut *self.0.lock().unwrap(), data)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Bomb {
    fn drop(&mut self) {
        drop(self.quittx.take());
        let res = self.thread.take().unwrap().join();
        let stderr = str::from_utf8(&self.iorx.0.lock().unwrap())
            .unwrap()
            .to_string();
        match res {
            Err(..) if !thread::panicking() => panic!("server subtask failed: {}", stderr),
            Err(e) => if !stderr.is_empty() {
                println!("server subtask failed ({:?}): {}", e, stderr)
            },
            Ok(_) if thread::panicking() => {}
            Ok(None) => {}
            Ok(Some((data, file))) => {
                t!(t!(File::create(&file)).write_all(&data));
            }
        }
    }
}

fn cache_file(name: &str) -> PathBuf {
    PathBuf::from(file!())
        .parent()
        .unwrap()
        .join("http-data")
        .join(name)
}

enum Record {
    Capture(Vec<Exchange>, PathBuf),
    Replay(Vec<Exchange>),
}

pub fn proxy() -> (String, Bomb) {
    let me = thread::current().name().unwrap().to_string();
    let record = env::var("RECORD").is_ok();

    let a = t!(net::TcpListener::bind("127.0.0.1:0"));
    let ret = format!("http://{}", t!(a.local_addr()));

    let data = cache_file(&me.replace("::", "_"));
    let record = if record && !data.exists() {
        Record::Capture(Vec::new(), data)
    } else if !data.exists() {
        Record::Replay(serde_json::from_slice(b"[]").unwrap())
    } else {
        let mut body = Vec::new();
        t!(t!(File::open(&data)).read_to_end(&mut body));
        Record::Replay(serde_json::from_slice(&body).unwrap())
    };

    let sink = Arc::new(Mutex::new(Vec::new()));
    let sink2 = Sink(Arc::clone(&sink));

    let (quittx, quitrx) = oneshot::channel();

    let thread = thread::spawn(move || {
        let mut core = t!(Core::new());
        let handle = core.handle();
        let addr = t!(a.local_addr());
        let listener = t!(TcpListener::from_listener(a, &addr, &handle));
        let client = hyper::Client::configure()
            .connector(hyper_tls::HttpsConnector::new(4, &handle).unwrap())
            .build(&handle);

        let record = Rc::new(RefCell::new(record));
        let srv = listener.incoming().for_each(|(socket, addr)| {
            Http::new().bind_connection(
                &handle,
                socket,
                addr,
                Proxy {
                    sink: sink2.clone(),
                    record: Rc::clone(&record),
                    client: client.clone(),
                },
                );
            Ok(())
        });
        drop(core.run(srv.select2(quitrx)));

        let record = record.borrow();
        match *record {
            Record::Capture(ref data, ref path) => {
                let data = t!(serde_json::to_string(data));
                Some((data.into_bytes(), path.clone()))
            }
            Record::Replay(..) => None,
        }
    });

    (
        ret,
        Bomb {
            iorx: Sink(sink),
            quittx: Some(quittx),
            thread: Some(thread),
        },
        )
}

struct Proxy {
    sink: Sink,
    record: Rc<RefCell<Record>>,
    client: Client,
}

impl Service for Proxy {
    type Request = hyper::Request;
    type Response = hyper::Response;
    type Error = hyper::Error;
    type Future = Box<Future<Item = hyper::Response, Error = hyper::Error>>;

    fn call(&self, req: hyper::Request) -> Self::Future {
        match *self.record.borrow_mut() {
            Record::Capture(_, _) => {
                let record = Rc::clone(&self.record);
                Box::new(
                    record_http(req, &self.client).map(move |(response, exchange)| {
                        if let Record::Capture(ref mut d, _) = *record.borrow_mut() {
                            d.push(exchange);
                        }
                        response
                    }),
                    )
            }
            Record::Replay(ref mut exchanges) => {
                replay_http(req, exchanges.remove(0), &mut &self.sink)
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Exchange {
    request: Request,
    response: Response,
}

#[derive(Serialize, Deserialize)]
struct Request {
    uri: String,
    method: String,
    headers: HashSet<(String, String)>,
    body: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
struct Response {
    status: u16,
    headers: HashSet<(String, String)>,
    body: Vec<u8>,
}

type Client = hyper::Client<hyper_tls::HttpsConnector<hyper::client::HttpConnector>>;

fn record_http(
    req: hyper::Request,
    client: &Client,
    ) -> Box<Future<Item = (hyper::Response, Exchange), Error = hyper::Error>> {
    let (method, uri, _version, headers, body) = req.deconstruct();

    let mut request = Request {
        uri: uri.to_string(),
        method: method.to_string(),
        headers: headers
            .iter()
            .map(|h| (h.name().to_string(), h.value_string()))
            .collect(),
            body: Vec::new(),
    };
    let body = body.concat2();

    let client = client.clone();
    let response = body.and_then(move |body| {
        request.body = body.to_vec();
        let uri = uri.to_string().replace("http://", "https://");
        let mut req = hyper::Request::new(method, uri.parse().unwrap());
        *req.headers_mut() = headers;
        req.set_body(body);
        client.request(req).map(|r| (r, request))
    });

    Box::new(response.and_then(|(hyper_response, request)| {
        let status = hyper_response.status();
        let headers = hyper_response.headers().clone();
        let mut response = Response {
            status: status.as_u16(),
            headers: headers
                .iter()
                .map(|h| (h.name().to_string(), h.value_string()))
                .collect(),
                body: Vec::new(),
        };

        hyper_response.body().concat2().map(move |body| {
            response.body = body.to_vec();
            let mut hyper_response = hyper::Response::new();
            hyper_response.set_body(body);
            hyper_response.set_status(status);
            *hyper_response.headers_mut() = headers;
            (
                hyper_response,
                Exchange {
                    response: response,
                    request: request,
                },
                )
        })
    }))
}

fn replay_http(
    req: hyper::Request,
    mut exchange: Exchange,
    stdout: &mut Write,
    ) -> Box<Future<Item = hyper::Response, Error = hyper::Error>> {
    assert_eq!(req.uri().to_string(), exchange.request.uri);
    assert_eq!(req.method().to_string(), exchange.request.method);
    t!(writeln!(
            stdout,
            "expecting: {:?}",
            exchange.request.headers
            ));
    for header in req.headers().iter() {
        let pair = (header.name().to_string(), header.value_string());
        t!(writeln!(stdout, "received: {:?}", pair));
        if header.name().starts_with("Date") {
            continue;
        }
        if header.name().starts_with("Authorization") {
            continue;
        }
        if !exchange.request.headers.remove(&pair) {
            panic!("found {:?} but didn't expect it", pair);
        }
    }
    for (name, value) in exchange.request.headers.drain() {
        if name.starts_with("Date") {
            continue;
        }
        if name.starts_with("Authorization") {
            continue;
        }
        panic!("didn't find header {:?}", (name, value));
    }
    let req_body = exchange.request.body;
    let verify_body = req.body().concat2().map(move |body| {
        assert_eq!(&body[..], &req_body[..]);
    });

    let mut response = hyper::Response::new();
    response.set_status(hyper::StatusCode::try_from(exchange.response.status).unwrap());
    for (key, value) in exchange.response.headers {
        response.headers_mut().append_raw(key, value);
    }
    response.set_body(exchange.response.body);

    Box::new(verify_body.map(|()| response))
}

