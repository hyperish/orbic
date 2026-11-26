//! A durable sequence of idempotent service operations with reverse-order
//! compensation. External calls use the stable step key across retries. A
//! worker reports an ambiguous result for reconciliation before proceeding.

use super::{JobQueue, add, millis, owned, query, rows::SagaRow};
use hyve::{
    HyveError, HyveResult, Scope, Ulid, Value,
    serde::{from_json, to_json},
};
use orbic_db::{Target, Tx, bind};
use std::{future::Future, time::Duration};

pub(crate) const DDL: &str = "CREATE TABLE IF NOT EXISTS orbic_sagas (
    id TEXT PRIMARY KEY, plan TEXT NOT NULL, input TEXT NOT NULL, maximum BIGINT NOT NULL,
    step BIGINT NOT NULL, direction TEXT NOT NULL, state TEXT NOT NULL, operation TEXT,
    attempt BIGINT NOT NULL, due BIGINT NOT NULL, lease TEXT, revision BIGINT NOT NULL, cancelled BIGINT NOT NULL DEFAULT 0, problem TEXT);
    CREATE INDEX IF NOT EXISTS orbic_sagas_ready ON orbic_sagas(operation,state,due);";

const SELECT_SAGA: &str = "SELECT id,plan,input,maximum,step,direction,state,operation,attempt,revision,cancelled,problem FROM
    orbic_sagas WHERE id=$1";

const INSERT_SAGA: &str = "INSERT INTO orbic_sagas(id,plan,input,maximum,step,direction,state,operation,attempt,due,revision)
    VALUES($1,$2,$3,$4,0,'apply','ready',$5,0,$6,0) ON CONFLICT(id) DO NOTHING";

const EXPIRE_SAGA: &str = "UPDATE orbic_sagas SET state='reconcile',lease=NULL,revision=revision+1,problem='last worker lease
    expired' WHERE id=$1 AND state='running' AND due<=$2 AND attempt>=maximum";

const EXPIRE_OPERATION: &str = "UPDATE orbic_sagas SET state='reconcile',lease=NULL,revision=revision+1,problem='last worker lease
    expired' WHERE operation=$1 AND state='running' AND due<=$2 AND attempt>=maximum";

const CLAIM_OPERATION: &str = "UPDATE orbic_sagas SET state='running',lease=$1,attempt=attempt+1,due=$2,revision=revision+1 WHERE
    id=(SELECT id FROM orbic_sagas WHERE operation=$3 AND state IN ('ready','running') AND due<=$4 AND
    attempt<maximum ORDER BY due,id LIMIT 1) AND operation=$3 AND state IN ('ready','running') AND
    due<=$4 AND attempt<maximum RETURNING
    id,plan,input,maximum,step,direction,state,operation,attempt,revision,cancelled,problem";

const RELEASE_OWNED: &str =
    "UPDATE orbic_sagas SET lease=NULL WHERE id=$1 AND state='running' AND lease=$2 AND due>$3";

const RENEW_LEASE: &str =
    "UPDATE orbic_sagas SET due=$1 WHERE id=$2 AND lease=$3 AND state='running' AND due>$4";

const CLAIM_RECONCILIATION: &str =
    "UPDATE orbic_sagas SET revision=revision+1 WHERE id=$1 AND state='reconcile' AND revision=$2";

const RETRY_COMPENSATION: &str =
    "UPDATE orbic_sagas SET state='ready',attempt=0,due=$1 WHERE id=$2";

const CANCEL_FORWARD: &str = "UPDATE orbic_sagas SET lease=NULL,cancelled=1,revision=revision+1 WHERE id=$1 AND cancelled=0 AND
    direction='apply' AND state IN ('ready','running','reconcile')";

const SAVE_TRANSITION: &str = "UPDATE orbic_sagas SET
    step=$1,direction=$2,state=$3,operation=$4,attempt=$5,due=$6,lease=NULL,problem=$7,revision=revision+1
    WHERE id=$8";

/// A named forward operation and its optional compensating operation.
#[hyve::model]
pub struct Step {
    /// Name served by the forward worker.
    pub apply: String,
    /// Name served by the compensation worker. An irreversible step requires reconciliation.
    pub undo: Option<String>,
}

/// Persisted workflow state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Waiting for a worker.
    Ready,
    /// A worker is executing this step under a lease.
    Running,
    /// Every forward operation committed.
    Succeeded,
    /// All committed operations were compensated.
    Compensated,
    /// The external outcome needs evidence before the workflow can continue.
    Reconcile,
}
/// Inspection snapshot used for operator reconciliation.
#[derive(Debug)]
pub struct Status {
    /// Current state.
    pub state: State,
    /// Stable external idempotency key for this exact operation.
    pub key: String,
    /// Current service operation, when one can run automatically.
    pub operation: Option<String>,
    /// Current action is a compensation.
    pub compensating: bool,
    /// A caller requested cancellation before the terminal state.
    pub cancellation_requested: bool,
    /// Fences a reconciliation decision against later work.
    pub revision: i64,
    /// Last recorded public diagnostic.
    pub problem: Option<String>,
}
/// Work delivered to a service. Repeated delivery preserves the same key.
#[derive(Clone, Debug)]
pub struct Work {
    /// Saga identifier supplied at creation.
    pub saga: Ulid,
    /// Idempotency key for this operation at the external service.
    pub key: String,
    /// Input supplied at creation.
    pub input: Value,
    /// Whether the worker is compensating an earlier committed operation.
    pub compensating: bool,
}
/// Exclusive, time-limited ownership of a step.
pub struct Lease {
    work: Work,
    token: String,
    duration: Duration,
}
impl Lease {
    /// Input and stable operation key to send to the service.
    pub fn work(&self) -> &Work {
        &self.work
    }
}
/// Result of a service call. Only a known rejection triggers automatic undo.
pub enum Outcome {
    /// The requested forward or compensating action committed.
    Applied,
    /// The service confirmed the action did not commit.
    Rejected(String),
    /// A transient failure. The next invocation uses the same key.
    Retry(String),
    /// The service's commit status is unknown.
    Unknown(String),
}
/// Outcome confirmed by looking up a stable step key at its service.
pub enum Resolution {
    /// The action committed.
    Applied,
    /// The action did not commit.
    NotApplied,
}
/// The saga coordinator shares the queue's database and clock.
#[derive(Clone)]
pub struct Sagas(JobQueue);
impl JobQueue {
    /// Operates durable compensating workflows in this database.
    pub fn sagas(&self) -> Sagas {
        Sagas(self.clone())
    }
}

async fn row(db: impl Target, id: Ulid) -> HyveResult<SagaRow> {
    query::<SagaRow>(SELECT_SAGA, vec![bind(&id)])
        .fetch_one(db)
        .await
}
impl SagaRow {
    fn status(&self) -> HyveResult<Status> {
        let state = match self.state.as_str() {
            "ready" => State::Ready,
            "running" => State::Running,
            "done" => State::Succeeded,
            "compensated" => State::Compensated,
            "reconcile" => State::Reconcile,
            _ => return Err(HyveError::internal("invalid saga state")),
        };
        Ok(Status {
            state,
            key: format!("{}/{}/{}", self.id, self.step, self.direction),
            operation: self.operation.clone(),
            compensating: self.direction == "undo",
            revision: self.revision,
            cancellation_requested: self.cancelled != 0,
            problem: self.problem.clone(),
        })
    }
}

impl Sagas {
    /// Durably starts a workflow. Repeating the same ID and definition is idempotent.
    pub async fn start(
        &self,
        id: Ulid,
        steps: Vec<Step>,
        input: Value,
        attempts: u32,
    ) -> HyveResult<()> {
        if steps.is_empty()
            || attempts == 0
            || steps
                .iter()
                .any(|s| s.apply.is_empty() || s.undo.as_ref().is_some_and(String::is_empty))
        {
            return Err(HyveError::invalid_request(
                "a saga needs named steps and a positive attempt limit",
            ));
        }
        let now = self.0.now().await?;
        let plan = to_json(&steps);
        let input = to_json(&input);
        self.0
            .db
            .tx(|tx| async move {
                let inserted = query::<()>(
                    INSERT_SAGA,
                    vec![
                        bind(&id),
                        bind(&plan),
                        bind(&input),
                        bind(&i64::from(attempts)),
                        bind(&steps[0].apply),
                        bind(&now),
                    ],
                )
                .execute(&tx)
                .await?;
                if inserted == 0 {
                    let existing = row(&tx, id).await?;
                    if existing.plan != plan
                        || existing.input != input
                        || existing.maximum != i64::from(attempts)
                    {
                        return Err(HyveError::domain(
                            "saga_conflict",
                            "saga ID already has a different definition or input",
                        ));
                    }
                }
                Ok(())
            })
            .await
    }

    /// Inspects the current operation and notices a worker lost on its final attempt.
    pub async fn status(&self, id: Ulid) -> HyveResult<Status> {
        let now = self.0.now().await?;
        query::<()>(EXPIRE_SAGA, vec![bind(&id), bind(&now)])
            .execute(&self.0.db)
            .await?;
        row(&self.0.db, id).await?.status()
    }
    /// Claims one step for this service, including recoverable expired claims.
    pub async fn claim(&self, operation: &str, duration: Duration) -> HyveResult<Option<Lease>> {
        if duration.is_zero() {
            return Err(HyveError::invalid_request("a saga lease must be positive"));
        }
        let now = self.0.now().await?;
        let until = add(now, millis(duration)?)?;
        let token = Ulid::new().to_string();
        query::<()>(EXPIRE_OPERATION, vec![bind(operation), bind(&now)])
            .execute(&self.0.db)
            .await?;
        let claimed = query::<SagaRow>(
            CLAIM_OPERATION,
            vec![bind(&token), bind(&until), bind(operation), bind(&now)],
        )
        .fetch_optional(&self.0.db)
        .await?;
        claimed
            .map(|r| {
                let status = r.status()?;
                let work = Work {
                    saga: r
                        .id
                        .parse()
                        .map_err(|_| HyveError::internal("invalid saga ID"))?,
                    input: from_json(&r.input)
                        .map_err(|_| HyveError::internal("invalid saga input"))?,
                    key: status.key,
                    compensating: status.compensating,
                };
                Ok(Lease {
                    work,
                    token,
                    duration,
                })
            })
            .transpose()
    }
    /// Reports an outcome while this worker still owns the live step.
    pub async fn finish(&self, claim: &Lease, outcome: Outcome) -> HyveResult<()> {
        let now = self.0.now().await?;
        let id = claim.work.saga;
        self.0
            .db
            .tx(|tx| async move {
                owned(
                    query::<()>(
                        RELEASE_OWNED,
                        vec![bind(&id), bind(&claim.token), bind(&now)],
                    )
                    .execute(&tx)
                    .await?,
                )?;
                let saga = row(&tx, id).await?;
                advance(&tx, &saga, outcome, now).await
            })
            .await
    }
    /// Extends a live worker's ownership.
    pub async fn renew(&self, claim: &Lease) -> HyveResult<()> {
        let now = self.0.now().await?;
        let until = add(now, millis(claim.duration)?)?;
        owned(
            query::<()>(
                RENEW_LEASE,
                vec![
                    bind(&until),
                    bind(&claim.work.saga),
                    bind(&claim.token),
                    bind(&now),
                ],
            )
            .execute(&self.0.db)
            .await?,
        )
    }
    /// Applies evidence for the exact reconciliation revision inspected by the caller.
    pub async fn resolve(&self, id: Ulid, revision: i64, decision: Resolution) -> HyveResult<()> {
        let now = self.0.now().await?;
        self.0
            .db
            .tx(|tx| async move {
                let locked = query::<()>(CLAIM_RECONCILIATION, vec![bind(&id), bind(&revision)])
                    .execute(&tx)
                    .await?;
                if locked != 1 {
                    return Err(HyveError::domain(
                        "saga_conflict",
                        "reconciliation revision changed",
                    ));
                }
                let saga = row(&tx, id).await?;
                match decision {
                    Resolution::Applied => advance(&tx, &saga, Outcome::Applied, now).await,
                    Resolution::NotApplied
                        if saga.direction == "undo" && saga.operation.is_some() =>
                    {
                        query::<()>(RETRY_COMPENSATION, vec![bind(&now), bind(&id)])
                            .execute(&tx)
                            .await?;
                        Ok(())
                    }
                    Resolution::NotApplied => {
                        advance(
                            &tx,
                            &saga,
                            Outcome::Rejected("confirmed not applied".into()),
                            now,
                        )
                        .await
                    }
                }
            })
            .await
    }
    /// Requests reverse compensation. An in-flight forward call first needs a service status lookup.
    pub async fn cancel(&self, id: Ulid) -> HyveResult<()> {
        let now = self.0.now().await?;
        self.0
            .db
            .tx(|tx| async move {
                let n = query::<()>(CANCEL_FORWARD, vec![bind(&id)])
                    .execute(&tx)
                    .await?;
                if n == 0 {
                    return Ok(());
                }
                let r = row(&tx, id).await?;
                if r.state == "reconcile" {
                    return Ok(());
                }
                let outcome = if r.state == "running" || r.attempt > 0 {
                    Outcome::Unknown("cancelled after dispatch".into())
                } else {
                    Outcome::Rejected("cancelled before dispatch".into())
                };
                advance(&tx, &r, outcome, now).await
            })
            .await
    }
    /// Runs one service operation, renews its lease and records an uncertain result on cancellation.
    pub async fn run_once<F: Future<Output = Outcome>>(
        &self,
        operation: &str,
        lease: Duration,
        scope: &Scope,
        handler: impl FnOnce(Work) -> F,
    ) -> HyveResult<bool> {
        if scope.cancellation().is_cancelled() {
            return Ok(false);
        }
        let Some(claim) = self.claim(operation, lease).await? else {
            return Ok(false);
        };
        let work = scope.scoped(handler(claim.work.clone()));
        tokio::pin!(work);
        let heartbeat = (lease / 3).max(Duration::from_millis(1));
        let outcome = loop {
            tokio::select! {
                result=&mut work=>break result,
                ()=scope.cancellation().cancelled()=>break Outcome::Unknown("worker cancelled".into()),
                ()=tokio::time::sleep(heartbeat)=>self.renew(&claim).await?,
            }
        };
        self.finish(&claim, outcome).await?;
        Ok(true)
    }
}

async fn advance(tx: &Tx, r: &SagaRow, outcome: Outcome, now: i64) -> HyveResult<()> {
    let steps: Vec<Step> =
        from_json(&r.plan).map_err(|_| HyveError::internal("invalid stored saga plan"))?;
    let mut cursor = r.step;
    let mut undo = r.direction == "undo";
    let mut state = "ready";
    let mut due = now;
    let mut attempt = 0;
    let mut problem = r.problem.clone();
    match outcome {
        Outcome::Applied if !undo && r.cancelled != 0 => undo = true,
        Outcome::Applied if !undo => {
            cursor += 1;
            if cursor == steps.len() as i64 {
                state = "done";
            }
        }
        Outcome::Applied => cursor -= 1,
        Outcome::Rejected(reason) => {
            problem = Some(reason);
            if undo {
                state = "reconcile";
            } else {
                undo = true;
                cursor -= 1;
            }
        }
        Outcome::Unknown(reason) => {
            state = "reconcile";
            problem = Some(reason);
        }
        Outcome::Retry(reason) => {
            problem = Some(reason);
            attempt = r.attempt;
            if attempt >= r.maximum {
                state = "reconcile";
            } else {
                due = add(now, (1000_i64 << (attempt - 1).clamp(0, 12)).min(3_600_000))?;
            }
        }
    }
    if cursor < 0 {
        state = "compensated";
    }
    let operation = steps
        .get(usize::try_from(cursor).unwrap_or(usize::MAX))
        .and_then(|s| {
            if undo {
                s.undo.as_deref()
            } else {
                Some(&s.apply)
            }
        });
    if state == "ready" && operation.is_none() {
        state = "reconcile";
        problem = Some("committed step has no automatic compensation".into());
    }
    query::<()>(
        SAVE_TRANSITION,
        vec![
            bind(&cursor),
            bind(if undo { "undo" } else { "apply" }),
            bind(state),
            bind(&operation.map(str::to_owned)),
            bind(&attempt),
            bind(&due),
            bind(&problem),
            bind(&r.id),
        ],
    )
    .execute(tx)
    .await?;
    Ok(())
}
