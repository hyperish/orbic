use crate::{JobQueue, Schedule, query, rows::ScheduleRow};
use hyve::{HyveError, HyveResult, Ulid, Value};
use orbic_db::bind;

impl JobQueue {
    /// Registers a schedule on first poll. When due, enqueues exactly one job
    /// and advances the schedule in the same transaction. Replicas may poll
    /// the same name concurrently. Missed intervals coalesce into one run.
    pub async fn tick(
        &self,
        name: &str,
        schedule: &Schedule,
        topic: &str,
        payload: Value,
        attempts: u32,
    ) -> HyveResult<Option<Ulid>> {
        if name.is_empty() || topic.is_empty() || attempts == 0 {
            return Err(HyveError::invalid_request(
                "a schedule needs a name, topic, and positive attempt limit",
            ));
        }
        let now = self.now().await?;
        let next = schedule.next(now)?;
        self.db.tx(|tx| async move {
            query::<()>("INSERT INTO orbic_job_schedules (name,spec,next_at) VALUES ($1,$2,$3) ON CONFLICT (name) DO NOTHING",
                vec![bind(name),bind(schedule.expression()),bind(&next)]).execute(&tx).await?;
            let stored = query::<ScheduleRow>("SELECT spec FROM orbic_job_schedules WHERE name=$1",vec![bind(name)]).fetch_one(&tx).await?;
            if stored.spec != schedule.expression() {
                return Err(HyveError::domain("schedule_conflict","schedule name already registered with different timing"));
            }
            let due = query::<()>("UPDATE orbic_job_schedules SET next_at=$1 WHERE name=$2 AND next_at<=$3",
                vec![bind(&next),bind(name),bind(&now)]).execute(&tx).await?;
            if due == 0 { return Ok(None); }
            JobQueue::insert(&tx, topic, &payload, now, attempts).await.map(Some)
        }).await
    }
}
