//! The client: speaks to a running daemon over a local socket.
//!
//! Every command exists to make a decision inspectable. `benshi sources` lists
//! each known source with its policy, state and capabilities, which retires the
//! whole class of "why does it not see my player". `benshi why` prints which
//! recognition stage fired and with what score. `benshi watch` subscribes to
//! the event bus. `benshi record` captures snapshots to a file that tests
//! replay as a fixture.
