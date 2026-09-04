use bincode::{DefaultOptions, Options};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::io::{Error, ErrorKind};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

// Enough for the largest clipboard contents plus overhead.
const MAX_LENGTH: usize = crate::clipboard::MAX_SIZE + 4096;

pub trait Message: Sized {
    async fn decode<R: AsyncRead + Send + Unpin>(stream: &mut R) -> Result<Self, Error>;

    async fn encode<W: AsyncWrite + Send + Unpin>(&self, stream: &mut W) -> Result<(), Error>;
}

impl<T: DeserializeOwned + Serialize + Sync> Message for T {
    async fn decode<R: AsyncRead + Send + Unpin>(stream: &mut R) -> Result<Self, Error> {
        let length = stream.read_u32().await?;
        if length as usize > MAX_LENGTH {
            return Err(Error::new(ErrorKind::InvalidData, "Message too large"));
        }

        let mut data = Vec::new();
        stream.take(length.into()).read_to_end(&mut data).await?;

        if data.len() != length as usize {
            return Err(Error::new(
                ErrorKind::UnexpectedEof,
                "Message shorter than advertised",
            ));
        }

        let data = options()
            .deserialize(&data)
            .map_err(|err| Error::new(ErrorKind::InvalidData, err))?;

        tracing::trace!("Read {} bytes", 4 + length);

        Ok(data)
    }

    async fn encode<W: AsyncWrite + Send + Unpin>(&self, stream: &mut W) -> Result<(), Error> {
        let data = options()
            .serialize(self)
            .map_err(|err| Error::new(ErrorKind::InvalidInput, err))?;

        let length = data
            .len()
            .try_into()
            .map_err(|_| Error::new(ErrorKind::InvalidInput, "Data too large"))?;

        stream.write_u32(length).await?;
        stream.write_all(&data).await?;

        tracing::trace!("Wrote {} bytes", 4 + data.len());

        Ok(())
    }
}

fn options() -> impl Options {
    DefaultOptions::new().with_limit(MAX_LENGTH as _)
}
