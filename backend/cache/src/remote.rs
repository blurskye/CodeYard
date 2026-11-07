//! The Redis dependency and all protocol operations stay inside this crate.
use crate::{Mode, BUDGET};
use redis::{
    aio::MultiplexedConnection, cluster::ClusterClient, cluster_async::ClusterConnection, Cmd,
    FromRedisValue,
};
use std::{sync::Mutex, time::Duration};
use tokio::time::Instant;

#[derive(Clone)]
enum Connection {
    Standalone(MultiplexedConnection),
    Cluster(ClusterConnection),
}
enum Client {
    Standalone(redis::Client),
    Cluster(ClusterClient),
}
struct Breaker {
    failures: u8,
    until: Instant,
}
pub(crate) struct Remote {
    client: Client,
    connection: tokio::sync::Mutex<Option<Connection>>,
    breaker: Mutex<Breaker>,
}
impl Remote {
    pub fn new(mode: Mode, urls: &[String]) -> anyhow::Result<Option<Self>> {
        let client = match mode {
            Mode::Local => return Ok(None),
            Mode::Standalone => Client::Standalone(redis::Client::open(urls[0].as_str())?),
            Mode::Cluster => Client::Cluster(
                ClusterClient::builder(urls.to_vec())
                    .retries(0)
                    .connection_timeout(BUDGET)
                    .response_timeout(BUDGET)
                    .build()?,
            ),
        };
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        Ok(Some(Self {
            client,
            connection: tokio::sync::Mutex::new(None),
            breaker: Mutex::new(Breaker {
                failures: 0,
                until: Instant::now(),
            }),
        }))
    }
    async fn connection(&self) -> redis::RedisResult<Connection> {
        let mut retained = self.connection.lock().await;
        if let Some(connection) = retained.as_ref() {
            return Ok(connection.clone());
        }
        let connection = match &self.client {
            Client::Standalone(client) => Connection::Standalone(
                client
                    .get_multiplexed_async_connection_with_config(
                        &redis::AsyncConnectionConfig::new()
                            .set_connection_timeout(BUDGET)
                            .set_response_timeout(BUDGET),
                    )
                    .await?,
            ),
            Client::Cluster(client) => Connection::Cluster(client.get_async_connection().await?),
        };
        *retained = Some(connection.clone());
        Ok(connection)
    }
    pub fn failed(&self) {
        let mut breaker = self.breaker.lock().unwrap_or_else(|e| e.into_inner());
        breaker.failures = breaker.failures.saturating_add(1);
        if breaker.failures >= 3 {
            breaker.until = Instant::now() + Duration::from_secs(1);
        }
    }
    pub async fn query<T: FromRedisValue>(&self, command: &Cmd) -> Option<T> {
        if self.breaker.lock().unwrap_or_else(|e| e.into_inner()).until > Instant::now() {
            return None;
        }
        let result = tokio::time::timeout(BUDGET, async {
            match self.connection().await? {
                Connection::Standalone(mut connection) => {
                    command.query_async(&mut connection).await
                }
                Connection::Cluster(mut connection) => command.query_async(&mut connection).await,
            }
        })
        .await;
        let mut breaker = self.breaker.lock().unwrap_or_else(|e| e.into_inner());
        match result {
            Ok(Ok(value)) => {
                breaker.failures = 0;
                Some(value)
            }
            _ => {
                breaker.failures = breaker.failures.saturating_add(1);
                if breaker.failures >= 3 {
                    breaker.until = Instant::now() + Duration::from_secs(1);
                }
                if let Ok(mut retained) = self.connection.try_lock() {
                    *retained = None;
                }
                None
            }
        }
    }
}

/// CRC16/XMODEM over the whole key. Generated keys cannot contain hash tags.
pub(crate) fn slot(key: &str) -> u16 {
    let mut crc = 0_u16;
    for byte in key.bytes() {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc % 16384
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn failed_connections_open_a_bounded_breaker() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        // Accept no requests: connection setup must time out within the call budget.
        let remote = super::Remote::new(crate::Mode::Standalone, &[format!("redis://{address}/")])
            .unwrap()
            .unwrap();
        for _ in 0..3 {
            let value: Option<Vec<u8>> = remote.query(redis::cmd("GET").arg("missing")).await;
            assert!(value.is_none());
        }
        assert!(remote.breaker.lock().unwrap().until > tokio::time::Instant::now());
        drop(listener);
    }

    #[test]
    fn redis_slot_uses_the_standard_crc() {
        assert_eq!(super::slot("123456789"), 0x31c3);
        assert_eq!(super::slot("foo"), 12182);
        assert_eq!(super::slot("bar"), 5061);
    }
}
