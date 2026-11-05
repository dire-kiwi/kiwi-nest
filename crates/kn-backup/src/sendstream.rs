//! The `btrfs send` stream format, version 1, as the kernel writes it.
//!
//! A stream is a header (`btrfs-stream\0` and a little-endian `u32` version), then commands until an `END`
//! command. Each command is a 10-byte header (`u32` length of its attributes, `u16` command, `u32` CRC-32C of the
//! whole command with this field zeroed), then attributes, each a `u16` type, a `u16` length and the value. Streams
//! can follow one another in one input; kiwi-nest puts one per volume in each backup.

use std::io::{self, Read, Write};

pub const MAGIC: &[u8; 13] = b"btrfs-stream\0";

/// Commands.
pub mod cmd {
    pub const SUBVOL: u16 = 1;
    pub const SNAPSHOT: u16 = 2;
    pub const MKFILE: u16 = 3;
    pub const MKDIR: u16 = 4;
    pub const MKNOD: u16 = 5;
    pub const MKFIFO: u16 = 6;
    pub const MKSOCK: u16 = 7;
    pub const SYMLINK: u16 = 8;
    pub const RENAME: u16 = 9;
    pub const LINK: u16 = 10;
    pub const UNLINK: u16 = 11;
    pub const RMDIR: u16 = 12;
    pub const SET_XATTR: u16 = 13;
    pub const REMOVE_XATTR: u16 = 14;
    pub const WRITE: u16 = 15;
    pub const CLONE: u16 = 16;
    pub const TRUNCATE: u16 = 17;
    pub const CHMOD: u16 = 18;
    pub const CHOWN: u16 = 19;
    pub const UTIMES: u16 = 20;
    pub const END: u16 = 21;
    pub const UPDATE_EXTENT: u16 = 22;
}

/// Attributes.
pub mod attr {
    pub const UUID: u16 = 1;
    pub const CTRANSID: u16 = 2;
    pub const INO: u16 = 3;
    pub const SIZE: u16 = 4;
    pub const MODE: u16 = 5;
    pub const UID: u16 = 6;
    pub const GID: u16 = 7;
    pub const RDEV: u16 = 8;
    pub const CTIME: u16 = 9;
    pub const MTIME: u16 = 10;
    pub const ATIME: u16 = 11;
    pub const OTIME: u16 = 12;
    pub const XATTR_NAME: u16 = 13;
    pub const XATTR_DATA: u16 = 14;
    pub const PATH: u16 = 15;
    pub const PATH_TO: u16 = 16;
    pub const PATH_LINK: u16 = 17;
    pub const FILE_OFFSET: u16 = 18;
    pub const DATA: u16 = 19;
    pub const CLONE_UUID: u16 = 20;
    pub const CLONE_CTRANSID: u16 = 21;
    pub const CLONE_PATH: u16 = 22;
    pub const CLONE_OFFSET: u16 = 23;
    pub const CLONE_LEN: u16 = 24;
}

/// The largest command the kernel writes in version 1 is 64 KiB; anything much larger isn't a real stream.
const MAX_COMMAND: usize = 1 << 20;

const fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0x82f6_3b78 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static CRC: [u32; 256] = crc_table();

/// CRC-32C as btrfs computes it: seed 0, and no final inversion.
pub fn crc32c(seed: u32, data: &[u8]) -> u32 {
    let mut c = seed;
    for b in data {
        c = CRC[((c ^ u32::from(*b)) & 0xff) as usize] ^ (c >> 8);
    }
    c
}

/// One command and its attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub cmd: u16,
    pub attrs: Vec<(u16, Vec<u8>)>,
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

impl Command {
    fn get(&self, a: u16) -> io::Result<&[u8]> {
        self.attrs
            .iter()
            .find(|(t, _)| *t == a)
            .map(|(_, v)| v.as_slice())
            .ok_or_else(|| bad(format!("command {} has no attribute {a}", self.cmd)))
    }

    pub fn bytes(&self, a: u16) -> io::Result<&[u8]> {
        self.get(a)
    }

    pub fn has(&self, a: u16) -> bool {
        self.attrs.iter().any(|(t, _)| *t == a)
    }

    pub fn u64(&self, a: u16) -> io::Result<u64> {
        let v = self.get(a)?;
        let b: [u8; 8] = v.try_into().map_err(|_| bad(format!("attribute {a} isn't 8 bytes")))?;
        Ok(u64::from_le_bytes(b))
    }

    pub fn uuid(&self, a: u16) -> io::Result<[u8; 16]> {
        let v = self.get(a)?;
        v.try_into().map_err(|_| bad(format!("attribute {a} isn't a UUID")))
    }

    /// A timespec: seconds and nanoseconds.
    pub fn time(&self, a: u16) -> io::Result<(i64, u32)> {
        let v = self.get(a)?;
        if v.len() != 12 {
            return Err(bad(format!("attribute {a} isn't a timespec")));
        }
        let s = i64::from_le_bytes(v[..8].try_into().expect("8 bytes"));
        let ns = u32::from_le_bytes(v[8..].try_into().expect("4 bytes"));
        Ok((s, ns))
    }

    pub fn path(&self, a: u16) -> io::Result<&[u8]> {
        self.get(a)
    }
}

/// Reads commands from one input that holds streams one after another.
pub struct Reader<R> {
    r: R,
    in_stream: bool,
}

/// Reads exactly `buf.len()` bytes, or returns false if the input ends before the first.
fn read_or_end(r: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]) {
            Ok(0) if got == 0 => return Ok(false),
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the stream ends part way")),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

impl<R: Read> Reader<R> {
    pub fn new(r: R) -> Self {
        Reader { r, in_stream: false }
    }

    /// Starts the next stream: `false` at the end of the input.
    pub fn next_stream(&mut self) -> io::Result<bool> {
        assert!(!self.in_stream, "the current stream hasn't ended");
        let mut h = [0u8; 17];
        if !read_or_end(&mut self.r, &mut h)? {
            return Ok(false);
        }
        if &h[..13] != MAGIC {
            return Err(bad("not a btrfs send stream"));
        }
        let version = u32::from_le_bytes(h[13..].try_into().expect("4 bytes"));
        if version != 1 {
            return Err(bad(format!("btrfs send stream version {version} isn't supported")));
        }
        self.in_stream = true;
        Ok(true)
    }

    /// The next command of the current stream; the stream ends with `END`.
    pub fn next_command(&mut self) -> io::Result<Command> {
        let mut h = [0u8; 10];
        if !read_or_end(&mut self.r, &mut h)? {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the stream ends without END",
            ));
        }
        let len = u32::from_le_bytes(h[..4].try_into().expect("4 bytes")) as usize;
        let cmd = u16::from_le_bytes(h[4..6].try_into().expect("2 bytes"));
        let crc = u32::from_le_bytes(h[6..].try_into().expect("4 bytes"));
        if len > MAX_COMMAND {
            return Err(bad(format!("a command of {len} bytes")));
        }
        let mut body = vec![0u8; len];
        if len > 0 && !read_or_end(&mut self.r, &mut body)? {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the stream ends part way"));
        }
        h[6..].fill(0);
        if crc32c(crc32c(0, &h), &body) != crc {
            return Err(bad(format!("command {cmd} fails its checksum")));
        }
        let mut attrs = Vec::new();
        let mut at = 0;
        while at < body.len() {
            if body.len() - at < 4 {
                return Err(bad("a truncated attribute"));
            }
            let t = u16::from_le_bytes(body[at..at + 2].try_into().expect("2 bytes"));
            let l = u16::from_le_bytes(body[at + 2..at + 4].try_into().expect("2 bytes")) as usize;
            at += 4;
            if body.len() - at < l {
                return Err(bad("a truncated attribute"));
            }
            attrs.push((t, body[at..at + l].to_vec()));
            at += l;
        }
        if cmd == cmd::END {
            self.in_stream = false;
        }
        Ok(Command { cmd, attrs })
    }
}

/// Writes streams, for tests: what the kernel would.
pub struct Writer<W> {
    w: W,
}

impl<W: Write> Writer<W> {
    pub fn new(mut w: W) -> io::Result<Self> {
        w.write_all(MAGIC)?;
        w.write_all(&1u32.to_le_bytes())?;
        Ok(Writer { w })
    }

    pub fn command(&mut self, cmd: u16, attrs: &[(u16, &[u8])]) -> io::Result<()> {
        let mut body = Vec::new();
        for (t, v) in attrs {
            body.extend_from_slice(&t.to_le_bytes());
            body.extend_from_slice(&(v.len() as u16).to_le_bytes());
            body.extend_from_slice(v);
        }
        let mut h = [0u8; 10];
        h[..4].copy_from_slice(&(body.len() as u32).to_le_bytes());
        h[4..6].copy_from_slice(&cmd.to_le_bytes());
        let crc = crc32c(crc32c(0, &h), &body);
        h[6..].copy_from_slice(&crc.to_le_bytes());
        self.w.write_all(&h)?;
        self.w.write_all(&body)
    }

    pub fn into_inner(self) -> W {
        self.w
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32c_matches_the_reference() {
        // The standard check value is for the inverted form: seed !0 and a final inversion.
        assert_eq!(!crc32c(!0, b"123456789"), 0xe306_9283);
    }

    #[test]
    fn streams_round_trip_and_damage_is_caught() {
        let mut w = Writer::new(Vec::new()).unwrap();
        w.command(
            cmd::SUBVOL,
            &[
                (attr::PATH, b"data"),
                (attr::UUID, &[1; 16]),
                (attr::CTRANSID, &7u64.to_le_bytes()),
            ],
        )
        .unwrap();
        w.command(cmd::END, &[]).unwrap();
        let one = w.into_inner();
        let two = [one.clone(), one.clone()].concat();
        let mut r = Reader::new(&two[..]);
        for _ in 0..2 {
            assert!(r.next_stream().unwrap());
            let c = r.next_command().unwrap();
            assert_eq!((c.cmd, c.path(attr::PATH).unwrap()), (cmd::SUBVOL, &b"data"[..]));
            assert_eq!(c.u64(attr::CTRANSID).unwrap(), 7);
            assert_eq!(r.next_command().unwrap().cmd, cmd::END);
        }
        assert!(!r.next_stream().unwrap());

        let mut damaged = one.clone();
        let n = damaged.len();
        damaged[n - 15] ^= 1;
        let mut r = Reader::new(&damaged[..]);
        r.next_stream().unwrap();
        assert!(r.next_command().unwrap_err().to_string().contains("checksum"));
        let mut r = Reader::new(&one[..one.len() - 3]);
        r.next_stream().unwrap();
        r.next_command().unwrap();
        assert!(r.next_command().is_err());
        assert!(Reader::new(&b"not a stream at all"[..]).next_stream().is_err());
    }
}
