use std::io::{self, Write};

use greetd_ipc::{Request, Response, codec::Error};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use zeroize::{Zeroize, Zeroizing};

// Normal greetd traffic is tiny. This still leaves ample room for unusually
// large PAM prompts and configured session environments while preventing a
// malformed peer from forcing a multi-gigabyte allocation.
const MAX_FRAME_SIZE: usize = 8 * 1024 * 1024;

#[cfg(test)]
type DropProbe = Box<dyn FnOnce(&Request) + Send>;

/// A queued request whose PAM response is scrubbed on every drop path.
///
/// This protects process-owned request storage. It cannot erase copies already
/// made by the allocator, kernel socket buffers, greetd, or PAM.
pub(crate) struct SensitiveRequest {
  request: Request,
  #[cfg(test)]
  drop_probe: Option<DropProbe>,
}

impl SensitiveRequest {
  pub(crate) fn new(request: Request) -> Self {
    Self {
      request,
      #[cfg(test)]
      drop_probe: None,
    }
  }

  #[cfg(test)]
  pub(crate) fn with_drop_probe(request: Request, probe: impl FnOnce(&Request) + Send + 'static) -> Self {
    Self {
      request,
      drop_probe: Some(Box::new(probe)),
    }
  }

  fn scrub(&mut self) {
    if let Request::PostAuthMessageResponse { response: Some(secret) } = &mut self.request {
      secret.zeroize();
    }
  }
}

impl AsRef<Request> for SensitiveRequest {
  fn as_ref(&self) -> &Request {
    &self.request
  }
}

impl Drop for SensitiveRequest {
  fn drop(&mut self) {
    self.scrub();
    #[cfg(test)]
    if let Some(probe) = self.drop_probe.take() {
      probe(&self.request);
    }
  }
}

/// Serialize one greetd request with the upstream wire format while keeping
/// the completed JSON body under zeroizing ownership. Serializing into an
/// exactly-sized buffer also avoids ordinary Vec growth copies in this layer.
pub(crate) async fn write_request<W>(request: &Request, stream: &mut W) -> Result<(), Error>
where
  W: AsyncWrite + Unpin + Send,
{
  let mut counter = CountingWriter::default();
  serde_json::to_writer(&mut counter, request)?;
  if counter.length > MAX_FRAME_SIZE {
    return Err(Error::Serialization(format!(
      "greetd request frame exceeds {MAX_FRAME_SIZE} bytes"
    )));
  }
  let length = u32::try_from(counter.length)
    .map_err(|_| Error::Serialization("greetd request exceeds the u32 frame length".into()))?;

  let mut body = Zeroizing::new(Vec::with_capacity(counter.length));
  serde_json::to_writer(&mut *body, request)?;
  debug_assert_eq!(body.len(), counter.length);

  stream.write_all(&length.to_ne_bytes()).await?;
  stream.write_all(&body).await?;
  Ok(())
}

/// Read one greetd response with the upstream wire format and scrub the raw
/// JSON frame when this future completes, errors, or is cancelled.
pub(crate) async fn read_response<R>(stream: &mut R) -> Result<Response, Error>
where
  R: AsyncRead + Unpin + Send,
{
  let mut length = [0; size_of::<u32>()];
  stream
    .read_exact(&mut length)
    .await
    .map_err(|error| match error.kind() {
      io::ErrorKind::UnexpectedEof => Error::Eof,
      _ => error.into(),
    })?;

  let length = u32::from_ne_bytes(length) as usize;
  if length > MAX_FRAME_SIZE {
    return Err(Error::Serialization(format!(
      "greetd response frame exceeds {MAX_FRAME_SIZE} bytes"
    )));
  }

  let mut body = Zeroizing::new(vec![0; length]);
  stream.read_exact(&mut body).await?;
  Ok(serde_json::from_slice(&body)?)
}

#[derive(Default)]
struct CountingWriter {
  length: usize,
}

impl Write for CountingWriter {
  fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
    self.length = self
      .length
      .checked_add(buffer.len())
      .ok_or_else(|| io::Error::other("serialized greetd request length overflow"))?;
    Ok(buffer.len())
  }

  fn flush(&mut self) -> io::Result<()> {
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use std::sync::mpsc;

  use greetd_ipc::{ErrorType, Request, Response, codec::TokioCodec};
  use tokio::io::{AsyncWriteExt, duplex};

  use super::{MAX_FRAME_SIZE, SensitiveRequest, read_response, write_request};

  #[test]
  fn sensitive_request_scrubs_its_response_before_drop_completes() {
    let (probe, result) = mpsc::channel();
    let request = SensitiveRequest::with_drop_probe(
      Request::PostAuthMessageResponse {
        response: Some("correct horse battery staple".into()),
      },
      move |request| {
        probe
          .send(matches!(
            request,
            Request::PostAuthMessageResponse { response: Some(response) } if response.is_empty()
          ))
          .unwrap();
      },
    );

    drop(request);
    assert!(result.recv().unwrap());
  }

  #[tokio::test]
  async fn local_request_writer_is_compatible_with_upstream_reader() {
    let (mut local, mut upstream) = duplex(4096);
    let request = Request::PostAuthMessageResponse {
      response: Some("p\"a\\s\nword".into()),
    };

    write_request(&request, &mut local).await.unwrap();
    let decoded = Request::read_from(&mut upstream).await.unwrap();

    assert!(matches!(
      decoded,
      Request::PostAuthMessageResponse { response: Some(response) } if response == "p\"a\\s\nword"
    ));
  }

  #[tokio::test]
  async fn local_response_reader_is_compatible_with_upstream_writer() {
    let (mut upstream, mut local) = duplex(4096);
    let response = Response::Error {
      error_type: ErrorType::AuthError,
      description: "secret echoed by a PAM module".into(),
    };

    response.write_to(&mut upstream).await.unwrap();
    let decoded = read_response(&mut local).await.unwrap();

    assert!(matches!(decoded, Response::Error {
      error_type: ErrorType::AuthError,
      description,
    } if description == "secret echoed by a PAM module"));
  }

  #[tokio::test]
  async fn truncated_frames_keep_upstream_error_semantics() {
    let (mut writer, mut reader) = duplex(64);
    writer.write_all(&[0, 0]).await.unwrap();
    writer.shutdown().await.unwrap();
    assert_eq!(read_response(&mut reader).await.unwrap_err().to_string(), "EOF");

    let (mut writer, mut reader) = duplex(64);
    writer.write_all(&8_u32.to_ne_bytes()).await.unwrap();
    writer.write_all(b"short").await.unwrap();
    writer.shutdown().await.unwrap();
    assert!(
      read_response(&mut reader)
        .await
        .unwrap_err()
        .to_string()
        .starts_with("i/o error:")
    );
  }

  #[tokio::test]
  async fn oversized_response_is_rejected_before_reading_its_body() {
    let (mut writer, mut reader) = duplex(64);
    let oversized = u32::try_from(MAX_FRAME_SIZE + 1).unwrap();
    writer.write_all(&oversized.to_ne_bytes()).await.unwrap();

    let error = read_response(&mut reader).await.unwrap_err().to_string();
    assert!(error.contains("greetd response frame exceeds"));
  }

  #[tokio::test]
  async fn oversized_request_is_rejected_before_writing_its_frame() {
    let (mut writer, _reader) = duplex(64);
    let request = Request::PostAuthMessageResponse {
      response: Some("x".repeat(MAX_FRAME_SIZE)),
    };

    let error = write_request(&request, &mut writer).await.unwrap_err().to_string();
    assert!(error.contains("greetd request frame exceeds"));
  }
}
