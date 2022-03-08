//! This module contains an `Encoder` and `Decoder` for the NIMIQ message type. This message type has a fixed header,
//! containing a message type and other auxiliary information. The body of the message can be arbitrary bytes which are
//! later serialized/deserialized to the Message trait.
//!
//! Note that this doesn't actually serialize/deserialize the message content, but only handles reading/writing the
//! message, extracting the type ID and performing consistency checks.
//!

use std::{
    fmt::Debug,
    io::{self, Cursor, Write},
};

use bytes::{Buf, BytesMut};
use futures::prelude::*;
use libp2p::core::{upgrade, ProtocolName};
use libp2p::request_response::RequestResponseCodec;
use thiserror::Error;
use tokio_util::codec::{Decoder, Encoder};

use beserial::{Deserialize, Serialize, SerializingError};
pub use nimiq_network_interface::message::{Message, MessageType};
use nimiq_network_interface::peer::SendError;
use nimiq_utils::crc::Crc32Computer;

use crate::REQRES_PROTOCOL;

/// Maximum response size in bytes (10 kB)
const MAX_REQUEST_SIZE: usize = 2 * 1024;
/// Maximum response size in bytes (10 MB)
const MAX_RESPONSE_SIZE: usize = 10 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum Error {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialize(SerializingError),

    #[error("Invalid magic: {0:x}")]
    InvalidMagic(u32),

    #[error("Invalid length: {0}")]
    InvalidLength(u32),

    #[error("Checksum mismatch. Expected: {0}, obtained: {1}")]
    ChecksumMismatch(u32, u32),
}

impl Error {
    pub fn eof() -> Self {
        Error::Io(std::io::Error::from(std::io::ErrorKind::UnexpectedEof))
    }
}

impl From<SerializingError> for Error {
    fn from(e: SerializingError) -> Self {
        match e {
            SerializingError::IoError(e) => Error::Io(e),
            e => Error::Serialize(e),
        }
    }
}

impl From<Error> for SendError {
    fn from(e: Error) -> Self {
        match e {
            Error::Io(e) => SendError::Serialization(e.into()),
            Error::Serialize(e) => SendError::Serialization(e),
            Error::InvalidMagic(_) => SendError::Serialization(SerializingError::InvalidValue),
            Error::InvalidLength(_) => SendError::Serialization(SerializingError::InvalidValue),
            Error::ChecksumMismatch(_, _) => {
                SendError::Serialization(SerializingError::InvalidValue)
            }
        }
    }
}

/// Header for the typed messages
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Header {
    /// Magic number: It should always be 0x4204_2042
    pub magic: u32,
    /// Type ID of the message
    pub type_id: u64,
    /// Length of the message including the header
    pub length: u32,
    /// Checksum of the frame
    pub checksum: u32,
}

impl Header {
    /// Magic value for the Typed messages (0x4204_2042)
    pub const MAGIC: u32 = 0x4204_2042;
    /// Total size of the header:
    /// - magic: 4B
    /// - type_id: 8B
    /// - length: 4B
    /// - checksum: 4B
    pub const SIZE: usize = 20;

    fn new(type_id: u64) -> Self {
        Self {
            magic: Self::MAGIC,
            type_id,
            length: 0,
            checksum: 0,
        }
    }

    fn preliminary_check(&self) -> Result<(), Error> {
        if self.magic != Self::MAGIC {
            Err(Error::InvalidMagic(self.magic))
        } else if (self.length as usize) < Self::SIZE {
            Err(Error::InvalidLength(self.length))
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Debug)]
enum DecodeState {
    Head,

    Data {
        header: Header,
        header_length: usize,
    },
}

impl Default for DecodeState {
    fn default() -> Self {
        DecodeState::Head
    }
}

#[derive(Clone, Debug, Default)]
pub struct MessageCodec {
    state: DecodeState,
}

impl MessageCodec {
    fn verify(&self, declared_crc: u32, data: &mut BytesMut) -> Result<(), Error> {
        let mut crc_comp = Crc32Computer::default();

        // Re-calculate CRC skipping the CRC field (last 4B of the header)
        crc_comp.update(&data.as_ref()[..(Header::SIZE - 4)]);
        crc_comp.update(&[0u8; 4]);
        crc_comp.update(&data.as_ref()[Header::SIZE..]);
        let crc = crc_comp.result();

        if crc != declared_crc {
            return Err(Error::ChecksumMismatch(declared_crc, crc));
        }

        Ok(())
    }

    fn encode_serialized_message(
        &mut self,
        type_id: u64,
        message: &BytesMut,
        dst: &mut BytesMut,
    ) -> Result<(), Error> {
        let mut header = Header::new(type_id);
        let message_length = Header::SIZE + message.len();
        header.length = message_length as u32;

        let existing_length = dst.len();
        dst.reserve(message_length);
        dst.resize(existing_length + message_length, 0);

        // Go to the bottom of the buffer to write the data
        let mut c = Cursor::new(dst.as_mut());
        c.set_position(existing_length as u64);

        // Write header
        header.serialize(&mut c)?;

        // Serialize message
        c.write_all(message)?;

        // Calculate the CRC
        let crc = Crc32Computer::default()
            .update(&c.get_ref()[existing_length..])
            .result();

        // Write the CRC in the respective field in the header
        c.set_position((existing_length + Header::SIZE - 4) as u64);
        crc.serialize(&mut c)?;

        Ok(())
    }
}

impl Decoder for MessageCodec {
    type Item = (MessageType, BytesMut);
    type Error = Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<(MessageType, BytesMut)>, Error> {
        let span = log::trace_span!("decode");
        let _enter = span.enter();
        loop {
            match &self.state {
                DecodeState::Head => {
                    // Make a cursor, so we later know how many bytes we read
                    let mut c = Cursor::new(&src);

                    match Header::deserialize(&mut c) {
                        Ok(header) => {
                            // Deserializing the header was successful
                            let header_length = c.position() as usize;

                            // Drop the cursor, so we can mut-borrow the `src` buffer again.
                            drop(c);

                            // Preliminary header check (we can't verify the checksum yet)
                            header.preliminary_check()?;

                            // Set decode state to reading the remaining data
                            self.state = DecodeState::Data {
                                header,
                                header_length,
                            };

                            // Don't return but continue in loop to parse the body.
                        }
                        Err(SerializingError::IoError(e))
                            if matches!(e.kind(), io::ErrorKind::UnexpectedEof) =>
                        {
                            // We just need to wait for more data
                            return Ok(None);
                        }
                        Err(e) => {
                            log::warn!("Error decoding message header: {}", e);
                            return Err(e.into());
                        }
                    }
                }
                DecodeState::Data {
                    header,
                    header_length,
                } => {
                    if src.len() >= header.length as usize {
                        // We have read enough bytes to read the full message
                        let message_type = header.type_id;

                        // Get buffer for whole message
                        let frame_size = header.length as usize;
                        let mut data = src.split_to(frame_size);

                        // Verify the message (i.e. checksum)
                        self.verify(header.checksum, &mut data).map_err(|e| {
                            log::warn!(
                                "CRC checksum mismatch for message type {}, error: {}",
                                message_type,
                                e
                            );
                            e
                        })?;

                        // Skip the header to have only the data
                        data.advance(*header_length);

                        self.state = DecodeState::Head;

                        return Ok(Some((MessageType::new(message_type), data)));
                    } else {
                        // We still need to read more of the message body
                        return Ok(None);
                    }
                }
            }
        }
    }

    fn decode_eof(&mut self, buf: &mut BytesMut) -> Result<Option<(MessageType, BytesMut)>, Error> {
        match self.decode(buf) {
            Ok(None) if buf.has_remaining() => Err(Error::eof()),
            r => r,
        }
    }
}

/// Encoder for a full message
impl<M: Message> Encoder<&M> for MessageCodec {
    type Error = Error;

    fn encode(&mut self, message: &M, dst: &mut BytesMut) -> Result<(), Error> {
        let mut header = Header::new(M::TYPE_ID);
        let message_length = Header::SIZE + message.serialized_size();
        header.length = message_length as u32;

        let existing_length = dst.len();
        dst.reserve(message_length);
        dst.resize(existing_length + message_length, 0);

        // Go to the bottom of the buffer to write the data
        let mut c = Cursor::new(dst.as_mut());
        c.set_position(existing_length as u64);

        // Write header
        header.serialize(&mut c)?;

        // Serialize message
        message.serialize(&mut c)?;

        // Calculate the CRC
        let crc = Crc32Computer::default()
            .update(&c.get_ref()[existing_length..])
            .result();

        // Write the CRC in the respective field in the header
        c.set_position((existing_length + Header::SIZE - 4) as u64);
        crc.serialize(&mut c)?;

        Ok(())
    }
}

pub type IncomingRequest = (MessageType, BytesMut);
pub type OutgoingResponse = (MessageType, BytesMut);

#[derive(Debug, Clone)]
pub enum ReqResProtocol {
    Version1,
}

impl ProtocolName for ReqResProtocol {
    fn protocol_name(&self) -> &[u8] {
        match *self {
            ReqResProtocol::Version1 => REQRES_PROTOCOL,
        }
    }
}

#[async_trait::async_trait]
impl RequestResponseCodec for MessageCodec {
    type Protocol = ReqResProtocol;
    type Request = IncomingRequest;
    type Response = OutgoingResponse;

    async fn read_request<T>(&mut self, _: &Self::Protocol, io: &mut T) -> io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        let bytes = upgrade::read_length_prefixed(io, MAX_REQUEST_SIZE).await?;
        let request = self
            .decode(&mut bytes[..].into())
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        if let Some((request_id, request_data)) = request {
            Ok((request_id, request_data))
        } else {
            Err(io::Error::new(
                io::ErrorKind::Other,
                "Fail to decode request",
            ))
        }
    }

    async fn read_response<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        let bytes = upgrade::read_length_prefixed(io, MAX_RESPONSE_SIZE).await?;
        let response = self
            .decode(&mut bytes[..].into())
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        if let Some((message_type, response_data)) = response {
            Ok((message_type, response_data))
        } else {
            Err(io::Error::new(
                io::ErrorKind::Other,
                "Fail to decode response",
            ))
        }
    }

    async fn write_request<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
        req: Self::Request,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Send + Unpin,
    {
        let (type_id, request) = req;
        let mut buffer = BytesMut::with_capacity(request.len());
        self.encode_serialized_message(type_id.into(), &request, &mut buffer)
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "Fail to encode request"))?;
        upgrade::write_length_prefixed(io, &buffer[..]).await?;
        io.close().await
    }

    async fn write_response<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
        res: Self::Response,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        let (type_id, response) = res;
        let mut buffer = BytesMut::with_capacity(response.len());
        self.encode_serialized_message(type_id.into(), &response, &mut buffer)
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "Fail to encode response"))?;
        upgrade::write_length_prefixed(io, &buffer[..]).await?;
        io.close().await
    }
}
