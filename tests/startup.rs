//! Starting while the database is not there yet: the server retries with a
//! growing wait, answers 503 until it connects, and gives up only once the
//! database has stayed away past the limit.

mod common;

use std::time::{Duration, Instant};

use common::TestDb;
use house_mcp::{
    config::{Config, Token},
    db::{self, Retry},
};
use tokio::net::{TcpListener, TcpStream};

/// A port nothing listens on yet, which after `delay` forwards every
/// connection to the test database: a database that comes up late.
async fn late_database(db: &TestDb, delay: Duration) -> (String, tokio::task::JoinHandle<()>) {
    let reserved = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = reserved.local_addr().expect("address").port();
    drop(reserved);
    let options = db.options("house", common::HOUSE_PASSWORD);
    let target = format!("{}:{}", options.get_host(), options.get_port());
    let proxy = tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .expect("bind the late port");
        loop {
            let (mut inbound, _) = listener.accept().await.expect("accept");
            let target = target.clone();
            tokio::spawn(async move {
                if let Ok(mut outbound) = TcpStream::connect(target).await {
                    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                }
            });
        }
    });
    let url = format!(
        "postgres://house:{}@127.0.0.1:{port}/{}",
        common::HOUSE_PASSWORD,
        db.name
    );
    (url, proxy)
}

const QUICK: Retry = Retry {
    give_up_after: Duration::from_secs(30),
    first_wait: Duration::from_millis(100),
    longest_wait: Duration::from_millis(400),
};

#[tokio::test]
async fn a_late_database_is_waited_for() {
    let db = TestDb::new().await;
    let (url, proxy) = late_database(&db, Duration::from_secs(2)).await;
    let started = Instant::now();
    let pool = db::connect_with_retry(&url, QUICK)
        .await
        .expect("connects once it is up");
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "connected before the database was up"
    );
    let one: i32 = sqlx::query_scalar("SELECT 1").fetch_one(&pool).await.expect("a query");
    assert_eq!(one, 1);
    proxy.abort();
}

#[tokio::test]
async fn a_database_that_never_comes_is_given_up_on() {
    let db = TestDb::new().await;
    let (url, proxy) = late_database(&db, Duration::from_secs(3600)).await;
    let retry = Retry {
        give_up_after: Duration::from_secs(1),
        ..QUICK
    };
    let started = Instant::now();
    assert!(db::connect_with_retry(&url, retry).await.is_err());
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(500),
        "gave up at once, after {waited:?}"
    );
    // A try that is refused can itself take the pool's 10 s acquire
    // timeout, so giving up comes within the limit and one try.
    assert!(waited < Duration::from_secs(15), "kept trying for {waited:?}");
    assert!(db::connect_with_retry(&url, Retry::ONCE).await.is_err());
    proxy.abort();
}

#[tokio::test]
async fn the_server_is_not_ready_until_the_database_is() {
    let db = TestDb::new().await;
    let (url, proxy) = late_database(&db, Duration::from_secs(2)).await;
    let files = tempfile::tempdir().expect("tempdir");
    let config = Config {
        database_url: url,
        files_dir: files.path().to_owned(),
        listen: "127.0.0.1:0".parse().expect("address"),
        allowed_hosts: vec!["127.0.0.1".into()],
        read_write: Token {
            name: "jesse".into(),
            secret: "rw-token-for-tests-0123456789abcdef0123456789".into(),
        },
        read_only: Token {
            name: "readonly".into(),
            secret: "ro-token-for-tests-0123456789abcdef0123456789".into(),
        },
    };
    let (app, ready) = house_mcp::start(config, QUICK);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let health = format!("http://{}/healthz", listener.local_addr().expect("address"));
    tokio::spawn(async move { axum::serve(listener, app).await });
    let http = reqwest::Client::new();
    let status = || async { http.get(&health).send().await.expect("an answer").status().as_u16() };

    // Listening at once, and not ready.
    assert_eq!(status().await, 503);
    let mcp = http
        .post(health.replace("/healthz", "/mcp"))
        .send()
        .await
        .expect("an answer");
    assert_eq!(mcp.status().as_u16(), 503, "no tool runs before the database is there");

    // Ready once the database comes up and the schema is migrated.
    tokio::time::timeout(Duration::from_secs(30), ready)
        .await
        .expect("ready in time")
        .expect("a task")
        .expect("started");
    assert_eq!(status().await, 200);
    proxy.abort();
}
