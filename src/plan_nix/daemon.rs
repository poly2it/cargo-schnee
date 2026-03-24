use anyhow::Result;
use log::debug;

const WORKER_MAGIC_1: u64 = 0x6e697863;
const WORKER_MAGIC_2: u64 = 0x6478696f;
const STDERR_NEXT: u64 = 0x6f6c6d67;
const STDERR_LAST: u64 = 0x616c7473;
const STDERR_ERROR: u64 = 0x63787470;

pub(super) struct NixDaemonConn {
    stream: std::os::unix::net::UnixStream,
}

impl NixDaemonConn {
    pub(super) fn connect() -> Result<Self> {
        use anyhow::Context;
        let socket_path = std::env::var("NIX_DAEMON_SOCKET_PATH")
            .unwrap_or_else(|_| "/nix/var/nix/daemon-socket/socket".to_string());
        let stream = std::os::unix::net::UnixStream::connect(&socket_path).with_context(|| {
            format!(
                "Failed to connect to Nix daemon at {}. \
                     Ensure the daemon is running (e.g. 'sudo systemctl start nix-daemon').",
                socket_path
            )
        })?;
        let timeout = Some(std::time::Duration::from_secs(30));
        stream.set_read_timeout(timeout)?;
        stream.set_write_timeout(timeout)?;
        let mut conn = Self { stream };
        conn.handshake()?;
        Ok(conn)
    }

    fn handshake(&mut self) -> Result<()> {
        use std::io::Write;

        self.write_u64(WORKER_MAGIC_1)?;
        self.stream.flush()?;

        let magic = self.read_u64()?;
        if magic != WORKER_MAGIC_2 {
            anyhow::bail!("Unexpected Nix daemon magic: 0x{:x}", magic);
        }

        let proto_version = self.read_u64()?;
        let major = proto_version >> 8;
        let minor = proto_version & 0xff;
        debug!("Nix daemon protocol version: {}.{}", major, minor);

        // Send client version (1.37)
        let client_version: u64 = (1 << 8) | 37;
        self.write_u64(client_version)?;

        let version = std::cmp::min(proto_version, client_version);
        debug!(
            "Negotiated protocol version: {}.{}",
            version >> 8,
            version & 0xff
        );

        // Protocol >= 1.14: send obsolete CPU affinity
        if version >= (1 << 8) | 14 {
            self.write_u64(0)?;
        }

        // Protocol >= 1.11: send obsolete reserve space
        if version >= (1 << 8) | 11 {
            self.write_u64(0)?;
        }

        self.stream.flush()?;

        // Protocol >= 1.33: daemon sends its version string
        if version >= (1 << 8) | 33 {
            let ver = self.read_string()?;
            debug!("Daemon version: {}", ver);
        }

        // Protocol >= 1.35: daemon sends trust level
        if version >= (1 << 8) | 35 {
            let trust = self.read_u64()?;
            debug!("Daemon trust level: {}", trust);
        }

        // Read startup OK (STDERR_LAST)
        self.process_stderr()?;

        Ok(())
    }

    /// Check whether a store path is valid (registered in the Nix DB).
    /// Uses wopIsValidPath (opcode 1).
    pub(super) fn is_valid_path(&mut self, path: &str) -> Result<bool> {
        use std::io::Write;

        self.write_u64(1)?; // wopIsValidPath
        self.write_string(path)?;
        self.stream.flush()?;

        self.process_stderr()?;
        Ok(self.read_u64()? != 0)
    }

    /// Register a text file in the Nix store (wopAddTextToStore, opcode 8).
    /// Returns the resulting store path.
    pub(super) fn add_text_to_store(
        &mut self,
        name: &str,
        content: &[u8],
        refs: &[&str],
    ) -> Result<String> {
        use std::io::Write;

        self.write_u64(8)?; // wopAddTextToStore
        self.write_string(name)?;
        self.write_bytes(content)?;
        self.write_string_list(refs)?;
        self.stream.flush()?;

        self.process_stderr()?;
        self.read_string()
    }

    fn write_u64(&mut self, val: u64) -> Result<()> {
        use std::io::Write;
        self.stream.write_all(&val.to_le_bytes())?;
        Ok(())
    }

    fn read_u64(&mut self) -> Result<u64> {
        use std::io::Read;
        let mut buf = [0u8; 8];
        self.stream.read_exact(&mut buf)?;
        Ok(u64::from_le_bytes(buf))
    }

    fn write_string(&mut self, s: &str) -> Result<()> {
        self.write_bytes(s.as_bytes())
    }

    fn write_bytes(&mut self, data: &[u8]) -> Result<()> {
        use std::io::Write;
        self.write_u64(data.len() as u64)?;
        self.stream.write_all(data)?;
        let padding = (8 - (data.len() % 8)) % 8;
        if padding > 0 {
            self.stream.write_all(&[0u8; 8][..padding])?;
        }
        Ok(())
    }

    fn read_string(&mut self) -> Result<String> {
        use std::io::Read;
        let len = self.read_u64()? as usize;
        if len > 64 * 1024 * 1024 {
            anyhow::bail!(
                "Nix daemon sent string of {} bytes, exceeding 64 MiB limit",
                len
            );
        }
        let mut buf = vec![0u8; len];
        self.stream.read_exact(&mut buf)?;
        let padding = (8 - (len % 8)) % 8;
        if padding > 0 {
            let mut pad = [0u8; 8];
            self.stream.read_exact(&mut pad[..padding])?;
        }
        Ok(String::from_utf8(buf)?)
    }

    fn write_string_list(&mut self, list: &[&str]) -> Result<()> {
        self.write_u64(list.len() as u64)?;
        for s in list {
            self.write_string(s)?;
        }
        Ok(())
    }

    /// Read stderr protocol messages until STDERR_LAST (success).
    fn process_stderr(&mut self) -> Result<()> {
        loop {
            let tag = self.read_u64()?;
            match tag {
                STDERR_LAST => return Ok(()),
                STDERR_NEXT => {
                    let _msg = self.read_string()?;
                }
                STDERR_ERROR => {
                    let _level = self.read_u64()?;
                    let typ = self.read_string()?;
                    let msg = self.read_string()?;
                    let _have_pos = self.read_u64()?;
                    let n_traces = self.read_u64()?;
                    for _ in 0..n_traces {
                        let _have_pos = self.read_u64()?;
                        let _trace = self.read_string()?;
                    }
                    anyhow::bail!("Nix daemon error ({}): {}", typ, msg);
                }
                _ => {
                    anyhow::bail!("Unknown Nix daemon stderr tag: 0x{:x}", tag);
                }
            }
        }
    }
}
