//! Requests a `hold` rule keeps while the page's main thread waits for them (engine patch 53). The page
//! cannot run, so the operation that was running cannot finish until the agent answers. The answer comes
//! through `BlockingHolds`, which needs no access to the generation; the generation's waiting loop
//! applies it with the browser-side commands a held request is answered with.
use super::*;

/// What the agent sees of a held request the page waits for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockingHold {
    pub ordinal: String,
    pub method: String,
    pub url: String,
    pub rule: RuleId,
    pub rules_revision: u64,
    /// When the page asked, which is where its clock stands while it waits.
    pub t_us: VirtualTime,
}

/// The agent's answer to a `BlockingHold`.
#[derive(Debug, Clone)]
pub enum BlockingAnswer {
    Respond(MockSpec),
    Fail(FailReason),
}

pub(super) struct Answering {
    ordinal: String,
    answer: BlockingAnswer,
    done: tokio::sync::oneshot::Sender<Result<(), ChromiumError>>,
}

/// The held request the page waits for, if any, and the way to answer it.
#[derive(Clone)]
pub struct BlockingHolds {
    pending: std::sync::Arc<tokio::sync::watch::Sender<Option<BlockingHold>>>,
    answers: tokio::sync::mpsc::UnboundedSender<Answering>,
}

impl BlockingHolds {
    pub(super) fn new() -> (BlockingHolds, tokio::sync::mpsc::UnboundedReceiver<Answering>) {
        let (answers, received) = tokio::sync::mpsc::unbounded_channel();
        (BlockingHolds { pending: std::sync::Arc::new(tokio::sync::watch::Sender::new(None)), answers }, received)
    }

    pub fn pending(&self) -> Option<BlockingHold> {
        self.pending.borrow().clone()
    }

    /// Waits until a held request makes the page wait.
    pub async fn wait_pending(&self) -> BlockingHold {
        let mut updates = self.pending.subscribe();
        loop {
            if let Some(hold) = updates.borrow_and_update().clone() {
                return hold;
            }
            if updates.changed().await.is_err() {
                return std::future::pending().await;
            }
        }
    }

    /// Answers the held request the page waits for; applied by the operation that is waiting.
    pub async fn answer(&self, ordinal: &str, answer: BlockingAnswer) -> Result<(), ChromiumError> {
        if self.pending().is_none_or(|h| h.ordinal != ordinal) {
            return Err(ChromiumError::NoSuchHold(ordinal.into()));
        }
        let (done, applied) = tokio::sync::oneshot::channel();
        self.answers.send(Answering { ordinal: ordinal.into(), answer, done })
            .map_err(|_| ChromiumError::Shape("the generation no longer runs".into()))?;
        applied.await.map_err(|_| ChromiumError::Shape("the waiting operation ended before the answer applied".into()))?
    }

    pub(super) fn set(&self, hold: Option<BlockingHold>) {
        self.pending.send_replace(hold);
    }
}

impl Generation {
    /// The held request the page waits for, and the way to answer it while an operation runs.
    pub fn blocking_holds(&self) -> BlockingHolds {
        self.subs.holds.clone()
    }

    /// Applies an agent's answer to the held request the page waits for.
    pub(super) async fn answer_blocking(&mut self, a: Answering) {
        let applied = if self.subs.holds.pending().is_some_and(|h| h.ordinal == a.ordinal) {
            // Boxed: answering can wait on further page work, which may serve another such request.
            let answered = match &a.answer {
                BlockingAnswer::Respond(spec) => Box::pin(self.answer_held(&a.ordinal, Answer::Fulfill(spec, ResponseSource::Agent))).await,
                BlockingAnswer::Fail(reason) => Box::pin(self.answer_held(&a.ordinal, Answer::Fail(*reason))).await,
            };
            if answered.is_ok() {
                self.subs.holds.set(None);
            }
            answered
        } else {
            Err(ChromiumError::NoSuchHold(a.ordinal.clone()))
        };
        let _ = a.done.send(applied);
    }
}
