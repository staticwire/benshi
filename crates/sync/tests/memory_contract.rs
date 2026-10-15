//! The backend in memory against the shared contract.

mod contract;
mod fakes;

#[tokio::test(start_paused = true)]
async fn the_backend_in_memory_satisfies_the_contract() {
    contract::verify(fakes::subject(|memory, _| memory)).await;
}
