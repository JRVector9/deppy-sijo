//! Private bounded OpenSSH askpass IPC. Authentication values have no printable representation.
use std::io::{self, Read, Write};

pub const MAX_PROMPT_BYTES: usize = 8192;
pub const MAX_SECRET_BYTES: usize = 4096;
pub const MAX_REQUEST_BYTES: usize = MAX_PROMPT_BYTES + 256;
pub const SOCKET_ENV: &str = "DEPPY_SSH_ASKPASS_SOCKET";
pub const TOKEN_ENV: &str = "DEPPY_SSH_ASKPASS_TOKEN";

pub struct Secret(Vec<u8>);
impl Secret {
    pub fn new(bytes: Vec<u8>) -> io::Result<Self> {
        let secret = Self(bytes);
        if secret.0.len() > MAX_SECRET_BYTES
            || secret.0.iter().any(|b| matches!(b, 0 | b'\n' | b'\r'))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid authentication response",
            ));
        }
        Ok(secret)
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}
impl Drop for Secret {
    fn drop(&mut self) {
        for byte in &mut self.0 {
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

pub fn encode_request(token: &str, prompt: &str, confirmation: bool) -> io::Result<Vec<u8>> {
    if token.is_empty()
        || token.len() > 128
        || !token.bytes().all(|b| b.is_ascii_hexdigit())
        || prompt.len() > MAX_PROMPT_BYTES
        || prompt.contains('\0')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid askpass request",
        ));
    }
    let mut bytes = vec![u8::from(confirmation), token.len() as u8];
    bytes.extend_from_slice(token.as_bytes());
    bytes.extend_from_slice(prompt.as_bytes());
    Ok(bytes)
}
pub fn decode_request(bytes: &[u8]) -> io::Result<(String, String, bool)> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid askpass request");
    if bytes.len() < 2
        || bytes[0] > 1
        || bytes[1] == 0
        || bytes[1] > 128
        || bytes.len() < 2 + bytes[1] as usize
    {
        return Err(invalid());
    }
    let boundary = 2 + bytes[1] as usize;
    let token = std::str::from_utf8(&bytes[2..boundary]).map_err(|_| invalid())?;
    let prompt = std::str::from_utf8(&bytes[boundary..]).map_err(|_| invalid())?;
    encode_request(token, prompt, bytes[0] == 1).map_err(|_| invalid())?;
    Ok((token.to_owned(), prompt.to_owned(), bytes[0] == 1))
}
pub fn write_frame(writer: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "askpass frame too large"))?;
    writer.write_all(&len.to_be_bytes())?;
    writer.write_all(payload)
}
pub fn read_frame(reader: &mut impl Read, max: usize) -> io::Result<Vec<u8>> {
    let mut len = [0; 4];
    reader.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "askpass frame too large",
        ));
    }
    let mut bytes = vec![0; len];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// Called only by the bundled helper process. No value is written to stderr.
#[cfg(unix)]
pub fn run_from_environment(prompt: &str, stdout: &mut impl Write) -> io::Result<()> {
    use std::{os::unix::net::UnixStream, time::Duration};
    let socket =
        std::env::var_os(SOCKET_ENV).ok_or_else(|| io::Error::other("missing askpass endpoint"))?;
    let token =
        std::env::var(TOKEN_ENV).map_err(|_| io::Error::other("missing askpass capability"))?;
    let confirmation = std::env::var("SSH_ASKPASS_PROMPT").is_ok_and(|value| value == "confirm")
        || prompt.contains("(yes/no");
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(90)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    write_frame(&mut stream, &encode_request(&token, prompt, confirmation)?)?;
    let secret = Secret::new(read_frame(&mut stream, MAX_SECRET_BYTES)?)?;
    stdout.write_all(secret.as_bytes())?;
    stdout.write_all(b"\n")?;
    stdout.flush()
}
