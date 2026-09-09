//! Control-stream message I/O: length-prefixed protobuf over QUIC streams.

use prost::Message;
use quinn::{RecvStream, SendStream};

use crate::error::Error;
use parley_proto::frame;

/// Write a length-prefixed protobuf message to a stream.
pub async fn write_message<M: Message>(tx: &mut SendStream, msg: &M) -> Result<(), Error> {
    let payload = msg.encode_to_vec();
    tx.write_all(&frame::encode(&payload)).await?;
    Ok(())
}

/// Read a length-prefixed protobuf message from a stream.
pub async fn read_message<M: Message + Default>(rx: &mut RecvStream) -> Result<M, Error> {
    let mut len_buf = [0u8; 4];
    rx.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;

    if len > frame::MAX_SIZE {
        return Err(Error::MessageTooLarge);
    }

    let mut payload = vec![0u8; len];
    rx.read_exact(&mut payload).await?;

    let msg = M::decode(payload.as_slice())?;
    Ok(msg)
}
