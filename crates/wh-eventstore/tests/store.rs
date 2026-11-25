use sqlx::PgPool;
use uuid::Uuid;
use wh_core::{
    Actor, EventMeta, NewEvent, RecordedEvent, TaskId,
    task::{StageOutcome, TaskEvent, TaskSource},
};
use wh_eventstore::{EventHandler, EventStore, ExpectedVersion, StoreError, Subscription};

fn ev(task: TaskId, e: TaskEvent) -> NewEvent {
    let meta = EventMeta::new(task.0, Actor::System { component: "test".into() });
    NewEvent::from_domain(&e, meta).unwrap()
}

fn created(task: TaskId) -> NewEvent {
    ev(task, TaskEvent::TaskCreated { title: "t".into(), body: None, source: TaskSource::Cli, factory: None, benchmark: None, external_ref: None })
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn append_assigns_versions_and_round_trips(pool: PgPool) {
    let store = EventStore::new(pool);
    let task = TaskId::new();
    let stream = task.stream_id();
    store
        .append(&stream, ExpectedVersion::NoStream, vec![
            created(task),
            ev(task, TaskEvent::StageEntered { stage: "triage".into(), feedback: None }),
        ])
        .await
        .unwrap();
    store
        .append(&stream, ExpectedVersion::Exact(2), vec![ev(task, TaskEvent::StageExited {
            stage: "triage".into(),
            outcome: StageOutcome::Succeeded,
            summary: None,
            session: None,
        })])
        .await
        .unwrap();

    let events = store.read_stream(&stream).await.unwrap();
    assert_eq!(events.iter().map(|e| e.stream_version).collect::<Vec<_>>(), vec![1, 2, 3]);
    assert_eq!(events[0].meta.correlation_id, task.0);
    assert!(matches!(events[1].decode::<TaskEvent>().unwrap(), TaskEvent::StageEntered { .. }));
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn expected_version_mismatch_is_rejected(pool: PgPool) {
    let store = EventStore::new(pool);
    let task = TaskId::new();
    let stream = task.stream_id();
    store.append(&stream, ExpectedVersion::NoStream, vec![created(task)]).await.unwrap();

    let err = store.append(&stream, ExpectedVersion::NoStream, vec![created(task)]).await.unwrap_err();
    assert!(matches!(err, StoreError::Conflict { actual: 1, .. }));
    let err = store
        .append(&stream, ExpectedVersion::Exact(5), vec![created(task)])
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Conflict { .. }));
    assert_eq!(store.read_stream(&stream).await.unwrap().len(), 1);
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn events_table_is_append_only(pool: PgPool) {
    let store = EventStore::new(pool.clone());
    let task = TaskId::new();
    store.append(&task.stream_id(), ExpectedVersion::Any, vec![created(task)]).await.unwrap();
    assert!(sqlx::query("DELETE FROM events").execute(&pool).await.is_err());
    assert!(sqlx::query("UPDATE events SET event_type = 'x'").execute(&pool).await.is_err());
}

struct Collector(Vec<(i64, Uuid)>);

impl EventHandler for Collector {
    fn name(&self) -> &str {
        "collector"
    }
    async fn handle(&mut self, e: &RecordedEvent) -> anyhow::Result<()> {
        self.0.push((e.global_seq, e.event_id));
        Ok(())
    }
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn catch_up_resumes_from_checkpoint_in_global_order(pool: PgPool) {
    let store = EventStore::new(pool);
    let (a, b) = (TaskId::new(), TaskId::new());
    store.append(&a.stream_id(), ExpectedVersion::Any, vec![created(a)]).await.unwrap();
    store.append(&b.stream_id(), ExpectedVersion::Any, vec![created(b)]).await.unwrap();

    let sub = Subscription::new(store.clone());
    let mut c = Collector(Vec::new());
    assert_eq!(sub.catch_up(&mut c).await.unwrap(), 2);
    assert_eq!(sub.catch_up(&mut c).await.unwrap(), 0, "checkpoint must prevent redelivery");

    store
        .append(&a.stream_id(), ExpectedVersion::Exact(1), vec![ev(a, TaskEvent::StageEntered { stage: "spec".into(), feedback: None })])
        .await
        .unwrap();
    assert_eq!(sub.catch_up(&mut c).await.unwrap(), 1);
    let seqs: Vec<i64> = c.0.iter().map(|x| x.0).collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]));
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn concurrent_appends_to_one_stream_never_duplicate_versions(pool: PgPool) {
    let store = EventStore::new(pool);
    let task = TaskId::new();
    let stream = task.stream_id();
    let handles: Vec<_> = (0..10)
        .map(|_| {
            let (store, stream) = (store.clone(), stream.clone());
            tokio::spawn(async move {
                store
                    .append(&stream, ExpectedVersion::Any, vec![ev(task, TaskEvent::StageEntered { stage: "x".into(), feedback: None })])
                    .await
            })
        })
        .collect();
    for h in handles {
        h.await.unwrap().unwrap();
    }
    let versions: Vec<i64> = store.read_stream(&stream).await.unwrap().iter().map(|e| e.stream_version).collect();
    assert_eq!(versions, (1..=10).collect::<Vec<_>>());
}
