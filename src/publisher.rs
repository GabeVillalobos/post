use super::find_service;
use super::framing::{Message, MessageCodec, Request};
use super::{DataGram, Error, Generation, PublisherDesc, Result};
use futures::{
    future::Future,
    sync::mpsc::{self, Sender},
    Async, AsyncSink, Poll, Sink, StartSend, Stream,
};
use itertools::Itertools;
use std::collections::HashSet;
use std::iter::FromIterator;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::vec::Vec;
use tokio::net::UdpFramed;

pub struct PublisherSetup {
    desc: PublisherDesc,
    request: find_service::RequestFuture<find_service::BlankResponse>,
}

impl PublisherSetup {
    pub fn new(
        desc: PublisherDesc,
        request: find_service::RequestFuture<find_service::BlankResponse>,
    ) -> PublisherSetup {
        PublisherSetup { desc, request }
    }
}

impl Future for PublisherSetup {
    type Item = Publisher;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        try_ready!(self.request.poll());
        Ok(Async::Ready(Publisher::from_description(
            self.desc.clone(),
        )?))
    }
}

struct PublisherShared {
    subscribers: HashSet<SocketAddr>,
    is_active: bool,
}

pub struct Publisher {
    shared: Arc<Mutex<PublisherShared>>,
    sink: Sender<DataGram>,
    generation: Generation,
    current_send: Option<Vec<DataGram>>,
    in_poll: bool,
}

struct PublisherInternal<T> {
    protocol: T,
    shared: Arc<Mutex<PublisherShared>>,
}

impl<T> PublisherInternal<T> {
    fn new(protocol: T, shared: Arc<Mutex<PublisherShared>>) -> PublisherInternal<T> {
        PublisherInternal { protocol, shared }
    }

    fn is_active(&self) -> bool {
        self.shared.lock().unwrap().is_active
    }
}

impl<T> Stream for PublisherInternal<T>
where
    T: Stream<Item = DataGram, Error = Error>,
{
    type Item = DataGram;
    type Error = Error;
    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        if self.is_active() {
            self.protocol.poll()
        } else {
            Ok(Async::Ready(None))
        }
    }
}

fn handle_publisher_backend(
    shared: Arc<Mutex<PublisherShared>>,
    incomming: DataGram,
) -> Option<DataGram> {
    match incomming.0 {
        Message::Request(r) => match r {
            Request::Subscribe(_) => {
                shared.lock().unwrap().subscribers.insert(incomming.1);
            }
            Request::Unsubscribe(_) => {
                shared.lock().unwrap().subscribers.remove(&incomming.1);
            }
        },
        Message::Data(_) => error!("Data Message sent to publisher from {}", incomming.1),
    };
    None
}

impl Publisher {
    pub fn new(name: String, host_name: String, port: u16, find_uri: &str) -> PublisherSetup {
        let desc = PublisherDesc {
            name,
            host_name,
            port,
        };
        let req = find_service::publisher_register(find_uri, &desc);
        PublisherSetup::new(desc, req)
    }

    fn from_description(desc: PublisherDesc) -> Result<Publisher> {
        let shared = Arc::new(Mutex::new(PublisherShared {
            subscribers: HashSet::new(),
            is_active: true,
        }));
        let (udp_sink, udp_stream) =
            UdpFramed::new(desc.to_tokio_socket()?, MessageCodec {}).split();
        let (sink, stream) = mpsc::channel::<DataGram>(1);

        let reserved_shared = Arc::clone(&shared);
        tokio::spawn(
            PublisherInternal::new(udp_stream, Arc::clone(&shared))
                .filter_map(move |d| handle_publisher_backend(Arc::clone(&reserved_shared), d))
                .forward(sink.clone())
                .map_err(move |e| {
                    error!("Stream Error {}", e);
                })
                .map(|_| ()),
        );
        tokio::spawn(
            stream
                .map_err(|_| Error::Empty)
                .forward(udp_sink)
                .map(|_| ())
                .map_err(|e| {
                    error!("Sink Error {}", e);
                }),
        );

        Ok(Publisher {
            shared,
            sink,
            generation: 1,
            current_send: None,
            in_poll: false,
        })
    }

    fn reset_stream(&mut self) {
        self.in_poll = false;
        self.current_send = None;
    }

    fn do_send(&mut self) -> Poll<(), Error> {
        if self.in_poll {
            let poll_result = self.sink.poll_complete();
            if let Ok(a) = poll_result {
                if let Async::Ready(_) = a {
                    self.in_poll = false;
                }
            }
            Ok(poll_result?)
        } else {
            let current = self.current_send.as_mut().unwrap().pop().unwrap();
            match self.sink.start_send(current)? {
                AsyncSink::Ready => {
                    if self.current_send.as_mut().unwrap().is_empty() {
                        self.current_send = None
                    }
                    Ok(Async::Ready(()))
                }
                AsyncSink::NotReady(c) => {
                    self.current_send.as_mut().unwrap().push(c);
                    Ok(Async::NotReady)
                }
            }
        }
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        self.shared.lock().unwrap().is_active = false;
    }
}

impl Sink for Publisher {
    type SinkItem = Vec<u8>;
    type SinkError = Error;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        if self.current_send.is_some() {
            return Ok(AsyncSink::NotReady(item));
        }

        {
            let shared = self.shared.lock().unwrap();
            self.current_send = Some(Vec::from_iter(
                Message::split_data_msgs(item.as_slice(), self.generation)?
                    .into_iter()
                    .cartesian_product(shared.subscribers.iter().cloned()),
            ));
        }

        match self.do_send() {
            Err(e) => {
                self.reset_stream();
                Err(e)
            }
            Ok(a) => Ok(match a {
                Async::Ready(_) => {
                    self.generation += 1;
                    AsyncSink::Ready
                }
                Async::NotReady => {
                    self.in_poll = true;
                    self.current_send = None;
                    AsyncSink::NotReady(item)
                }
            }),
        }
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        loop {
            try_ready!(match self.do_send() {
                Err(e) => {
                    self.reset_stream();
                    Err(e)
                }
                Ok(a) => Ok(a),
            });
            if self.current_send.is_none() && !self.in_poll {
                return Ok(Async::Ready(()));
            }
        }
    }
}

#[cfg(test)]
mod tests {}
