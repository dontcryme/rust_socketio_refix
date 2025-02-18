use crate::error::{Error, Result};
use crate::packet::{Packet, PacketId};
use bytes::Bytes;
use rust_engineio::{Client as EngineClient, Packet as EnginePacket, PacketId as EnginePacketId};
use std::convert::TryFrom;
use std::sync::atomic::AtomicI32;
use std::sync::{atomic::AtomicBool, Arc};
use std::{fmt::Debug, sync::atomic::Ordering};

use super::{event::Event, payload::Payload};

/// Handles communication in the `socket.io` protocol.
#[derive(Clone, Debug)]
pub struct Socket {
    //TODO: 0.4.0 refactor this
    engine_client: Arc<EngineClient>,
    connected: Arc<AtomicBool>,
    ack_id: Arc<AtomicI32>,
}

impl Socket {
    /// Creates an instance of `Socket`.
    #[doc(hidden)]
    pub(super) fn new(engine_client: EngineClient) -> Result<Self> {
        let ack_id = Arc::new(AtomicI32::new(-1));

        Ok(Socket {
            engine_client: Arc::new(engine_client),
            connected: Arc::new(AtomicBool::default()),
            ack_id: ack_id.clone(),
        })
    }

    /// Connects to the server. This includes a connection of the underlying
    /// engine.io client and afterwards an opening socket.io request.
    pub fn connect(&self) -> Result<()> {
        self.engine_client.connect()?;

        // store the connected value as true, if the connection process fails
        // later, the value will be updated
        self.connected.store(true, Ordering::Release);

        Ok(())
    }

    /// Disconnects from the server by sending a socket.io `Disconnect` packet. This results
    /// in the underlying engine.io transport to get closed as well.
    pub fn disconnect(&self) -> Result<()> {
        if self.is_engineio_connected()? {
            self.engine_client.disconnect()?;
        }
        if self.connected.load(Ordering::Acquire) {
            self.connected.store(false, Ordering::Release);
        }

        if self.ack_id.load(Ordering::Acquire) != -1 {
            self.ack_id.store(-1, Ordering::Release);
        }

        Ok(())
    }

    /// Sends a `socket.io` packet to the server using the `engine.io` client.
    pub fn send(&self, packet: Packet) -> Result<()> {
        if !self.is_engineio_connected()? || !self.connected.load(Ordering::Acquire) {
            return Err(Error::IllegalActionBeforeOpen());
        }

        // the packet, encoded as an engine.io message packet
        let engine_packet = EnginePacket::new(EnginePacketId::Message, Bytes::from(&packet));
        self.engine_client.emit(engine_packet)?;

        if let Some(attachments) = packet.attachments {
            for attachment in attachments {
                let engine_packet = EnginePacket::new(EnginePacketId::MessageBinary, attachment);
                self.engine_client.emit(engine_packet)?;
            }
        }

        Ok(())
    }

    /// Emits to connected other side with given data
    pub fn ack(&self, nsp: &str, data: Payload) -> Result<()> {
        let socket_packet =
            Packet::ack_from_payload(data, nsp, Some(self.ack_id.load(Ordering::Acquire)))?;
        self.send(socket_packet)
    }

    /// Emits to certain event with given data. The data needs to be JSON,
    /// otherwise this returns an `InvalidJson` error.
    pub fn emit(&self, nsp: &str, event: Event, data: Payload) -> Result<()> {
        let socket_packet = Packet::new_from_payload(data, event, nsp, None)?;

        self.send(socket_packet)
    }

    pub(crate) fn poll(&self) -> Result<Option<Packet>> {
        loop {
            match self.engine_client.poll() {
                Ok(Some(packet)) => {
                    if packet.packet_id == EnginePacketId::Message
                        || packet.packet_id == EnginePacketId::MessageBinary
                    {
                        let packet = self.handle_engineio_packet(packet)?;
                        if self.ack_id.load(Ordering::Acquire) != packet.id.unwrap_or(-1) {
                            self.ack_id
                                .store(packet.id.unwrap_or(-1), Ordering::Release);
                        }
                        self.handle_socketio_packet(&packet);
                        return Ok(Some(packet));
                    } else {
                        continue;
                    }
                }
                Ok(None) => {
                    return Ok(None);
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    /// Handles the connection/disconnection.
    #[inline]
    fn handle_socketio_packet(&self, socket_packet: &Packet) {
        match socket_packet.packet_type {
            PacketId::Connect => {
                self.connected.store(true, Ordering::Release);
            }
            PacketId::ConnectError => {
                self.connected.store(false, Ordering::Release);
            }
            PacketId::Disconnect => {
                self.connected.store(false, Ordering::Release);
            }
            _ => (),
        }
    }

    /// Handles new incoming engineio packets
    fn handle_engineio_packet(&self, packet: EnginePacket) -> Result<Packet> {
        let mut socket_packet = Packet::try_from(&packet.data)?;

        // Only handle attachments if there are any
        if socket_packet.attachment_count > 0 {
            let mut attachments_left = socket_packet.attachment_count;
            let mut attachments = Vec::new();
            while attachments_left > 0 {
                let next = self.engine_client.poll();
                match next {
                    Err(err) => return Err(err.into()),
                    Ok(Some(packet)) => match packet.packet_id {
                        EnginePacketId::MessageBinary | EnginePacketId::Message => {
                            attachments.push(packet.data);
                            attachments_left -= 1;
                        }
                        _ => {
                            return Err(Error::InvalidAttachmentPacketType(
                                packet.packet_id.into(),
                            ));
                        }
                    },
                    Ok(None) => {
                        // Engineio closed before attachments completed.
                        return Err(Error::IncompletePacket());
                    }
                }
            }
            socket_packet.attachments = Some(attachments);
        }

        Ok(socket_packet)
    }

    fn is_engineio_connected(&self) -> Result<bool> {
        Ok(self.engine_client.is_connected()?)
    }
}
