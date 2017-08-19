use std::io;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::sync::Arc;
use std::thread;

use {KcpListener, ServerKcpStream};
use futures::future::{Future, Then};
use futures::stream::Stream;
use net2;
use tokio_core::reactor::{Core, Handle};
use tokio_proto::BindServer;
use tokio_service::{NewService, Service};

/// A builder for TCP servers.
///
/// Setting up a server needs, at minimum:
///
/// - A server protocol implementation
/// - An address
/// - A service to provide
///
pub struct KcpServer<Kind, P> {
    _kind: PhantomData<Kind>,
    proto: Arc<P>,
    addr: SocketAddr,
}

impl<Kind, P> KcpServer<Kind, P>
where
    P: BindServer<Kind, ServerKcpStream> + Send + Sync + 'static,
{
    /// Starts building a server for the given protocol and address, with
    /// default configuration.
    ///
    /// Generally, a protocol is implemented *not* by implementing the
    /// `BindServer` trait directly, but instead by implementing one of the
    /// protocol traits:
    ///
    /// - `pipeline::ServerProto`
    /// - `multiplex::ServerProto`
    /// - `streaming::pipeline::ServerProto`
    /// - `streaming::multiplex::ServerProto`
    ///
    /// See the crate documentation for more details on those traits.
    pub fn new(protocol: P, addr: SocketAddr) -> KcpServer<Kind, P> {
        KcpServer {
            _kind: PhantomData,
            proto: Arc::new(protocol),
            addr: addr,
        }
    }

    /// Set the address for the server.
    pub fn addr(&mut self, addr: SocketAddr) {
        self.addr = addr;
    }

    /// Start up the server, providing the given service on it.
    ///
    /// This method will block the current thread until the server is shut down.
    pub fn serve<S>(&self, new_service: S)
    where
        S: NewService + Send + Sync + 'static,
        S::Instance: 'static,
        P::ServiceError: 'static,
        P::ServiceResponse: 'static,
        P::ServiceRequest: 'static,
        S::Request: From<P::ServiceRequest>,
        S::Response: Into<P::ServiceResponse>,
        S::Error: Into<P::ServiceError>,
    {
        let new_service = Arc::new(new_service);
        self.with_handle(move |_| new_service.clone())
    }

    /// Start up the server, providing the given service on it, and providing
    /// access to the event loop handle.
    ///
    /// The `new_service` argument is a closure that is given an event loop
    /// handle, and produces a value implementing `NewService`. That value is in
    /// turn used to make a new service instance for each incoming connection.
    ///
    /// This method will block the current thread until the server is shut down.
    pub fn with_handle<F, S>(&self, new_service: F)
    where
        F: Fn(&Handle) -> S + Send + Sync + 'static,
        S: NewService + Send + Sync + 'static,
        S::Instance: 'static,
        P::ServiceError: 'static,
        P::ServiceResponse: 'static,
        P::ServiceRequest: 'static,
        S::Request: From<P::ServiceRequest>,
        S::Response: Into<P::ServiceResponse>,
        S::Error: Into<P::ServiceError>,
    {
        let proto = self.proto.clone();
        let new_service = Arc::new(new_service);
        let addr = self.addr;
        let thread = {
            let proto = proto.clone();
            let new_service = new_service.clone();
            thread::Builder::new()
                .name("kcp server's worker".into())
                .spawn(move || serve(proto, addr, &*new_service))
                .unwrap()
        };

        serve(proto, addr, &*new_service);

        thread.join().unwrap();
    }
}

fn serve<P, Kind, F, S>(binder: Arc<P>, addr: SocketAddr, new_service: &F)
where
    P: BindServer<Kind, ServerKcpStream>,
    F: Fn(&Handle) -> S,
    S: NewService + Send + Sync,
    S::Instance: 'static,
    P::ServiceError: 'static,
    P::ServiceResponse: 'static,
    P::ServiceRequest: 'static,
    S::Request: From<P::ServiceRequest>,
    S::Response: Into<P::ServiceResponse>,
    S::Error: Into<P::ServiceError>,
{
    struct WrapService<S, Request, Response, Error> {
        inner: S,
        _marker: PhantomData<fn() -> (Request, Response, Error)>,
    }

    impl<S, Request, Response, Error> Service for WrapService<S, Request, Response, Error>
    where
        S: Service,
        S::Request: From<Request>,
        S::Response: Into<Response>,
        S::Error: Into<Error>,
    {
        type Request = Request;
        type Response = Response;
        type Error = Error;
        type Future = Then<
            S::Future,
            Result<Response, Error>,
            fn(Result<S::Response, S::Error>) -> Result<Response, Error>,
        >;

        fn call(&self, req: Request) -> Self::Future {
            fn change_types<A, B, C, D>(r: Result<A, B>) -> Result<C, D>
            where
                A: Into<C>,
                B: Into<D>,
            {
                match r {
                    Ok(e) => Ok(e.into()),
                    Err(e) => Err(e.into()),
                }
            }

            self.inner.call(S::Request::from(req)).then(change_types)
        }
    }

    let mut core = Core::new().unwrap();
    let handle = core.handle();
    let new_service = new_service(&handle);
    let listener = listener(&addr, &handle).unwrap();

    let server = listener.incoming().for_each(move |(socket, _)| {
        let service = new_service.new_service()?;
        binder.bind_server(&handle,
                           socket,
                           WrapService {
                               inner: service,
                               _marker: PhantomData,
                           });

        Ok(())
    });

    core.run(server).unwrap();
}

fn listener(addr: &SocketAddr, handle: &Handle) -> io::Result<KcpListener> {
    let udp = match *addr {
        SocketAddr::V4(_) => net2::UdpBuilder::new_v4()?,
        SocketAddr::V6(_) => net2::UdpBuilder::new_v6()?,
    };
    udp.reuse_address(true)?;
    udp.bind(addr)
       .and_then(|udp| KcpListener::from_std_udp(udp, handle))

}
