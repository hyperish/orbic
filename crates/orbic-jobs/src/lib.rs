//! Delayed, at-least-once jobs on the app's database. The database clock
//! decides when work is due. Each claim has a fresh lease. Only its owner
//! can renew, finish or retry the job. Use the stable job ID to deduplicate
//! downstream writes across worker restarts.

mod rows;
pub mod saga;
mod schedule;
pub use orbic_core::Schedule;

use std::future::Future;
use std::time::Duration;

use hyve::{HyveError, HyveResult, Scope, Ulid, Value};
use orbic_db::{Bind, Db, Dialect, Sql, Target, bind};
use rows::{JobRow, Tick};

const DDL: &str = "CREATE TABLE IF NOT EXISTS orbic_jobs (
    id TEXT PRIMARY KEY, topic TEXT NOT NULL, body TEXT NOT NULL,
    attempt BIGINT NOT NULL, maximum BIGINT NOT NULL, state TEXT NOT NULL,
    due BIGINT NOT NULL, lease TEXT, last_error TEXT);
    CREATE INDEX IF NOT EXISTS orbic_jobs_due ON orbic_jobs (topic,state,due);
    CREATE TABLE IF NOT EXISTS orbic_job_schedules (name TEXT PRIMARY KEY, spec TEXT NOT NULL, next_at BIGINT NOT NULL);";

/// Durable state of a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// Waiting for its due time.
    Queued,
    /// A worker owns a time-limited lease.
    Running,
    /// The worker acknowledged completion.
    Done,
    /// Attempts exhausted. The last error is kept for inspection.
    Dead,
    /// An external write may have committed. The app must check its outcome.
    Reconcile,
}

impl std::str::FromStr for JobState {
    type Err = HyveError;
    fn from_str(s: &str) -> HyveResult<Self> {
        match s {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "done" => Ok(Self::Done),
            "dead" => Ok(Self::Dead),
            "reconcile" => Ok(Self::Reconcile),
            _ => Err(HyveError::internal("invalid stored job state")),
        }
    }
}

/// Persisted, inspectable work item.
#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    /// Stable ID, preserved across retries.
    pub id: Ulid,
    /// Worker or queue name.
    pub topic: String,
    /// Application payload.
    pub payload: Value,
    /// Number of claims so far.
    pub attempt: u32,
    /// Maximum number of claims.
    pub maximum: u32,
    /// Current durable state.
    pub state: JobState,
    /// Last worker-supplied public diagnostic.
    pub last_error: Option<String>,
}

/// One fenced claim. A stale worker cannot acknowledge its replacement's work.
pub struct Claim {
    job: Job,
    token: String,
    lease: Duration,
}

impl Claim {
    /// Work item associated with this claim.
    pub fn job(&self) -> &Job {
        &self.job
    }
}

/// Worker decision after handling a claim.
pub enum Outcome {
    /// The operation completed.
    Done,
    /// Retry with exponential backoff, or dead-letter on the last attempt.
    Retry(String),
    /// Hold until the application reconciles an uncertain external result.
    Reconcile(String),
}

/// A queue sharing the application's existing database pool.
#[derive(Clone)]
pub struct JobQueue {
    db: Db,
}

fn query<M>(sql: &'static str, args: Vec<Bind>) -> Sql<M> {
    Sql::new(sql, &["sqlite", "postgres"], &[], args)
}

fn millis(d: Duration) -> HyveResult<i64> {
    i64::try_from(d.as_nanos().div_ceil(1_000_000))
        .map_err(|_| HyveError::invalid_request("duration is too large"))
}

fn add(at: i64, millis: i64) -> HyveResult<i64> {
    at.checked_add(millis)
        .ok_or_else(|| HyveError::invalid_request("job time is out of range"))
}

impl JobQueue {
    /// Initializes the queue tables in the selected app database.
    pub async fn new(db: Db) -> HyveResult<Self> {
        let pg = db.dialect() == Dialect::Postgres;
        db.tx(move |tx| async move {
            if pg {
                tx.execute_script("SELECT pg_advisory_xact_lock(27451895063489651)")
                    .await?;
            }
            tx.execute_script(DDL).await?;
            tx.execute_script(saga::DDL).await
        })
        .await?;
        Ok(Self { db })
    }

    async fn now(&self) -> HyveResult<i64> {
        let sql = match self.db.dialect() {
            Dialect::Postgres => {
                "SELECT CAST(FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000) AS BIGINT) AS tick"
            }
            Dialect::Sqlite => "SELECT CAST(unixepoch('subsec') * 1000 AS INTEGER) AS tick",
        };
        Ok(query::<Tick>(sql, vec![]).fetch_one(&self.db).await?.tick)
    }

    /// Enqueues a job for a topic, optionally delayed.
    pub async fn enqueue(
        &self,
        topic: &str,
        payload: Value,
        delay: Duration,
        attempts: u32,
    ) -> HyveResult<Ulid> {
        let due = add(self.now().await?, millis(delay)?)?;
        Self::insert(&self.db, topic, &payload, due, attempts).await
    }

    async fn insert(
        db: impl Target,
        topic: &str,
        payload: &Value,
        due: i64,
        attempts: u32,
    ) -> HyveResult<Ulid> {
        if topic.is_empty() || attempts == 0 {
            return Err(HyveError::invalid_request(
                "jobs need a topic and a positive attempt limit",
            ));
        }
        let id = Ulid::new();
        query::<()>("INSERT INTO orbic_jobs (id,topic,body,attempt,maximum,state,due) VALUES ($1,$2,$3,0,$4,'queued',$5)",
            vec![bind(&id), bind(topic), bind(&hyve::serde::to_json(payload)), bind(&i64::from(attempts)), bind(&due)]).execute(db).await?;
        Ok(id)
    }

    /// Inspects a job, including completed, dead-lettered and uncertain work.
    pub async fn get(&self, id: Ulid) -> HyveResult<Job> {
        query::<JobRow>(
            "SELECT id,topic,body,attempt,maximum,state,last_error FROM orbic_jobs WHERE id=$1",
            vec![bind(&id)],
        )
        .fetch_one(&self.db)
        .await?
        .decode()
    }

    /// Atomically claims one due job, reclaiming an expired lease when eligible.
    pub async fn claim(&self, topic: &str, lease: Duration) -> HyveResult<Option<Claim>> {
        if lease.is_zero() {
            return Err(HyveError::invalid_request("job lease must be positive"));
        }
        let now = self.now().await?;
        let until = add(now, millis(lease)?)?;
        query::<()>("UPDATE orbic_jobs SET state='reconcile',lease=NULL,last_error='worker lease expired on final attempt'
            WHERE topic=$1 AND state='running' AND due<=$2 AND attempt>=maximum",
            vec![bind(topic),bind(&now)]).execute(&self.db).await?;
        let token = Ulid::new().to_string();
        let row = query::<JobRow>("UPDATE orbic_jobs SET lease=$1,due=$2,state='running',attempt=attempt+1
            WHERE id=(SELECT id FROM orbic_jobs WHERE topic=$3 AND state IN ('queued','running') AND due<=$4 AND attempt<maximum ORDER BY due,id LIMIT 1)
            AND state IN ('queued','running') AND due<=$4 AND attempt<maximum
            RETURNING id,topic,body,attempt,maximum,state,last_error",
            vec![bind(&token),bind(&until),bind(topic),bind(&now)]).fetch_optional(&self.db).await?;
        row.map(|row| {
            Ok(Claim {
                job: row.decode()?,
                token,
                lease,
            })
        })
        .transpose()
    }

    /// Extends a still-live lease.
    pub async fn renew(&self, claim: &Claim) -> HyveResult<()> {
        let now = self.now().await?;
        let until = add(now, millis(claim.lease)?)?;
        let n = query::<()>(
            "UPDATE orbic_jobs SET due=$1 WHERE id=$2 AND lease=$3 AND state='running' AND due>$4",
            vec![
                bind(&until),
                bind(&claim.job.id),
                bind(&claim.token),
                bind(&now),
            ],
        )
        .execute(&self.db)
        .await?;
        owned(n)
    }

    /// Records the outcome only while this worker still owns the job.
    pub async fn finish(&self, claim: &Claim, outcome: Outcome) -> HyveResult<()> {
        let now = self.now().await?;
        let (state, due, reason) = match outcome {
            Outcome::Done => ("done", now, None),
            Outcome::Reconcile(reason) => ("reconcile", now, Some(reason)),
            Outcome::Retry(reason) if claim.job.attempt >= claim.job.maximum => {
                ("dead", now, Some(reason))
            }
            Outcome::Retry(reason) => {
                let delay =
                    (1000_i64 << claim.job.attempt.saturating_sub(1).min(12)).min(3_600_000);
                ("queued", add(now, delay)?, Some(reason))
            }
        };
        let n = query::<()>(
            "UPDATE orbic_jobs SET state=$1,due=$2,last_error=$3,lease=NULL
            WHERE id=$4 AND lease=$5 AND state='running' AND due>$6",
            vec![
                bind(state),
                bind(&due),
                bind(&reason),
                bind(&claim.job.id),
                bind(&claim.token),
                bind(&now),
            ],
        )
        .execute(&self.db)
        .await?;
        owned(n)
    }

    /// Runs one job and renews its lease while the handler is active. On
    /// cancellation an already started operation goes to reconciliation.
    pub async fn run_once<F: Future<Output = Outcome>>(
        &self,
        topic: &str,
        lease: Duration,
        scope: &Scope,
        handler: impl FnOnce(Job) -> F,
    ) -> HyveResult<bool> {
        if scope.cancellation().is_cancelled() {
            return Ok(false);
        }
        let Some(claim) = self.claim(topic, lease).await? else {
            return Ok(false);
        };
        let work = scope.scoped(handler(claim.job.clone()));
        tokio::pin!(work);
        let heartbeat = (lease / 3).max(Duration::from_millis(1));
        let outcome = loop {
            tokio::select! {
                result = &mut work => break result,
                () = scope.cancellation().cancelled() => break Outcome::Reconcile("worker cancelled".into()),
                () = tokio::time::sleep(heartbeat) => self.renew(&claim).await?,
            }
        };
        self.finish(&claim, outcome).await?;
        Ok(true)
    }
}

fn owned(n: u64) -> HyveResult<()> {
    if n == 1 {
        Ok(())
    } else {
        Err(HyveError::domain(
            "job_lease_lost",
            "job lease is no longer owned by this worker",
        ))
    }
}
