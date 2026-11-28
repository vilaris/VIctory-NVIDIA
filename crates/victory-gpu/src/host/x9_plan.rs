//! Read-only planning of detached lists. The queue still owns the ordering
//! of title waits and submission. A plan contains indices into one list's
//! immutable recording and never reads title memory or changes host state.

use super::{Deferred, Step};
use std::sync::{Arc, LockResult, Mutex, MutexGuard, OnceLock, Weak, mpsc};

type Shared = Arc<Mutex<Deferred>>;
type Ticket = Arc<OnceLock<Plan>>;

pub(super) enum Recording {
    Inline(Mutex<Deferred>),
    Shared(Shared),
}

impl Recording {
    pub(super) fn new() -> Self {
        if crate::rf_x9::enabled() {
            Self::Shared(Arc::default())
        } else {
            Self::Inline(Mutex::default())
        }
    }

    pub(super) fn lock(&self) -> LockResult<MutexGuard<'_, Deferred>> {
        match self {
            Self::Inline(recording) => recording.lock(),
            Self::Shared(recording) => recording.lock(),
        }
    }

    pub(super) fn plan(&self) {
        let Self::Shared(recording) = self else { return };
        let mut deferred = recording.lock().unwrap_or_else(|e| e.into_inner());
        // For small lists the inline walk is cheaper than publishing a job.
        if deferred.x9_plan.is_some() || deferred.copies.len() < 128 {
            return;
        }
        let ticket = Ticket::default();
        deferred.x9_plan = Some(ticket.clone());
        drop(deferred);
        // A full queue keeps the ordinary inline planner. Never stall a
        // title submit while doing speculative planning.
        if let Some(worker) = worker() {
            let _ = worker.try_send(Job { recording: Arc::downgrade(recording), ticket });
        }
    }
}

struct Job {
    recording: Weak<Mutex<Deferred>>,
    ticket: Ticket,
}

// SAFETY: the queue lends a detached list's recording, whose host objects
// travel on the queue and relay threads already. The worker only inspects
// step tags under the same lock used by the consumer and reset.
unsafe impl Send for Job {}

impl Job {
    fn run(self) {
        let Some(recording) = self.recording.upgrade() else { return };
        let Ok(deferred) = recording.try_lock() else { return };
        // The list may already have landed, reset and been recycled. Hold
        // the lock while copying its step indices, and bind the result to
        // this recording's ticket even if it resets after the lock drops.
        if !deferred.x9_plan.as_ref().is_some_and(|current| Arc::ptr_eq(current, &self.ticket)) {
            return;
        }
        let _timer = crate::rf_x9::timer(crate::rf_x9::Work::Planning);
        let plan = Plan::build(&deferred.copies);
        drop(deferred);
        let _ = self.ticket.set(plan);
    }
}

fn worker() -> Option<&'static mpsc::SyncSender<Job>> {
    static WORKER: OnceLock<Option<mpsc::SyncSender<Job>>> = OnceLock::new();
    WORKER.get_or_init(|| {
        let (sender, receiver) = mpsc::sync_channel::<Job>(128);
        std::thread::Builder::new().name("victory-x9-plan".into()).spawn(move || {
            crate::cores::keep_on_fast_cores("victory-x9-plan");
            for job in receiver {
                job.run();
            }
        }).ok()?;
        Some(sender)
    }).as_ref()
}

pub(super) struct Plan {
    coherence: Vec<usize>,
    writes: Vec<usize>,
}

impl Plan {
    fn build(steps: &[Step]) -> Self {
        let mut plan = Self { coherence: Vec::new(), writes: Vec::new() };
        for (index, step) in steps.iter().enumerate() {
            match step {
                Step::GpuWrite(..) => plan.writes.push(index),
                Step::Refresh(..) | Step::Texture(_) | Step::Copy(..) | Step::Placed(_) | Step::Run(_) => {}
                Step::Fill(..) | Step::Host(..) | Step::Plane(..) => continue,
            }
            plan.coherence.push(index);
        }
        plan
    }
}

impl Deferred {
    pub(super) fn x9_steps(&self, writes: bool) -> impl Iterator<Item = &Step> {
        let plan = self.x9_plan.as_ref().and_then(|ticket| ticket.get());
        let selected = plan.map_or(&[][..], |plan| {
            if writes { &plan.writes[..] } else { &plan.coherence[..] }
        });
        let inline = if plan.is_some() { &[] } else { &self.copies[..] };
        inline.iter().chain(selected.iter().map(|&index| &self.copies[index]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planning_keeps_cpu_write_boundaries_in_recorded_order() {
        let copies = vec![
            Step::Copy(16, 32, 4),
            Step::Fill(0, 16, 4, None, false),
            Step::Run(Box::new(|| panic!("steps are only inspected"))),
            Step::Copy(32, 16, 4),
        ];
        let plan = Plan::build(&copies);
        assert_eq!(plan.coherence, [0, 2, 3]);
        assert!(plan.writes.is_empty());
    }

    #[test]
    fn a_ticket_from_a_recycled_recording_cannot_publish_a_plan() {
        let recording = Shared::default();
        let ticket = Ticket::default();
        let new_ticket = Ticket::default();
        recording.lock().unwrap().x9_plan = Some(new_ticket.clone());
        let job = Job { recording: Arc::downgrade(&recording), ticket: ticket.clone() };
        job.run();
        assert!(ticket.get().is_none());
        assert!(new_ticket.get().is_none());
    }
}
