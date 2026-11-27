//! Per-task databases (`task_database:` in factory.yaml).
//!
//! Each task gets `wh_task_<12 hex>` on the configured server, migrated once and
//! shared by its stages. The database carries a comment naming the event store
//! that created it; [`crate::cleanup`] only ever drops its own.

use anyhow::{Context, bail, ensure};
use sqlx::{Connection, PgConnection};
use wh_core::TaskId;
use wh_eventstore::{EventStore, MIGRATOR};

const PREFIX: &str = "wh_task_";

/// `wh_task_` + the last 12 hex of the task id (as in its branch name; the first
/// 12 of a UUIDv7 are a timestamp and would collide).
pub fn db_name(task: TaskId) -> String {
    format!("{PREFIX}{}", &task.0.simple().to_string()[20..])
}

/// Exactly the names [`db_name`] produces.
pub fn is_task_db_name(name: &str) -> bool {
    name.strip_prefix(PREFIX)
        .is_some_and(|hex| hex.len() == 12 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

/// `url` with only the database name replaced.
fn with_database(url: &str, db: &str) -> anyhow::Result<String> {
    let (scheme, rest) = url.split_once("://").context("the admin URL is not a postgres:// URL")?;
    ensure!(matches!(scheme, "postgres" | "postgresql"), "the admin URL is not a postgres:// URL");
    let (base, query) = rest.split_once('?').unwrap_or((rest, ""));
    let authority = base.split_once('/').map_or(base, |(a, _)| a);
    // A `dbname` parameter would override the path.
    let query: Vec<&str> = query.split('&').filter(|p| !p.is_empty() && !p.starts_with("dbname=")).collect();
    let mut out = format!("{scheme}://{authority}/{db}");
    if !query.is_empty() {
        out.push('?');
        out.push_str(&query.join("&"));
    }
    Ok(out)
}

/// The comment marking a database as created by the event store `instance`.
pub(crate) fn owner_comment(instance: uuid::Uuid) -> String {
    format!("workhub:{instance}")
}

/// Create and migrate the task's database if needed, and return its URL. A database
/// that was migrated once is left alone: the task branch may have added migrations.
pub async fn provision(store: &EventStore, admin_url: &str, task: TaskId) -> anyhow::Result<String> {
    let name = db_name(task);
    let url = with_database(admin_url, &name)?;
    let owner = owner_comment(store.instance_id().await?);
    let ready = format!("{owner} ready");
    let mut admin = PgConnection::connect(admin_url).await.context("connect to the task database server")?;
    let find = "SELECT shobj_description(oid, 'pg_database') FROM pg_database WHERE datname = $1";
    let mut found: Option<Option<String>> = sqlx::query_scalar(find).bind(&name).fetch_optional(&mut admin).await?;
    if found.is_none() {
        // `name` is `wh_task_` + hex, safe to splice into DDL.
        if let Err(e) = sqlx::query(&format!("CREATE DATABASE {name}")).execute(&mut admin).await {
            // Someone else creating it at the same time is fine.
            let code = e.as_database_error().and_then(|d| d.code());
            if !matches!(code.as_deref(), Some("42P04" | "23505")) {
                return Err(e).with_context(|| format!("create database {name}"));
            }
        }
        found = sqlx::query_scalar(find).bind(&name).fetch_optional(&mut admin).await?;
    }
    match found.context("the database disappeared while being created")? {
        Some(c) if c == ready => return Ok(url),
        Some(c) if c == owner => {}
        Some(_) => bail!("database {name} exists and was not created by this WorkHub"),
        None => {
            sqlx::query(&format!("COMMENT ON DATABASE {name} IS '{owner}'")).execute(&mut admin).await?;
        }
    }
    let mut db = PgConnection::connect(&url).await.with_context(|| format!("connect to {name}"))?;
    MIGRATOR.run_direct(&mut db).await.with_context(|| format!("migrate {name}"))?;
    db.close().await?;
    sqlx::query(&format!("COMMENT ON DATABASE {name} IS '{ready}'")).execute(&mut admin).await?;
    admin.close().await?;
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_use_the_random_end_of_the_task_id() {
        let a = TaskId("01a21dc8-9bcd-73e4-a28d-c4f399950ce7".parse().unwrap());
        let b = TaskId("01a21dc8-9bcd-73e4-a28d-000000000001".parse().unwrap());
        assert_eq!(db_name(a), "wh_task_c4f399950ce7");
        assert_ne!(db_name(a), db_name(b), "same timestamp, different databases");
        assert!(is_task_db_name(&db_name(a)));
        for other in ["workhub", "wh_task_", "wh_task_c4f399950ce7x", "wh_task_C4F399950CE7", "xwh_task_c4f399950ce7"] {
            assert!(!is_task_db_name(other), "{other}");
        }
    }

    #[test]
    fn only_the_database_name_is_replaced() {
        let to = |u| with_database(u, "db").unwrap();
        assert_eq!(to("postgres://u:p@h:5544/workhub"), "postgres://u:p@h:5544/db");
        assert_eq!(to("postgres://u:p@h"), "postgres://u:p@h/db");
        assert_eq!(
            to("postgresql://u@h/x?sslmode=disable&dbname=y&host=/run/pg"),
            "postgresql://u@h/db?sslmode=disable&host=/run/pg"
        );
        assert!(with_database("host=h dbname=x", "db").is_err());
    }
}
