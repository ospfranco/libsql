use std::future::ready;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossbeam::channel::Sender;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use rayon::{ThreadPool, ThreadPoolBuilder};
use tokio::sync::oneshot;

use crate::job::Job;
use crate::messages::{ErrorCode, Message};
use crate::scheduler::UpdateStateMessage;
use crate::statements::{State, Statements};

const TXN_TIMEOUT_SECS: usize = 5;

pub struct WorkerPool {
    _pool: ThreadPool,
    worker_sigs: FuturesUnordered<oneshot::Receiver<()>>,
}

impl WorkerPool {
    /// Create a new coordinator that will spawn `ncpu` threads.
    /// Each worker maintains a connections to the database, and process jobs sequentially.
    /// `conn_builder` must create a fresh db_connection each time it is called.
    /// If ncpu is 0, then the number of worker is determined automatically.
    pub fn new(
        ncpu: usize,
        conn_builder: impl Fn() -> sqlite::Connection + Sync + Send,
    ) -> Result<(Self, Sender<Job>)> {
        let _pool = ThreadPoolBuilder::new().num_threads(ncpu).build()?;
        let (fifo, receiver) = crossbeam::channel::unbounded();

        let worker_sigs = FuturesUnordered::new();
        _pool.install(|| {
            for id in 0.._pool.current_num_threads() {
                let db_conn = conn_builder();
                let global_fifo = receiver.clone();
                let (_close_sig, reveiver) = oneshot::channel();
                worker_sigs.push(reveiver);
                rayon::spawn(move || {
                    let worker = Worker {
                        global_fifo,
                        db_conn,
                        id,
                        _close_sig,
                    };

                    worker.run();
                })
            }
        });

        let this = Self { _pool, worker_sigs };
        Ok((this, fifo))
    }

    /// waits for all workers to finish their work and exit.
    pub async fn join(self) {
        self.worker_sigs.for_each(|_| ready(())).await;
    }
}

struct Worker {
    global_fifo: crossbeam::channel::Receiver<Job>,
    db_conn: sqlite::Connection,
    id: usize,
    /// signal that the worker has exited
    /// nothing is done with this, it simply gets dropped with the worker.
    _close_sig: oneshot::Sender<()>,
}

impl Worker {
    fn perform_oneshot(&self, stmts: &Statements) -> Message {
        let mut rows = vec![];
        let result = self.db_conn.iterate(&stmts.stmts, |pairs| {
            for &(name, value) in pairs.iter() {
                rows.push(format!("{} = {}", name, value.unwrap()));
            }
            true
        });

        match result {
            Ok(_) => Message::ResultSet(rows),
            Err(err) => Message::Error(ErrorCode::SQLError, format!("{:?}", err)),
        }
    }

    fn handle_transaction(&self, job: Job) {
        let (sender, receiver) = crossbeam::channel::unbounded();
        job.scheduler_sender
            .send(UpdateStateMessage::TxnBegin(job.endpoint, sender))
            .unwrap();
        let mut stmts = job.statements;

        let txn_timeout = Instant::now() + Duration::from_secs(TXN_TIMEOUT_SECS as _);

        loop {
            let message = self.perform_oneshot(&stmts);
            job.responder.respond(&message);
            match stmts.state(State::TxnOpened) {
                State::TxnClosed if !message.is_err() => {
                    // the transaction was closed successfully
                    job.scheduler_sender
                        .send(UpdateStateMessage::TxnEnded(job.endpoint))
                        .unwrap();
                    break;
                }
                _ => {
                    // Let the database handle any other state
                    job.scheduler_sender
                        .send(UpdateStateMessage::Ready(job.endpoint))
                        .unwrap();
                    match receiver.recv_timeout(txn_timeout - Instant::now()) {
                        Ok(job) => {
                            stmts = job.statements;
                        }
                        Err(_) => {
                            log::warn!("rolling back transaction!");
                            let _ = self.db_conn.execute("ROLLBACK TRANSACTION;");
                            job.responder.respond(&Message::Error(
                                ErrorCode::TxTimeout,
                                "transaction timed out".into(),
                            ));
                            break;
                        }
                    }
                }
            }
        }
    }

    fn run(self) {
        while let Ok(job) = self.global_fifo.recv() {
            log::debug!("executing job `{:?}` on worker {}", job.statements, self.id);

            // This is an interactive transaction.
            if let State::TxnOpened = job.statements.state(State::Start) {
                self.handle_transaction(job)
            } else {
                // Any other state falls in this branch, even invalid: we let sqlite deal with the
                // error handling.
                let m = self.perform_oneshot(&job.statements);
                job.responder.respond(&m);
                job.scheduler_sender
                    .send(UpdateStateMessage::Ready(job.endpoint))
                    .unwrap();
            }

            log::debug!("job finished on worker {}", self.id);
        }
    }
}
