//! Bounded streaming observation of initial Git object transfers, not advertisements/fetches.
//! Only bounded packet prefixes are inspected; no protocol data is persisted.
//! Both HTTP and SSH use this observer.
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Context;
use std::task::Poll;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::ReadBuf;

#[derive(Default, Clone)]
pub struct Observation(Arc<Mutex<State>>);
#[derive(Default)]
struct State {
    input: Packets,
    output: Packets,
    want: bool,
    have: bool,
    initial_pack: bool,
}
#[derive(Default)]
struct Events {
    want: bool,
    have: bool,
    pack: bool,
}
struct Packets {
    header: [u8; 4],
    header_len: usize,
    remaining: usize,
    prefix: [u8; 80],
    prefix_len: usize,
    invalid: bool,
    raw_pack: bool,
}
impl Default for Packets {
    fn default() -> Self {
        Self {
            header: [0; 4],
            header_len: 0,
            remaining: 0,
            prefix: [0; 80],
            prefix_len: 0,
            invalid: false,
            raw_pack: false,
        }
    }
}
impl Packets {
    fn feed(&mut self, mut bytes: &[u8], output: bool) -> Events {
        let mut events = Events::default();
        while !bytes.is_empty() && !self.invalid && !self.raw_pack {
            if self.remaining > 0 {
                let n = self.remaining.min(bytes.len());
                let keep = n.min(self.prefix.len() - self.prefix_len);
                self.prefix[self.prefix_len..self.prefix_len + keep]
                    .copy_from_slice(&bytes[..keep]);
                self.prefix_len += keep;
                self.remaining -= n;
                bytes = &bytes[n..];
                if self.remaining == 0 {
                    let prefix = &self.prefix[..self.prefix_len];
                    if output {
                        events.pack |= prefix.starts_with(b"\x01PACK");
                    } else {
                        events.want |=
                            prefix.starts_with(b"want ") || prefix.starts_with(b"want-ref ");
                        events.have |= prefix.starts_with(b"have ");
                    }
                }
                continue;
            }
            let n = (4 - self.header_len).min(bytes.len());
            self.header[self.header_len..self.header_len + n].copy_from_slice(&bytes[..n]);
            self.header_len += n;
            bytes = &bytes[n..];
            if self.header_len < 4 {
                continue;
            }
            self.header_len = 0;
            if output && self.header == *b"PACK" {
                self.raw_pack = true;
                events.pack = true;
                break;
            }
            let length = self.header.iter().try_fold(0_usize, |value, c| {
                char::from(*c)
                    .to_digit(16)
                    .map(|digit| value * 16 + digit as usize)
            });
            match length {
                Some(0..=2) => {}
                Some(length @ 4..=65520) => {
                    self.remaining = length - 4;
                    self.prefix_len = 0;
                }
                _ => self.invalid = true,
            }
        }
        events
    }
}
impl Observation {
    pub fn input<R>(&self, inner: R) -> Input<R> {
        Input {
            inner,
            observation: self.clone(),
        }
    }
    pub fn output<W>(&self, inner: W) -> Output<W> {
        Output {
            inner,
            observation: self.clone(),
        }
    }
    fn incoming(&self, bytes: &[u8]) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let events = state.input.feed(bytes, false);
        state.want |= events.want;
        state.have |= events.have;
    }
    pub fn outgoing(&self, bytes: &[u8]) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let events = state.output.feed(bytes, true);
        if events.pack && state.want && !state.have {
            state.initial_pack = true;
        }
    }
    /// Call only after the real Git service has completed successfully.
    pub fn initial_transfer(&self) -> bool {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.initial_pack
            && !state.input.invalid
            && !state.output.invalid
            && state.input.remaining == 0
            && state.input.header_len == 0
    }
}
pub struct Input<R> {
    inner: R,
    observation: Observation,
}
impl<R: AsyncRead + Unpin> AsyncRead for Input<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let previous = buffer.filled().len();
        let result = Pin::new(&mut this.inner).poll_read(context, buffer);
        if matches!(result, Poll::Ready(Ok(()))) {
            this.observation.incoming(&buffer.filled()[previous..]);
        }
        result
    }
}
pub struct Output<W> {
    inner: W,
    observation: Observation,
}
impl<W: AsyncWrite + Unpin> AsyncWrite for Output<W> {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write(context, bytes);
        if let Poll::Ready(Ok(n)) = result {
            this.observation.outgoing(&bytes[..n]);
        }
        result
    }
    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(context)
    }
    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn packet(data: &[u8]) -> Vec<u8> {
        let mut packet = format!("{:04x}", data.len() + 4).into_bytes();
        packet.extend_from_slice(data);
        packet
    }
    #[test]
    fn advertisements_and_negotiated_fetches_are_not_initial_transfers() {
        let refs = packet(b"1111111111111111111111111111111111111111 refs/heads/PACK\n");
        let observed = Observation::default();
        observed.outgoing(&refs);
        assert!(!observed.initial_transfer());
        let observed = Observation::default();
        observed.incoming(&packet(b"want 1111111111111111111111111111111111111111\n"));
        observed.incoming(&packet(b"have 2222222222222222222222222222222222222222\n"));
        observed.incoming(b"0000");
        observed.outgoing(b"PACKbinary");
        assert!(!observed.initial_transfer());
    }
    #[test]
    fn fragmented_raw_and_sideband_packs_follow_real_wants() {
        for pack in [b"PACKbinary".to_vec(), packet(b"\x01PACKbinary")] {
            let observed = Observation::default();
            for byte in packet(b"want 1111111111111111111111111111111111111111\n")
                .iter()
                .chain(b"0000")
            {
                observed.incoming(&[*byte]);
            }
            for byte in &pack {
                observed.outgoing(&[*byte]);
            }
            assert!(observed.initial_transfer());
        }
        let observed = Observation::default();
        observed.incoming(&packet(b"want 1111111111111111111111111111111111111111\n"));
        observed.incoming(b"ffff");
        observed.outgoing(b"PACK");
        assert!(!observed.initial_transfer());
    }
}
