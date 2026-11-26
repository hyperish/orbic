use hyve::{HyveError, HyveResult};
use orbic_db::projection as row;

row!(Tick, "clock", tick: i64);
row!(ScheduleRow, "orbic_job_schedules", spec: String);
row!(JobRow, "orbic_jobs", id: String, topic: String, body: String, attempt: i64, maximum: i64, state: String, last_error: Option<String>);

impl JobRow {
    pub(crate) fn decode(self) -> HyveResult<super::Job> {
        Ok(super::Job {
            id: self
                .id
                .parse()
                .map_err(|_| HyveError::internal("invalid job ID"))?,
            topic: self.topic,
            payload: hyve::serde::from_json(&self.body)
                .map_err(|_| HyveError::internal("invalid job payload"))?,
            attempt: self
                .attempt
                .try_into()
                .map_err(|_| HyveError::internal("invalid attempt count"))?,
            maximum: self
                .maximum
                .try_into()
                .map_err(|_| HyveError::internal("invalid attempt limit"))?,
            state: self.state.parse()?,
            last_error: self.last_error,
        })
    }
}

row!(SagaRow, "orbic_sagas", id: String, plan: String, input: String, maximum: i64, step: i64, direction: String, state: String, operation: Option<String>, attempt: i64, revision: i64, cancelled: i64, problem: Option<String>);
