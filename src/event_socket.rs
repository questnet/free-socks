//! # FreeSWITCH inbound socket.
//!
//! ADR:
//! Initially we thought about creating a design that polls the events and the commands at the same
//! time, but it turned out that select! not simple to use on futures that are not cancellation safe
//! (which our event reader isn't, because of the intermediate buffers).
//!
//! To remedy that, a channel mpsc queue based approach was used. The event reader and the command
//! sender push both to the same queue which gets processed by a driver.
use crate::{
    client::send_api,
    event::{AuthRequest, DisconnectNotice},
    event_socket::driver::ProcessingResult,
    Client, FromMessage,
};
use anyhow::{Context, Result};
use futures::{Future, FutureExt};
use log::{debug, error, trace};
use std::{future::pending, time::Duration};
use tokio::{
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpStream, ToSocketAddrs,
    },
    select, spawn,
    sync::mpsc::{self, Receiver, Sender},
    task::{JoinError, JoinHandle},
    time,
};

use self::{
    driver::{Driver, DriverMessage},
    event_reader::EventReader,
};

pub mod driver;
mod event_reader;

#[derive(Debug)]
pub struct EventSocket {
    driver_tx: mpsc::Sender<DriverMessage>,
    event_socket: JoinHandle<Result<Option<DisconnectNotice>>>,
}

impl Drop for EventSocket {
    fn drop(&mut self) {
        self.event_socket.abort()
    }
}

/// `EventSocket` is itself a future that completes when the connection to the FreeSWITCH event
/// socket is closed. It does not need to be polled for the event socket to execute.
impl Future for EventSocket {
    type Output = std::result::Result<Result<Option<DisconnectNotice>>, JoinError>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.event_socket.poll_unpin(cx)
    }
}

const MAX_QUEUE_SIZE: usize = 256;

impl EventSocket {
    pub async fn connect(
        endpoint: impl ToSocketAddrs,
        keepalive_interval: impl Into<Option<Duration>>,
    ) -> Result<EventSocket> {
        let (driver_sender, driver_rx) = mpsc::channel(MAX_QUEUE_SIZE);

        let (tcp_receive, tcp_write) = {
            let tcp = TcpStream::connect(endpoint).await?;
            // Flush network packets as soon as possible.
            tcp.set_nodelay(true)?;
            tcp.into_split()
        };

        let mut reader = EventReader::new(tcp_receive);

        // Wait for the initial `auth/request`.
        {
            let initial_request = reader
                .read_event()
                .await
                .context("Waiting for the initial auth request")?;
            AuthRequest::from_message(initial_request).context("Expecting auth/request")?;
        }

        // Spawn the event socket.
        //
        // ADR: This is for convenience so that callers don't need to drive the async function and
        // can simply send API requests.
        let driver_tx_socket = driver_sender.clone();
        let keepalive_interval = keepalive_interval.into();
        let event_socket = spawn(async move {
            event_socket(
                keepalive_interval,
                reader,
                tcp_write,
                driver_rx,
                driver_tx_socket,
            )
            .await
        });

        let socket = EventSocket {
            driver_tx: driver_sender,
            event_socket,
        };

        Ok(socket)
    }

    pub fn client(&self) -> Client {
        Client::new(self.driver_tx.clone())
    }

    pub fn is_connected(&self) -> bool {
        // If all receivers are closed, we can assume that the event socket is closed.
        !self.driver_tx.is_closed()
    }
}

/// The primary async function that represents the event socket. Its returned future must be polled
/// until it completes indicating that the connection to the FreeSWITCH event socket terminated.
async fn event_socket(
    keepalives: Option<Duration>,
    mut reader: EventReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    driver_rx: Receiver<DriverMessage>,
    driver_tx: Sender<DriverMessage>,
) -> Result<Option<DisconnectNotice>> {
    let reader = dispatch_events(&mut reader, driver_tx.clone());
    let driver = dispatch_messages(driver_rx, writer);

    let send_keepalives = {
        if let Some(keepalive_duration) = keepalives {
            debug!(
                "Enabling keepalives with a duration of {:?}",
                keepalive_duration
            );
            send_keepalives(driver_tx, keepalive_duration).boxed()
        } else {
            debug!("Keepalives disabled");
            pending::<Result<()>>().boxed()
        }
    };

    select! {
        r = reader => {
            debug!("Reader loop ended");
            if let Err(ref e) = r {
                error!("Reader loop crashed: {}", e);
            }
            r.map(|()| None)
        }
        r = driver => {
            debug!("Driver loop ended");
            if let Err(ref e) = r {
                error!("Driver loop crashed: {}", e);
            }
            r
        }
        r = send_keepalives => {
            // Note that the keepalive loop ends only in case of an error.
            debug!("Keepalive loop ended");
            if let Err(ref e) = r {
                error!("Keepalive loop crashed: {}", e);
            }
            r.map(|()| None)
        }
    }
}

async fn dispatch_events(
    reader: &mut EventReader<OwnedReadHalf>,
    driver: Sender<DriverMessage>,
) -> Result<()> {
    loop {
        let event = reader.read_event().await?;
        driver.send(DriverMessage::Event(event)).await?;
    }
}

async fn dispatch_messages(
    receiver: Receiver<DriverMessage>,
    tcp_write: OwnedWriteHalf,
) -> Result<Option<DisconnectNotice>> {
    let mut driver = Driver::new(tcp_write);
    let mut receiver = receiver;
    loop {
        let message = receiver.recv().await;
        if let Some(message) = message {
            use ProcessingResult::*;
            match driver.process_message(message).await? {
                Continue => {
                    continue;
                }
                Disconnected(notice) => {
                    debug!("Received disconnect notice: {}", notice);
                    return Ok(Some(notice));
                }
            }
        } else {
            debug!("Receiver channel got closed (all senders dropped)");
            return Ok(None);
        }
    }
}

async fn send_keepalives(driver_tx: Sender<DriverMessage>, duration: Duration) -> Result<()> {
    loop {
        time::sleep(duration).await;
        // TODO: Really need timeouts here, what if FreeSWITCH crashes before responding?
        trace!("Sending keepalive");
        send_api(&driver_tx, "echo keepalive").await?;
    }
}
