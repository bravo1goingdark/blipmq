use std::sync::Arc;

use bytes::{Buf, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tracing::{error, trace};

use blipmq_auth::{ApiKey, ApiKeyValidator};

use crate::error::Error;
use crate::frame::{self, AckPayload, AuthPayload, Frame, FrameType, HelloPayload, NackPayload};
use crate::server::{FrameResponse, MessageHandler};

const INITIAL_BUFFER_SIZE: usize = 8 * 1024;

pub struct Connection<H>
where
    H: MessageHandler,
{
    id: u64,
    stream: TcpStream,
    handler: H,
    read_buf: BytesMut,
    write_buf: BytesMut,
    shutdown: watch::Receiver<bool>,
    auth_validator: Arc<dyn ApiKeyValidator>,
    hello_performed: bool,
    authenticated: bool,
}

impl<H> Connection<H>
where
    H: MessageHandler,
{
    pub fn new(
        id: u64,
        stream: TcpStream,
        handler: H,
        auth_validator: Arc<dyn ApiKeyValidator>,
        shutdown: watch::Receiver<bool>,
    ) -> Self {
        Self {
            id,
            stream,
            handler,
            read_buf: BytesMut::with_capacity(INITIAL_BUFFER_SIZE),
            write_buf: BytesMut::with_capacity(INITIAL_BUFFER_SIZE),
            shutdown,
            auth_validator,
            hello_performed: false,
            authenticated: false,
        }
    }

    pub async fn run(mut self) {
        let span = tracing::info_span!("connection", conn_id = self.id);
        let _guard = span.enter();
        if let Err(err) = self.run_inner().await {
            error!("connection {} error: {}", self.id, err);
        }
        trace!("connection {} closed", self.id);
    }

    async fn run_inner(&mut self) -> Result<(), Error> {
        loop {
            tokio::select! {
                read_result = self.stream.read_buf(&mut self.read_buf) => {
                    let n = read_result?;
                    if n == 0 {
                        return Ok(());
                    }

                    while let Some(frame) = frame::try_decode_frame(&mut self.read_buf)? {
                        self.process_frame(frame).await?;
                    }
                }
                result = self.shutdown.changed() => {
                    return match result {
                        Ok(_) => {
                            Ok(())
                        }
                        Err(_) => {
                            Ok(())
                        }
                    }
                }
            }
        }
    }

    async fn process_frame(&mut self, frame: Frame) -> Result<(), Error> {
        match frame.msg_type {
            FrameType::Hello => self.handle_hello(frame).await,
            FrameType::Auth => self.handle_auth(frame).await,
            _ => {
                if !self.authenticated {
                    self.send_nack(frame.correlation_id, 401, "unauthenticated")
                        .await?;
                    return Ok(());
                }

                let response = self.handler.handle_frame(self.id, frame).await;
                match response {
                    Ok(FrameResponse::None) => {}
                    Ok(FrameResponse::Frame(resp_frame)) => {
                        frame::encode_frame(&resp_frame, &mut self.write_buf)?;
                        self.flush_write_buffer().await?;
                    }
                    Err(err) => {
                        return Err(err);
                    }
                }

                Ok(())
            }
        }
    }

    async fn flush_write_buffer(&mut self) -> Result<(), Error> {
        while !self.write_buf.is_empty() {
            let n = self.stream.write(&self.write_buf).await?;
            if n == 0 {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "failed to write frame",
                )));
            }
            self.write_buf.advance(n);
        }

        self.stream.flush().await?;
        Ok(())
    }

    async fn handle_hello(&mut self, frame: Frame) -> Result<(), Error> {
        if self.hello_performed {
            self.send_nack(frame.correlation_id, 400, "HELLO already performed")
                .await?;
            return Ok(());
        }

        let decoded = HelloPayload::decode(&frame.payload);
        match decoded {
            Ok(payload) => {
                if payload.protocol_version != frame::PROTOCOL_VERSION {
                    self.send_nack(frame.correlation_id, 426, "unsupported protocol version")
                        .await?;
                    return Ok(());
                }

                self.hello_performed = true;
                self.send_ack(frame.correlation_id).await?;
                Ok(())
            }
            Err(_) => {
                self.send_nack(frame.correlation_id, 400, "invalid HELLO payload")
                    .await?;
                Ok(())
            }
        }
    }

    async fn handle_auth(&mut self, frame: Frame) -> Result<(), Error> {
        if !self.hello_performed {
            self.send_nack(frame.correlation_id, 400, "HELLO not performed")
                .await?;
            return Ok(());
        }

        if self.authenticated {
            self.send_nack(frame.correlation_id, 400, "already authenticated")
                .await?;
            return Ok(());
        }

        let decoded = AuthPayload::decode(&frame.payload);
        let payload = match decoded {
            Ok(p) => p,
            Err(_) => {
                self.send_nack(frame.correlation_id, 400, "invalid AUTH payload")
                    .await?;
                return Ok(());
            }
        };

        let api_key = ApiKey::new(payload.api_key);
        if self.auth_validator.validate(&api_key) {
            self.authenticated = true;
            self.send_ack(frame.correlation_id).await?;
        } else {
            self.send_nack(frame.correlation_id, 401, "invalid API key")
                .await?;
        }

        Ok(())
    }

    async fn send_ack(&mut self, correlation_id: u64) -> Result<(), Error> {
        let payload = AckPayload { subscription_id: 0 }.encode()?;
        let frame = Frame {
            msg_type: FrameType::Ack,
            correlation_id,
            payload,
        };
        frame::encode_frame(&frame, &mut self.write_buf)?;
        self.flush_write_buffer().await
    }

    async fn send_nack(
        &mut self,
        correlation_id: u64,
        code: u16,
        message: &str,
    ) -> Result<(), Error> {
        let payload = NackPayload {
            code,
            message: message.to_string(),
        }
        .encode()?;
        let frame = Frame {
            msg_type: FrameType::Nack,
            correlation_id,
            payload,
        };
        frame::encode_frame(&frame, &mut self.write_buf)?;
        self.flush_write_buffer().await
    }
}
