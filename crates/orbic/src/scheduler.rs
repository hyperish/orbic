use std::{future::Future, io, time::Duration};

use hyve::{HyveResult, Scope, Value};
use orbic_core::ScheduledTask;
use orbic_jobs::{JobQueue, Outcome};
use tokio::task::JoinSet;

const POLL: Duration = Duration::from_millis(100);
const LEASE: Duration = Duration::from_secs(30);

pub(crate) async fn serve(
    http: crate::HttpApp,
    listener: impl crate::ServerListener,
    shutdown: impl Future<Output = ()>,
) -> io::Result<()> {
    let mut timers = Vec::new();
    let mut scopes = Vec::new();
    for task in http.pipeline().schedules() {
        let queue = task.scope.require::<JobQueue>().map_err(io::Error::other)?;
        queue
            .tick(&task.name, &task.timing, &task.name, Value::Null, 3)
            .await
            .map_err(io::Error::other)?;
        let scope = task.scope.child();
        scopes.push(scope.clone());
        timers.push((task.clone(), (*queue).clone(), scope));
    }
    let mut workers = JoinSet::new();
    for (task, queue, scope) in timers {
        workers.spawn(worker(task, queue, scope));
    }
    let stop = || {
        for scope in &scopes {
            scope.cancellation().cancel();
        }
    };
    let result = http
        .serve(listener, async {
            shutdown.await;
            stop();
        })
        .await;
    stop();
    while let Some(worker) = workers.join_next().await {
        worker.map_err(io::Error::other)?;
    }
    result
}

async fn worker(task: ScheduledTask, queue: JobQueue, scope: Scope) {
    while !scope.cancellation().is_cancelled() {
        let result: HyveResult<bool> = async {
            queue
                .tick(&task.name, &task.timing, &task.name, Value::Null, 3)
                .await?;
            queue
                .run_once(&task.name, LEASE, &scope, |job| {
                    let run =
                        hyve::trace::Span::request(&scope, hyve::RequestId::new(), &task.name);
                    run.scope().provide(job);
                    let task = &task;
                    async move {
                        match run.scope().scoped(task.run()).await {
                            Ok(()) => Outcome::Done,
                            Err(error) if error.is_retryable() => {
                                Outcome::Retry(error.to_public().code().to_owned())
                            }
                            Err(error) => Outcome::Reconcile(error.to_public().code().to_owned()),
                        }
                    }
                })
                .await
        }
        .await;
        let delay = match result {
            Ok(true) => continue,
            Ok(false) => POLL,
            Err(e) => {
                eprintln!("orbic: schedule {} failed: {}", task.name, e.code());
                Duration::from_secs(1)
            }
        };
        tokio::select! {
            () = scope.cancellation().cancelled() => break,
            () = tokio::time::sleep(delay) => {}
        }
    }
    scope.dispose();
}
