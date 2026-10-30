use sqlx::{postgres::PgListener, PgPool};

const CHANNEL: &str = "notifications";

pub(crate) struct NotificationListener {
    connection: PgListener,
}

impl NotificationListener {
    pub(crate) async fn connect(pool: &PgPool) -> Result<Self, sqlx::Error> {
        let mut connection = PgListener::connect_with(pool).await?;
        connection.listen(CHANNEL).await?;
        Ok(Self { connection })
    }

    /// `None` when the connection was lost. Hints sent until it is back are gone.
    pub(crate) async fn recv(&mut self) -> Result<Option<String>, sqlx::Error> {
        Ok(self
            .connection
            .try_recv()
            .await?
            .map(|notification| notification.payload().to_owned()))
    }
}

pub(crate) async fn announce(pool: &PgPool, payload: &str) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_notify($1, $2)")
        .bind(CHANNEL)
        .bind(payload)
        .execute(pool)
        .await?;
    Ok(())
}
