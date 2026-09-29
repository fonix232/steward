//! A ubus client in Rust: calls methods on ubusd's objects the way `ubus
//! call` does, with no libubus underneath.
//!
//! A ubus message is an 8-byte header (version 0, type, big-endian sequence
//! number and peer id), then one blob attribute holding the message's
//! attributes (see [`blob`]). ubusd greets a new connection with HELLO,
//! whose peer is the client's id. A call is LOOKUP (object path to id), then
//! INVOKE; the object answers with DATA messages and a closing STATUS, each
//! carrying the request's sequence number.

pub mod blob;

use serde_json::{Map, Value};
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;
use std::{fmt, fs};

pub const SOCKET: &str = "/var/run/ubus/ubus.sock";

mod msg {
    pub const HELLO: u8 = 0;
    pub const STATUS: u8 = 1;
    pub const DATA: u8 = 2;
    pub const LOOKUP: u8 = 4;
    pub const INVOKE: u8 = 5;
}

mod attr {
    pub const STATUS: u8 = 1;
    pub const OBJPATH: u8 = 2;
    pub const OBJID: u8 = 3;
    pub const METHOD: u8 = 4;
    pub const DATA: u8 = 7;
}

/// ubus's status codes (ubusmsg.h).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    InvalidCommand = 1,
    InvalidArgument,
    MethodNotFound,
    NotFound,
    NoData,
    PermissionDenied,
    Timeout,
    NotSupported,
    UnknownError,
    ConnectionFailed,
    NoMemory,
    ParseError,
    SystemError,
}

impl Status {
    fn from_code(code: u32) -> Status {
        use Status::*;
        [
            InvalidCommand,
            InvalidArgument,
            MethodNotFound,
            NotFound,
            NoData,
            PermissionDenied,
            Timeout,
            NotSupported,
            UnknownError,
            ConnectionFailed,
            NoMemory,
            ParseError,
            SystemError,
        ]
        .get((code as usize).wrapping_sub(1))
        .copied()
        .unwrap_or(UnknownError)
    }
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Malformed(blob::Malformed),
    /// ubusd or the object answered with a status other than OK.
    Status(Status),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "ubus: {e}"),
            Error::Malformed(e) => write!(f, "ubus: {e}"),
            Error::Status(s) => write!(f, "ubus: {s:?}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<blob::Malformed> for Error {
    fn from(e: blob::Malformed) -> Self {
        Error::Malformed(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// One message: its header's fields and its attributes, still encoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub kind: u8,
    pub seq: u16,
    pub peer: u32,
    pub body: Vec<u8>,
}

impl Message {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![0, self.kind];
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(&self.peer.to_be_bytes());
        blob::put(&mut out, 0, false, &self.body);
        out
    }

    pub fn read(r: &mut impl Read) -> Result<Message> {
        let mut hdr = [0u8; 12];
        r.read_exact(&mut hdr)?;
        if hdr[0] != 0 {
            return Err(blob::Malformed("unknown message version").into());
        }
        let len = (u32::from_be_bytes(hdr[8..12].try_into().unwrap()) & 0x00ff_ffff) as usize;
        if len < 4 {
            return Err(blob::Malformed("message length out of range").into());
        }
        let mut body = vec![0; len - 4];
        r.read_exact(&mut body)?;
        Ok(Message {
            kind: hdr[1],
            seq: u16::from_be_bytes([hdr[2], hdr[3]]),
            peer: u32::from_be_bytes(hdr[4..8].try_into().unwrap()),
            body,
        })
    }

    fn attr(&self, id: u8) -> Result<Option<&[u8]>> {
        Ok(blob::attrs(&self.body)?
            .into_iter()
            .find(|a| a.id == id)
            .map(|a| a.payload))
    }

    fn u32_attr(&self, id: u8) -> Result<Option<u32>> {
        Ok(self
            .attr(id)?
            .and_then(|p| p.get(..4))
            .map(|p| u32::from_be_bytes(p.try_into().unwrap())))
    }
}

/// A connection to ubusd. Calls are answered one at a time, in order.
pub struct Ubus {
    sock: UnixStream,
    seq: u16,
    id: u32,
}

impl Ubus {
    pub fn connect() -> Result<Ubus> {
        Self::connect_to(SOCKET)
    }

    pub fn connect_to(path: impl AsRef<Path>) -> Result<Ubus> {
        let mut sock = UnixStream::connect(path)?;
        sock.set_read_timeout(Some(Duration::from_secs(30)))?;
        let hello = Message::read(&mut sock)?;
        if hello.kind != msg::HELLO {
            return Err(blob::Malformed("no HELLO from ubusd").into());
        }
        Ok(Ubus {
            sock,
            seq: 0,
            id: hello.peer,
        })
    }

    /// This client's id on the bus.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// The id of the object at `path`.
    pub fn lookup(&mut self, path: &str) -> Result<u32> {
        let mut body = Vec::new();
        blob::put(
            &mut body,
            attr::OBJPATH,
            false,
            &[path.as_bytes(), b"\0"].concat(),
        );
        let mut id = None;
        self.request(msg::LOOKUP, 0, body, |m| {
            if id.is_none() {
                id = m.u32_attr(attr::OBJID)?;
            }
            Ok(())
        })?;
        id.ok_or(Error::Status(Status::NotFound))
    }

    /// Calls `method` on the object at `path` with `args` (a JSON object, as
    /// `ubus call` takes), and returns what it answered: an empty object when
    /// it answered with a status only.
    pub fn call(
        &mut self,
        path: &str,
        method: &str,
        args: &Map<String, Value>,
    ) -> Result<Map<String, Value>> {
        let obj = self.lookup(path)?;
        let mut body = Vec::new();
        blob::put(&mut body, attr::OBJID, false, &obj.to_be_bytes());
        blob::put(
            &mut body,
            attr::METHOD,
            false,
            &[method.as_bytes(), b"\0"].concat(),
        );
        let mut data = Vec::new();
        blob::put_table(&mut data, args);
        blob::put(&mut body, attr::DATA, false, &data);
        let mut out = Map::new();
        self.request(msg::INVOKE, obj, body, |m| {
            if let Some(d) = m.attr(attr::DATA)? {
                out.extend(blob::table(d)?);
            }
            Ok(())
        })?;
        Ok(out)
    }

    /// Sends one request and hands every DATA answer to `on_data` until its
    /// STATUS arrives.
    fn request(
        &mut self,
        kind: u8,
        peer: u32,
        body: Vec<u8>,
        mut on_data: impl FnMut(&Message) -> Result<()>,
    ) -> Result<()> {
        self.seq = self.seq.wrapping_add(1);
        let seq = self.seq;
        self.sock.write_all(
            &Message {
                kind,
                seq,
                peer,
                body,
            }
            .encode(),
        )?;
        loop {
            let m = Message::read(&mut self.sock)?;
            if m.seq != seq {
                continue; // an answer to a request given up on
            }
            match m.kind {
                msg::DATA => on_data(&m)?,
                msg::STATUS => {
                    return match m.u32_attr(attr::STATUS)?.unwrap_or(0) {
                        0 => Ok(()),
                        code => Err(Error::Status(Status::from_code(code))),
                    };
                }
                _ => {}
            }
        }
    }
}

/// Whether ubusd is there to connect to.
pub fn available() -> bool {
    fs::metadata(SOCKET).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;
    use std::os::unix::net::UnixListener;

    #[test]
    fn a_message_round_trips() {
        let mut body = Vec::new();
        blob::put(&mut body, attr::STATUS, false, &4u32.to_be_bytes());
        let m = Message {
            kind: msg::STATUS,
            seq: 0x1234,
            peer: 0xdeadbeef,
            body,
        };
        let wire = m.encode();
        assert_eq!(&wire[..8], &[0, 1, 0x12, 0x34, 0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(Message::read(&mut Cursor::new(wire)).unwrap(), m);
    }

    /// A stand-in for ubusd that knows one object, "uci", whose "get" echoes
    /// its arguments back under "values".
    fn fake_ubusd(path: &Path) {
        let listener = UnixListener::bind(path).unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            s.write_all(
                &Message {
                    kind: msg::HELLO,
                    seq: 0,
                    peer: 0x42,
                    body: vec![],
                }
                .encode(),
            )
            .unwrap();
            while let Ok(m) = Message::read(&mut s) {
                let status = |code: u32| {
                    let mut b = Vec::new();
                    blob::put(&mut b, attr::STATUS, false, &code.to_be_bytes());
                    Message {
                        kind: msg::STATUS,
                        seq: m.seq,
                        peer: m.peer,
                        body: b,
                    }
                    .encode()
                };
                match m.kind {
                    msg::LOOKUP if m.attr(attr::OBJPATH).unwrap() == Some(b"uci\0") => {
                        let mut b = Vec::new();
                        blob::put(&mut b, attr::OBJPATH, false, b"uci\0");
                        blob::put(&mut b, attr::OBJID, false, &7u32.to_be_bytes());
                        s.write_all(
                            &Message {
                                kind: msg::DATA,
                                seq: m.seq,
                                peer: 0,
                                body: b,
                            }
                            .encode(),
                        )
                        .unwrap();
                        s.write_all(&status(0)).unwrap();
                    }
                    msg::LOOKUP => s.write_all(&status(4)).unwrap(),
                    msg::INVOKE => {
                        assert_eq!(m.u32_attr(attr::OBJID).unwrap(), Some(7));
                        if m.attr(attr::METHOD).unwrap() != Some(b"get\0") {
                            s.write_all(&status(3)).unwrap();
                            continue;
                        }
                        let args = blob::table(m.attr(attr::DATA).unwrap().unwrap()).unwrap();
                        let mut data = Vec::new();
                        blob::put_table(&mut data, json!({ "values": args }).as_object().unwrap());
                        let mut b = Vec::new();
                        blob::put(&mut b, attr::DATA, false, &data);
                        s.write_all(
                            &Message {
                                kind: msg::DATA,
                                seq: m.seq,
                                peer: 7,
                                body: b,
                            }
                            .encode(),
                        )
                        .unwrap();
                        s.write_all(&status(0)).unwrap();
                    }
                    _ => s.write_all(&status(1)).unwrap(),
                }
            }
        });
    }

    #[test]
    fn a_call_goes_through_lookup_and_invoke() {
        let dir = std::env::temp_dir().join(format!("steward-ubus-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ubus.sock");
        fake_ubusd(&path);

        let mut ubus = Ubus::connect_to(&path).unwrap();
        assert_eq!(ubus.id(), 0x42);
        let args = json!({ "config": "network", "section": "lan" });
        let got = ubus.call("uci", "get", args.as_object().unwrap()).unwrap();
        assert_eq!(Value::Object(got), json!({ "values": args }));
        assert!(matches!(
            ubus.call("uci", "set", &Map::new()),
            Err(Error::Status(Status::MethodNotFound))
        ));
        assert!(matches!(
            ubus.call("nope", "get", &Map::new()),
            Err(Error::Status(Status::NotFound))
        ));
        let _ = fs::remove_dir_all(&dir);
    }
}
