use super::*;
use std::task::{Context, Poll, Waker};

fn poll<F: Future>(future: std::pin::Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

#[tokio::test(start_paused = true)]
async fn overlapping_agent_waits_preserve_the_remaining_service_budget() {
    let clock = ServiceClock::default();
    let command = clock.timeout(Duration::from_secs(20), std::future::pending::<()>());
    tokio::pin!(command);
    assert!(poll(command.as_mut()).is_pending());
    tokio::time::advance(Duration::from_secs(7)).await;
    let first = clock.park();
    let second = clock.clone().park();
    tokio::time::advance(Duration::from_secs(60)).await;
    drop(first);
    tokio::time::advance(Duration::from_secs(60)).await;
    assert!(poll(command.as_mut()).is_pending());
    drop(second);
    tokio::time::advance(Duration::from_secs(12)).await;
    assert!(poll(command.as_mut()).is_pending());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(poll(command.as_mut()), Poll::Ready(Err(ServiceTimeout)));
}

#[tokio::test(start_paused = true)]
async fn a_hold_between_polls_is_not_charged_to_the_command() {
    let clock = ServiceClock::default();
    let command = clock.timeout(Duration::from_secs(20), std::future::pending::<()>());
    tokio::pin!(command);
    assert!(poll(command.as_mut()).is_pending());
    tokio::time::advance(Duration::from_secs(3)).await;
    let hold = clock.park();
    tokio::time::advance(Duration::from_secs(120)).await;
    drop(hold);
    // Both notifications have coalesced before this future next polls.
    assert!(poll(command.as_mut()).is_pending());
    tokio::time::advance(Duration::from_secs(16)).await;
    assert!(poll(command.as_mut()).is_pending());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(poll(command.as_mut()), Poll::Ready(Err(ServiceTimeout)));
}

#[tokio::test(start_paused = true)]
async fn a_command_can_start_and_complete_while_service_time_is_parked() {
    let clock = ServiceClock::default();
    let _hold = clock.park();
    let (answer, receive) = tokio::sync::oneshot::channel();
    let command = clock.timeout(Duration::from_secs(20), receive);
    tokio::pin!(command);
    assert!(poll(command.as_mut()).is_pending());
    tokio::time::advance(Duration::from_secs(120)).await;
    assert!(poll(command.as_mut()).is_pending());
    answer.send(42).unwrap();
    assert_eq!(poll(command.as_mut()), Poll::Ready(Ok(Ok(42))));
    assert_eq!(clock.timeout(Duration::ZERO, std::future::ready(7)).await, Ok(7));
}
