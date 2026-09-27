// Copyright © 2020 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0
//

use std::io::{self, Read, Write};
use std::os::unix::io::RawFd;
use std::{num, str};

use thiserror::Error;
use vmm_sys_util::errno;
use vmm_sys_util::sock_ctrl_msg::ScmSocket;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Error writing to or reading from HTTP socket")]
    Socket(#[source] io::Error),
    #[error("Error sending file descriptors")]
    SocketSendFds(#[source] errno::Error),
    #[error("Error parsing HTTP status code")]
    StatusCodeParsing(#[source] num::ParseIntError),
    #[error("HTTP output is missing protocol statement")]
    MissingProtocol,
    #[error("Error parsing HTTP Content-Length field")]
    ContentLengthParsing(#[source] num::ParseIntError),
    #[error("HTTP response is not valid UTF-8")]
    InvalidUtf8(#[source] str::Utf8Error),
    #[error("Server responded with error {0:?}: {1:?}")]
    ServerResponse(
        StatusCode,
        // TODO: Move `api` module from `vmm` to dedicated crate and use a common type definition
        Option<
            String, /* Untyped: Currently Vec<String> of error messages from top to root cause */
        >,
    ),
}

#[derive(Clone, Copy, Debug)]
pub enum StatusCode {
    Continue,
    Ok,
    NoContent,
    BadRequest,
    NotFound,
    Conflict,
    TooManyRequests,
    InternalServerError,
    NotImplemented,
    Unknown,
}

impl StatusCode {
    fn from_raw(code: usize) -> StatusCode {
        match code {
            100 => StatusCode::Continue,
            200 => StatusCode::Ok,
            204 => StatusCode::NoContent,
            400 => StatusCode::BadRequest,
            404 => StatusCode::NotFound,
            409 => StatusCode::Conflict,
            429 => StatusCode::TooManyRequests,
            500 => StatusCode::InternalServerError,
            501 => StatusCode::NotImplemented,
            _ => StatusCode::Unknown,
        }
    }

    fn parse(code: &str) -> Result<StatusCode, Error> {
        Ok(StatusCode::from_raw(
            code.trim().parse().map_err(Error::StatusCodeParsing)?,
        ))
    }

    fn is_server_error(self) -> bool {
        !matches!(
            self,
            StatusCode::Ok | StatusCode::Continue | StatusCode::NoContent
        )
    }
}

fn get_header<'a>(res: &'a str, header: &'a str) -> Option<&'a str> {
    let header_str = format!("{header}: ");
    res.find(&header_str)
        .map(|o| &res[o + header_str.len()..o + res[o..].find('\r').unwrap()])
}

fn get_status_code(res: &str) -> Result<StatusCode, Error> {
    if let Some(o) = res.find("HTTP/1.1") {
        Ok(StatusCode::parse(
            &res[o + "HTTP/1.1 ".len()..res[o..].find('\r').unwrap()],
        )?)
    } else {
        Err(Error::MissingProtocol)
    }
}

fn parse_http_response(socket: &mut dyn Read) -> Result<Option<String>, Error> {
    // Collect raw bytes and decode only complete parts: a read may end in
    // the middle of a multi-byte UTF-8 character.
    let mut bytes = Vec::new();
    let mut body_offset = None;
    let mut content_length: Option<usize> = None;
    loop {
        let mut buf = [0; 256];
        let count = socket.read(&mut buf).map_err(Error::Socket)?;
        // If the return value is 0, the peer has performed an orderly shutdown.
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[0..count]);

        // End of headers
        if body_offset.is_none()
            && let Some(o) = bytes.windows(4).position(|w| w == b"\r\n\r\n")
        {
            let offset = o + "\r\n\r\n".len();
            body_offset = Some(offset);
            let headers = str::from_utf8(&bytes[..offset]).map_err(Error::InvalidUtf8)?;

            // With all headers available we can see if there is any body
            content_length = if let Some(length) = get_header(headers, "Content-Length") {
                Some(length.trim().parse().map_err(Error::ContentLengthParsing)?)
            } else {
                None
            };

            if content_length.is_none() {
                break;
            }
        }

        if let Some(body_offset) = body_offset
            && let Some(content_length) = content_length
            && bytes.len() >= content_length + body_offset
        {
            break;
        }
    }
    let res = str::from_utf8(&bytes).map_err(Error::InvalidUtf8)?;
    let body_string = content_length.and(body_offset.map(|o| String::from(&res[o..])));
    let status_code = get_status_code(res)?;

    if status_code.is_server_error() {
        Err(Error::ServerResponse(status_code, body_string))
    } else {
        Ok(body_string)
    }
}

/// Make an API request using the fully qualified command name.
/// For example, full_command could be "vm.create" or "vmm.ping".
pub fn simple_api_full_command_with_fds_and_response<T: Read + Write + ScmSocket>(
    socket: &mut T,
    method: &str,
    full_command: &str,
    request_body: Option<&str>,
    request_fds: &[RawFd],
) -> Result<Option<String>, Error> {
    socket
        .send_with_fds(
            &[format!(
                "{method} /api/v1/{full_command} HTTP/1.1\r\nHost: localhost\r\nAccept: */*\r\n"
            )
            .as_bytes()],
            request_fds,
        )
        .map_err(Error::SocketSendFds)?;

    if let Some(request_body) = request_body {
        socket
            .write_all(format!("Content-Length: {}\r\n", request_body.len()).as_bytes())
            .map_err(Error::Socket)?;
    }

    socket.write_all(b"\r\n").map_err(Error::Socket)?;

    if let Some(request_body) = request_body {
        socket
            .write_all(request_body.as_bytes())
            .map_err(Error::Socket)?;
    }

    socket.flush().map_err(Error::Socket)?;

    parse_http_response(socket)
}

pub fn simple_api_full_command_with_fds<T: Read + Write + ScmSocket>(
    socket: &mut T,
    method: &str,
    full_command: &str,
    request_body: Option<&str>,
    request_fds: &[RawFd],
) -> Result<(), Error> {
    let response = simple_api_full_command_with_fds_and_response(
        socket,
        method,
        full_command,
        request_body,
        request_fds,
    )?;

    if let Some(response) = response {
        println!("{response}");
    }

    Ok(())
}

pub fn simple_api_full_command<T: Read + Write + ScmSocket>(
    socket: &mut T,
    method: &str,
    full_command: &str,
    request_body: Option<&str>,
) -> Result<(), Error> {
    simple_api_full_command_with_fds(socket, method, full_command, request_body, &[])
}

pub fn simple_api_command_with_fds<T: Read + Write + ScmSocket>(
    socket: &mut T,
    method: &str,
    c: &str,
    request_body: Option<&str>,
    request_fds: &[RawFd],
) -> Result<(), Error> {
    // Create the full VM command. For VMM commands, use
    // simple_api_full_command().
    let full_command = format!("vm.{c}");

    simple_api_full_command_with_fds(socket, method, &full_command, request_body, request_fds)
}

pub fn simple_api_command<T: Read + Write + ScmSocket>(
    socket: &mut T,
    method: &str,
    c: &str,
    request_body: Option<&str>,
) -> Result<(), Error> {
    simple_api_command_with_fds(socket, method, c, request_body, &[])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns at most one byte per read, splitting multi-byte characters.
    struct ByteReader<'a>(&'a [u8]);

    impl Read for ByteReader<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let Some((first, rest)) = self.0.split_first() else {
                return Ok(0);
            };
            buf[0] = *first;
            self.0 = rest;
            Ok(1)
        }
    }

    #[test]
    fn test_parse_http_response_split_utf8() {
        let body = "{\"path\":\"/tmp/d\u{e4}t\u{e9}n\"}";
        let response = format!(
            "HTTP/1.1 200 \r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );

        let parsed = parse_http_response(&mut ByteReader(response.as_bytes())).unwrap();

        assert_eq!(parsed.as_deref(), Some(body));
    }
}
