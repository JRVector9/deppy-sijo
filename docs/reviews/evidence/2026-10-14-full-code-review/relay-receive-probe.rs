
#![allow(dead_code)]
mod relay_client {
  #[path="/Users/jr/Desktop/projects/deppy-sijo-performance/crates/web-remote/src/relay_client/lifecycle.rs"] pub mod lifecycle;
  #[path="/Users/jr/Desktop/projects/deppy-sijo-performance/crates/web-remote/src/relay_client/worker.rs"] pub mod worker;
  #[path="/private/tmp/deppy-all-review-remote-data-20261014/tls-production-copy.rs"] pub mod tls;
}
fn main() { relay_client::tls::review_receive_probe(); }
