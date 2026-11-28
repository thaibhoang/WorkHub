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
    append(&store, a, TaskEvent::TaskCreated { title: "A".into(), body: None, source: TaskSource::Cli, factory: None, benchmark: None, external_ref: None, parent: None, draft: false }).await;
    append(&store, b, TaskEvent::TaskCreated { title: "B".into(), body: None, source: TaskSource::Cli, factory: None, benchmark: None, external_ref: None, parent: None, draft: false }).await;
    append(&store, a, TaskEvent::StageEntered { stage: "spec".into(), feedback: None }).await;
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
        ("A".into(), "running".into(), Some("spec".into())),
        ("B".into(), "cancelled".into(), None),
    ]);

    assert_eq!(wh_metrics::rebuild(&store, &mut proj).await.unwrap(), 4);
    assert_eq!(snapshot(TasksProjection::list(&pool).await.unwrap()), before);
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn redelivered_events_are_ignored(pool: PgPool) {
    let store = EventStore::new(pool.clone());
    let a = TaskId::new();
    append(&store, a, TaskEvent::TaskCreated { title: "A".into(), body: None, source: TaskSource::Cli, factory: None, benchmark: None, external_ref: None, parent: None, draft: false }).await;
    append(&store, a, TaskEvent::StageEntered { stage: "spec".into(), feedback: None }).await;
    append(&store, a, TaskEvent::StageEntered { stage: "implement".into(), feedback: None }).await;

    let mut proj = TasksProjection::new(pool.clone());
    let sub = Subscription::new(store.clone());
    sub.catch_up(&mut proj).await.unwrap();
    // Simulate a crash before the checkpoint was saved: replay from the middle.
    store.save_checkpoint(TasksProjection::NAME, 1).await.unwrap();
    sub.catch_up(&mut proj).await.unwrap();
    let rows = TasksProjection::list(&pool).await.unwrap();
    assert_eq!(rows[0].current_stage.as_deref(), Some("implement"));
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn awaiting_input_keeps_the_stage_until_answered(pool: PgPool) {
    let store = EventStore::new(pool.clone());
    let a = TaskId::new();
    append(&store, a, TaskEvent::TaskCreated { title: "A".into(), body: None, source: TaskSource::Cli, factory: None, benchmark: None, external_ref: None, parent: None, draft: false }).await;
    append(&store, a, TaskEvent::StageEntered { stage: "spec".into(), feedback: None }).await;
    append(&store, a, TaskEvent::AgentAsked { stage: "spec".into(), session: wh_core::SessionId::new(), questions: vec![] }).await;

    let mut proj = TasksProjection::new(pool.clone());
    let sub = Subscription::new(store.clone());
    sub.catch_up(&mut proj).await.unwrap();
    let row = TasksProjection::list(&pool).await.unwrap().remove(0);
    assert_eq!((row.status.as_str(), row.current_stage.as_deref()), ("awaiting_input", Some("spec")));

    append(&store, a, TaskEvent::UserAnswered { stage: "spec".into(), answers: vec![], user: "u".into() }).await;
    sub.catch_up(&mut proj).await.unwrap();
    let row = TasksProjection::list(&pool).await.unwrap().remove(0);
    assert_eq!((row.status.as_str(), row.current_stage.as_deref()), ("running", Some("spec")));
    assert_eq!(row.rework_loops, 0);
}
