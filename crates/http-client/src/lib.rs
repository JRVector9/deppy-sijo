//! Shared synchronous HTTP transport policy.

use std::time::Duration;
use ureq::unversioned::resolver::DefaultResolver;
use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector, DefaultConnector, NextTimeout, Transport,
};

/// Construct an HTTP agent with idle limits on individual socket operations.
pub fn agent_with_idle_timeouts(
    config: ureq::config::Config,
    read_idle: Duration,
    write_idle: Option<Duration>,
) -> ureq::Agent {
    ureq::Agent::with_parts(
        config,
        DefaultConnector::default().chain(IdleConnector {
            read_idle,
            write_idle,
        }),
        DefaultResolver::default(),
    )
}

// ureq is pinned because its public transport extension is not semver-stable.
// Wrapping after the default connector preserves proxy/TLS negotiation and keeps
// the idle budget local to each I/O operation, not the lifetime of an SSE body.
#[derive(Debug)]
struct IdleConnector {
    read_idle: Duration,
    write_idle: Option<Duration>,
}

impl<T: Transport> Connector<T> for IdleConnector {
    type Out = IdleTransport<T>;

    fn connect(
        &self,
        _: &ConnectionDetails,
        chained: Option<T>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        Ok(chained.map(|inner| IdleTransport {
            inner,
            read_idle: self.read_idle,
            write_idle: self.write_idle,
        }))
    }
}

#[derive(Debug)]
struct IdleTransport<T> {
    inner: T,
    read_idle: Duration,
    write_idle: Option<Duration>,
}

fn clamp_timeout(
    timeout: NextTimeout,
    idle: Option<Duration>,
    reason: ureq::Timeout,
) -> NextTimeout {
    match idle {
        Some(idle) if ureq::unversioned::transport::time::Duration::from(idle) < timeout.after => {
            NextTimeout {
                after: idle.into(),
                reason,
            }
        }
        _ => timeout,
    }
}

impl<T: Transport> Transport for IdleTransport<T> {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.inner.buffers()
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.inner.transmit_output(
            amount,
            clamp_timeout(timeout, self.write_idle, ureq::Timeout::SendBody),
        )
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        self.inner.await_input(clamp_timeout(
            timeout,
            Some(self.read_idle),
            ureq::Timeout::RecvBody,
        ))
    }

    fn is_open(&mut self) -> bool {
        self.inner.is_open()
    }
    fn is_tls(&self) -> bool {
        self.inner.is_tls()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn shorter_deadline_and_its_reason_are_preserved() {
        let timeout = NextTimeout {
            after: Duration::from_millis(10).into(),
            reason: ureq::Timeout::Global,
        };
        assert_eq!(
            clamp_timeout(
                timeout,
                Some(Duration::from_millis(20)),
                ureq::Timeout::RecvBody
            ),
            timeout
        );
        assert_eq!(
            clamp_timeout(
                timeout,
                Some(Duration::from_millis(10)),
                ureq::Timeout::RecvBody
            ),
            timeout
        );
        assert_eq!(
            clamp_timeout(timeout, None, ureq::Timeout::SendBody),
            timeout
        );
    }

    #[test]
    fn idle_budget_caps_an_unlimited_operation() {
        let timeout = NextTimeout {
            after: ureq::unversioned::transport::time::Duration::NotHappening,
            reason: ureq::Timeout::Global,
        };
        let capped = clamp_timeout(
            timeout,
            Some(Duration::from_millis(20)),
            ureq::Timeout::RecvBody,
        );
        assert_eq!(capped.after, Duration::from_millis(20).into());
        assert_eq!(capped.reason, ureq::Timeout::RecvBody);
    }

    #[derive(Debug)]
    struct RecordingTransport {
        buffers: ureq::unversioned::transport::LazyBuffers,
        last_timeout: Option<NextTimeout>,
        last_amount: usize,
    }

    impl Transport for RecordingTransport {
        fn buffers(&mut self) -> &mut dyn Buffers {
            &mut self.buffers
        }
        fn transmit_output(
            &mut self,
            amount: usize,
            timeout: NextTimeout,
        ) -> Result<(), ureq::Error> {
            self.last_amount = amount;
            self.last_timeout = Some(timeout);
            Ok(())
        }
        fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
            self.last_timeout = Some(timeout);
            Ok(true)
        }
        fn is_open(&mut self) -> bool {
            true
        }
        fn is_tls(&self) -> bool {
            true
        }
    }

    #[test]
    fn wrapper_preserves_tls_buffers_and_applies_independent_io_budgets() {
        let mut transport = IdleTransport {
            inner: RecordingTransport {
                buffers: ureq::unversioned::transport::LazyBuffers::new(1024, 1024),
                last_timeout: None,
                last_amount: 0,
            },
            read_idle: Duration::from_millis(20),
            write_idle: Some(Duration::from_millis(30)),
        };
        assert!(transport.is_tls());
        assert!(transport.is_open());
        transport.buffers().output()[0] = 42;
        assert_eq!(transport.inner.buffers().output()[0], 42);
        let timeout = NextTimeout {
            after: Duration::from_secs(1).into(),
            reason: ureq::Timeout::Global,
        };
        transport.transmit_output(1, timeout).unwrap();
        assert_eq!(transport.inner.last_amount, 1);
        assert_eq!(
            transport.inner.last_timeout.unwrap().after,
            Duration::from_millis(30).into()
        );
        assert_eq!(
            transport.inner.last_timeout.unwrap().reason,
            ureq::Timeout::SendBody
        );
        assert!(transport.await_input(timeout).unwrap());
        assert_eq!(
            transport.inner.last_timeout.unwrap().after,
            Duration::from_millis(20).into()
        );
        assert_eq!(
            transport.inner.last_timeout.unwrap().reason,
            ureq::Timeout::RecvBody
        );
    }

    fn read_request(stream: &mut std::net::TcpStream) {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
            assert!(request.len() <= 4096);
        }
    }

    #[test]
    fn silent_body_exits_on_read_idle_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/idle", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&mut stream);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\n")
                .unwrap();
            thread::sleep(Duration::from_millis(600));
            let _ = stream.write_all(b"x");
        });
        let agent = agent_with_idle_timeouts(
            ureq::Agent::config_builder().build(),
            Duration::from_millis(150),
            None,
        );
        let response = agent.get(&url).call().unwrap();
        let outcome = response
            .into_body()
            .into_reader()
            .read_to_end(&mut Vec::new());
        server.join().unwrap();
        assert!(
            outcome.is_err(),
            "a silent socket must time out instead of waiting for its later byte"
        );
    }

    #[test]
    fn progressing_body_can_outlive_read_idle_budget() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/stream", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&mut stream);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\n")
                .unwrap();
            for _ in 0..12 {
                thread::sleep(Duration::from_millis(40));
                if stream.write_all(b"x").is_err() {
                    break;
                }
            }
        });
        let agent = agent_with_idle_timeouts(
            ureq::Agent::config_builder().build(),
            Duration::from_millis(200),
            None,
        );
        let response = agent.get(&url).call().unwrap();
        let mut body = Vec::new();
        let outcome = response.into_body().into_reader().read_to_end(&mut body);
        server.join().unwrap();
        assert!(
            outcome.is_ok(),
            "progress must reset the idle budget: {outcome:?}"
        );
        assert_eq!(body, b"xxxxxxxxxxxx");
    }
}
