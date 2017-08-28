// Copyright 2017 Amagicom AB.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.


#[macro_use]
extern crate error_chain;
extern crate futures;
extern crate hyper;
extern crate jsonrpc_client_core;
#[macro_use]
extern crate log;
extern crate tokio_core;

#[cfg(feature = "tls")]
extern crate hyper_tls;
#[cfg(feature = "tls")]
extern crate native_tls;

use futures::{future, BoxFuture, Future, Stream};
use futures::sync::{mpsc, oneshot};

use hyper::{Client, Request, StatusCode, Uri};
use hyper::client::HttpConnector;

#[cfg(feature = "tls")]
use hyper_tls::HttpsConnector;

use jsonrpc_client_core::Transport;

use std::io;
use std::marker::PhantomData;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use tokio_core::reactor::{Core, Handle};

mod client_builder;
pub use client_builder::*;

error_chain! {
    errors {
        /// When there was an error creating the Hyper `Client` from the given builder.
        ClientBuilderError {
            description("Failed to create the Hyper Client")
        }
        /// When the http status code of the response is not 200 OK
        HttpError(http_code: StatusCode) {
            description("Http error. Server did not return 200 OK")
            display("Http error. Status code {}", http_code)
        }
        /// When there was an error in the Tokio Core.
        TokioCoreError(msg: &'static str) {
            description("Error with the Tokio Core")
            display("Error with the Tokio Core: {}", msg)
        }
    }
    foreign_links {
        Hyper(hyper::Error);
        Uri(hyper::error::UriError);
    }
}


/// Builder struct for `HttpTransport`. Created from static metods on `HttpTransport`.
#[derive(Debug)]
pub struct HttpTransportBuilder<C, E, CB>
where
    C: hyper::client::Connect,
    E: ::std::error::Error + Send + 'static,
    CB: ClientBuilder<C, E>,
{
    client_builder: CB,
    _connect_marker: PhantomData<C>,
    _error_marker: PhantomData<E>,

    handle: Option<Handle>,
}

impl<C, E, CB> HttpTransportBuilder<C, E, CB>
where
    C: hyper::client::Connect,
    E: ::std::error::Error + Send + 'static,
    CB: ClientBuilder<C, E>,
{
    /// Change how the Hyper `Client` is created.
    pub fn client<C2, E2, CB2>(self, builder: CB2) -> HttpTransportBuilder<C2, E2, CB2>
    where
        C2: hyper::client::Connect,
        E2: ::std::error::Error + Send + 'static,
        CB2: ClientBuilder<C2, E2>,
    {
        HttpTransportBuilder {
            client_builder: builder,
            _connect_marker: PhantomData,
            _error_marker: PhantomData,
            handle: self.handle,
        }
    }

    /// Sets which Tokio `Handle`, and thus which `Core` to run the resulting `HttpTransport` on.
    ///
    /// If this method is not called, the default is to spawn a standalone Tokio `Core` in a
    /// separate thread. The thread and the event loop will run for as long as the
    /// returned `HttpTransport`, or any `HttpHandle` to it, exists.
    pub fn handle(mut self, handle: Handle) -> Self {
        self.handle = Some(handle);
        self
    }

    pub fn build(self) -> Result<HttpTransport> {
        if let Some(handle) = self.handle {
            let client = self.client_builder
                .build(&handle)
                .chain_err(|| ErrorKind::ClientBuilderError)?;
            let (request_tx, request_rx) = mpsc::unbounded();
            handle.spawn(Self::create_request_processing_future(request_rx, client));
            Ok(HttpTransport::new(request_tx))
        } else {
            let (tx, rx) = ::std::sync::mpsc::channel();
            let client_builder = self.client_builder;
            thread::spawn(move || {
                match Self::create_standalone_core(client_builder) {
                    Err(e) => {
                        tx.send(Err(e)).unwrap();
                    }
                    Ok((mut core, request_tx, future)) => {
                        tx.send(Ok(HttpTransport::new(request_tx))).unwrap();
                        let _ = core.run(future);
                    }
                }
                debug!("Standalone HttpTransport thread exiting");
            });

            rx.recv().unwrap()
        }
    }

    /// Creates all the components needed to run the `HttpTransport` in standalone mode.
    fn create_standalone_core(
        client_builder: CB,
    ) -> Result<(Core, CoreSender, Box<Future<Item = (), Error = ()>>)> {
        let core = Core::new().chain_err(|| ErrorKind::TokioCoreError("Unable to create"))?;
        let client = client_builder
            .build(&core.handle())
            .chain_err(|| ErrorKind::ClientBuilderError)?;
        let (request_tx, request_rx) = mpsc::unbounded();
        let future = Self::create_request_processing_future(request_rx, client);
        Ok((core, request_tx, future))
    }

    /// Creates the `Future` that, when running on a Tokio Core, processes incoming RPC call
    /// requests.
    fn create_request_processing_future(
        request_rx: CoreReceiver,
        client: Client<C, hyper::Body>,
    ) -> Box<Future<Item = (), Error = ()>> {
        let f = request_rx.for_each(move |(request, response_tx)| {
            client
                .request(request)
                .from_err()
                .and_then(|response: hyper::Response| {
                    if response.status() == hyper::StatusCode::Ok {
                        future::ok(response)
                    } else {
                        future::err(ErrorKind::HttpError(response.status()).into())
                    }
                })
                .and_then(|response: hyper::Response| {
                    response.body().concat2().from_err()
                })
                .map(|response_chunk| response_chunk.to_vec())
                .then(move |response_result| {
                    response_tx.send(response_result).map_err(|_| {
                        warn!("Unable to send response back to caller");
                        ()
                    })
                })
        });
        Box::new(f) as Box<Future<Item = (), Error = ()>>
    }
}

type CoreSender = mpsc::UnboundedSender<(Request, oneshot::Sender<Result<Vec<u8>>>)>;
type CoreReceiver = mpsc::UnboundedReceiver<(Request, oneshot::Sender<Result<Vec<u8>>>)>;

/// The main struct of the HTTP transport implementation for `jsonrpc-client-core`.
///
/// Created with the `HttpTransportBuilder` builder.
#[derive(Debug, Clone)]
pub struct HttpTransport {
    request_tx: CoreSender,
    id: Arc<AtomicUsize>,
}

impl HttpTransport {
    /// Returns the default builder that can be configured and then used to create a
    /// `HttpTransport` instance.
    pub fn builder() -> HttpTransportBuilder<HttpConnector, io::Error, DefaultClientBuilder> {
        HttpTransportBuilder {
            client_builder: DefaultClientBuilder,
            _connect_marker: PhantomData,
            _error_marker: PhantomData,
            handle: None,
        }
    }

    #[cfg(feature = "tls")]
    /// Returns a builder with TLS enabled from the start.
    pub fn tls_builder() -> HttpTransportBuilder<
        HttpsConnector<HttpConnector>,
        native_tls::Error,
        DefaultTlsClientBuilder,
    > {
        HttpTransportBuilder {
            client_builder: DefaultTlsClientBuilder,
            _connect_marker: PhantomData,
            _error_marker: PhantomData,
            handle: None,
        }
    }

    fn new(request_tx: CoreSender) -> Self {
        HttpTransport {
            request_tx,
            id: Arc::new(AtomicUsize::new(1)),
        }
    }

    /// Returns a handle to this `HttpTransport` valid for a given URI.
    ///
    /// Used to create instances implementing `jsonrpc_client_core::Transport` for use with RPC
    /// clients.
    pub fn handle(&self, uri: &str) -> Result<HttpHandle> {
        let uri = Uri::from_str(uri)?;
        Ok(HttpHandle {
            request_tx: self.request_tx.clone(),
            uri,
            id: self.id.clone(),
        })
    }
}

/// A handle to a `HttpTransport`. This implements `jsonrpc_client_core::Transport` and can be used
/// as the transport object for an RPC client generated by `jsonrpc_client_core`.
#[derive(Debug, Clone)]
pub struct HttpHandle {
    request_tx: CoreSender,
    uri: Uri,
    id: Arc<AtomicUsize>,
}

impl HttpHandle {
    /// Creates a Hyper POST request with JSON content type and the given body data.
    fn create_request(&self, body: Vec<u8>) -> Request {
        let mut request = hyper::Request::new(hyper::Method::Post, self.uri.clone());
        request
            .headers_mut()
            .set(hyper::header::ContentType::json());
        request
            .headers_mut()
            .set(hyper::header::ContentLength(body.len() as u64));
        request.set_body(body);
        request
    }
}

impl Transport<Error> for HttpHandle {
    fn get_next_id(&mut self) -> u64 {
        self.id.fetch_add(1, Ordering::SeqCst) as u64
    }

    fn send(&self, json_data: Vec<u8>) -> BoxFuture<Vec<u8>, Error> {
        let request = self.create_request(json_data.clone());
        let (response_tx, response_rx) = oneshot::channel();
        future::result(mpsc::UnboundedSender::send(
            &self.request_tx,
            (request, response_tx),
        )).map_err(|e| {
            Error::with_chain(e, ErrorKind::TokioCoreError("Not listening for requests"))
        })
            .and_then(move |_| {
                response_rx.map_err(|e| {
                    Error::with_chain(
                        e,
                        ErrorKind::TokioCoreError("Died without returning response"),
                    )
                })
            })
            .and_then(future::result)
            .boxed()
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_accept_handle() {
        let core = Core::new().unwrap();
        HttpTransport::builder()
            .handle(core.handle())
            .build()
            .unwrap();
    }

    #[test]
    fn builder_no_handle() {
        HttpTransport::builder().build().unwrap();
    }

    #[test]
    fn builder_closure_client_builder() {
        HttpTransport::builder()
            .client(|handle: &Handle| {
                Ok(Client::new(handle)) as Result<Client<HttpConnector, hyper::Body>>
            })
            .build()
            .unwrap();
    }

    #[test]
    fn builder_client_builder_fails() {
        let error = HttpTransport::builder()
            .client(|_: &Handle| {
                Err(io::Error::new(io::ErrorKind::Other, "Dummy error")) as
                    ::std::result::Result<Client<HttpConnector, hyper::Body>, io::Error>
            })
            .build()
            .unwrap_err();
        match error.kind() {
            &ErrorKind::ClientBuilderError => (),
            kind => panic!("invalid error kind response: {:?}", kind),
        }
    }
}
