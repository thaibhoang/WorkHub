use sqlx::PgPool;
use wh_core::{
    Actor, EventMeta, NewEvent, TaskId,
    task::{TaskEvent, TaskSource},
};
use wh_eventstore::{EventStore, ExpectedVersion, Subscription};
use wh_metrics::tasks::TasksProjection;

async fn append(store: &EventStore, task: TaskId, e: TaskEvent) {
    let meta = EventMeta::new(task.0, Actor::System { component: "test".into() });
    store
        .append(&task.stream_id(), ExpectedVersion::Any, vec![NewEvent::from_domain(&e, meta).unwrap()])
        .await
        .unwrap();
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn projection_tracks_state_and_rebuild_is_identical(pool: PgPool) {
    let store = EventStore::new(pool.clone());
    let (a, b) = (TaskId::new(), TaskId::new());
    append(&store, a, TaskEvent::TaskCreated { title: "A".into(), body: None, source: TaskSource::Cli }).await;
    append(&store, b, TaskEvent::TaskCreated { title: "B".into(), body: None, source: TaskSource::Cli }).await;
    append(&store, a, TaskEvent::StageEntered { stage: "spec".into() }).await;
    append(&store, b, TaskEvent::TaskCancelled { reason: "dup".into() }).await;

    let mut proj = TasksProjection::new(pool.clone());
    Subscription::new(store.clone()).catch_up(&mut proj).await.unwrap();
    let snapshot = |rows: Vec<wh_metrics::tasks::TaskRow>| {
        let mut v: Vec<_> = rows.into_iter().map(|r| (r.title, r.status, r.current_stage)).collect();
        v.sort();
        v
    };
    let before = snapshot(TasksProjection::list(&pool).await.unwrap());
    assert_eq!(before, vec![
        ("A".into(), "in_progress".into(), Some("spec".into())),
        ("B".into(), "cancelled".into(), None),
    ]);

    assert_eq!(wh_metrics::rebuild(&store, &mut proj).await.unwrap(), 4);
    assert_eq!(snapshot(TasksProjection::list(&pool).await.unwrap()), before);
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn redelivered_events_are_ignored(pool: PgPool) {
    let store = EventStore::new(pool.clone());
    let a = TaskId::new();
    append(&store, a, TaskEvent::TaskCreated { title: "A".into(), body: None, source: TaskSource::Cli }).await;
    append(&store, a, TaskEvent::StageEntered { stage: "spec".into() }).await;
    append(&store, a, TaskEvent::StageEntered { stage: "implement".into() }).await;

    let mut proj = TasksProjection::new(pool.clone());
    let sub = Subscription::new(store.clone());
    sub.catch_up(&mut proj).await.unwrap();
    // Simulate a crash before the checkpoint was saved: replay from the middle.
    store.save_checkpoint(TasksProjection::NAME, 1).await.unwrap();
    sub.catch_up(&mut proj).await.unwrap();
    let rows = TasksProjection::list(&pool).await.unwrap();
    assert_eq!(rows[0].current_stage.as_deref(), Some("implement"));
}
